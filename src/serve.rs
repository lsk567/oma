//! Long-lived HTTP admission surface for OMAR programs.
//!
//! `omar run` binds a per-run diagram server that dies with the run, so it can
//! never be the place programs arrive. This daemon outlives individual runs: it
//! accepts source, compiles it, supervises the run on a worker thread, and
//! reports the per-run diagram address back to the caller.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::config::Config;
use crate::ea::EaId;
use crate::tmux::{DeliveryOptions, TmuxClient};
use crate::topology::{self, PortKind, TopologyRunConfig, VmState};

pub const SERVE_PROTOCOL_VERSION: u32 = 1;

/// The diagram server binds early in `run_topology`, so this only covers
/// compilation and process start-up.
const DIAGRAM_READY_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BODY_BYTES: usize = 512 * 1024;
const TERMINAL_PREFIX: &str = "/v1/agents/";
const TERMINAL_SUFFIX: &str = "/terminal";
/// How long the relay waits on each side before checking the other.
const TERMINAL_POLL: Duration = Duration::from_millis(25);
/// A selection names diagram components, and a diagram the operator can click
/// through is far smaller than this. The cap is here because the names are
/// echoed into the EA's pane, not because a real selection approaches it.
const MAX_SELECTION: usize = 64;
const MAX_SELECTION_NAME: usize = 128;
/// Messages a stalled chat subscriber may fall behind by before it is dropped.
const CHAT_QUEUE: usize = 256;

#[derive(Debug, Clone, Serialize)]
pub struct RunRecord {
    pub run_id: String,
    pub team: String,
    pub status: String,
    pub diagram_address: Option<String>,
    pub started_at: u64,
    pub finished_at: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StartRunRequest {
    program: String,
    #[serde(default)]
    inputs: BTreeMap<String, Value>,
    /// A daemon re-runs the same team repeatedly, so stale agent sessions are
    /// replaced rather than treated as a conflict.
    #[serde(default = "default_replace")]
    replace: bool,
    #[serde(default = "default_timeout_seconds")]
    timeout_seconds: u64,
}

fn default_replace() -> bool {
    true
}

fn default_timeout_seconds() -> u64 {
    300
}

type Runs = Arc<Mutex<BTreeMap<String, RunRecord>>>;

/// One entry in the operator/EA conversation. `design` is set only on
/// proposals, and carries a program the operator has *not* yet approved.
#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub sequence: u64,
    pub role: String,
    pub text: String,
    /// Commentary while working, rather than something awaiting an answer.
    pub progress: bool,
    pub design: Option<ProposedDesign>,
    /// Diagram components the operator had selected when they sent this.
    /// "this one" is unresolvable in text; a selection says which.
    #[serde(default)]
    pub selection: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposedDesign {
    pub program: String,
    #[serde(default)]
    pub inputs: BTreeMap<String, Value>,
    /// The compiled topology, so the operator sees what they are approving
    /// before any run exists.
    pub preview: crate::diagram::DiagramSnapshot,
}

#[derive(Debug, Deserialize)]
struct AgentReply {
    token: String,
    text: String,
    #[serde(default)]
    progress: bool,
}

#[derive(Debug, Deserialize)]
struct AgentProposal {
    token: String,
    program: String,
    summary: String,
    #[serde(default)]
    inputs: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct ChatRequest {
    text: String,
    #[serde(default)]
    selection: Vec<String>,
}

#[derive(Default)]
struct Chat {
    messages: Vec<ChatMessage>,
    subscribers: Vec<mpsc::SyncSender<ChatMessage>>,
    sequence: u64,
}

struct Context_ {
    omar_dir: PathBuf,
    ea_id: EaId,
    session_prefix: String,
    default_workdir: String,
    health_idle_warning: i64,
    runs: Runs,
    chat: Arc<Mutex<Chat>>,
    /// Authenticates the EA's MCP sidecar on the agent-only endpoints.
    agent_token: String,
    /// The command the assistant runs, and where to reach this server when it
    /// is relaunched on a different backend.
    command: Arc<Mutex<String>>,
    address: SocketAddr,
}

impl Context_ {
    fn publish(
        &self,
        role: &str,
        text: String,
        progress: bool,
        design: Option<ProposedDesign>,
    ) -> ChatMessage {
        self.publish_with_selection(role, text, progress, design, Vec::new())
    }

    fn publish_with_selection(
        &self,
        role: &str,
        text: String,
        progress: bool,
        design: Option<ProposedDesign>,
        selection: Vec<String>,
    ) -> ChatMessage {
        let mut chat = self.chat.lock().expect("serve chat poisoned");
        chat.sequence += 1;
        let message = ChatMessage {
            sequence: chat.sequence,
            role: role.to_string(),
            text,
            progress,
            design,
            selection,
        };
        chat.messages.push(message.clone());
        chat.subscribers
            .retain(|subscriber| subscriber.try_send(message.clone()).is_ok());
        message
    }
}

pub enum AttachEa {
    Attached(String),
    /// Running, but launched without a serve context, so it has no way to reply
    /// or propose designs.
    AlreadyRunningWithoutServe(String),
    /// Launched, but the context its MCP sidecar will read does not carry this
    /// server. The EA will follow instructions to use `omar_reply` and find no
    /// such tool, answering into a terminal nobody reads.
    LaunchedWithoutServe {
        session: String,
        reason: String,
    },
}

pub struct Serve {
    address: SocketAddr,
    agent_token: String,
    running: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Serve {
    pub fn start(
        address: SocketAddr,
        config: &Config,
        omar_dir: &Path,
        ea_id: EaId,
    ) -> Result<Self> {
        // This surface executes arbitrary OMAR programs, so it stays loopback
        // only, matching the diagram server it supervises.
        anyhow::ensure!(
            address.ip().is_loopback(),
            "serve must bind to a loopback address"
        );
        let listener = TcpListener::bind(address)
            .with_context(|| format!("failed to bind serve at {address}"))?;
        let address = listener.local_addr()?;
        let agent_token = Uuid::new_v4().to_string();
        let context = Arc::new(Context_ {
            omar_dir: omar_dir.to_path_buf(),
            ea_id,
            session_prefix: config.dashboard.session_prefix.clone(),
            default_workdir: config.agent.default_workdir.clone(),
            health_idle_warning: config.health.idle_warning,
            runs: Runs::default(),
            chat: Arc::new(Mutex::new(Chat::default())),
            agent_token: agent_token.clone(),
            command: Arc::new(Mutex::new(config.agent.default_command.clone())),
            address,
        });
        // Blocking accept rather than a polling loop: `Drop` wakes it with a
        // self-connection, so there is no need to spin, and no added latency on
        // every connection from a poll interval.
        let running = Arc::new(AtomicBool::new(true));
        let thread_running = running.clone();
        let thread = thread::spawn(move || {
            while thread_running.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let context = context.clone();
                        thread::spawn(move || {
                            let _ = handle_client(stream, context);
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            address,
            agent_token,
            running,
            thread: Some(thread),
        })
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Launch the executive assistant with a serve context, so it can converse
    /// with the operator and propose designs.
    ///
    /// The context is baked into the EA's MCP config at launch, so an already
    /// running EA cannot gain these tools without being restarted. Restarting
    /// discards its session, so that is opt-in and reported rather than silent.
    pub fn attach_ea(
        &self,
        config: &Config,
        omar_dir: &Path,
        ea_id: EaId,
        restart: bool,
        launch: bool,
    ) -> Result<AttachEa> {
        let name = crate::ea::load_registry(omar_dir)
            .into_iter()
            .find(|ea| ea.id == ea_id)
            .map(|ea| ea.name)
            .unwrap_or_else(|| format!("ea-{ea_id}"));
        let client = TmuxClient::new(crate::ea::ea_prefix(
            ea_id,
            &config.dashboard.session_prefix,
        ));
        // Write the context even when not launching: it names this server, so a
        // harness can act as the assistant without an agent process existing.
        let context = crate::manager::McpLaunchContext {
            omar_dir: omar_dir.to_path_buf(),
            ea_id,
            session_prefix: config.dashboard.session_prefix.clone(),
            default_command: config.agent.default_command.clone(),
            default_workdir: config.agent.default_workdir.clone(),
            health_idle_warning: config.health.idle_warning,
            tmux_server: None,
            topology: None,
            serve: Some(crate::manager::ServeMcpContext {
                endpoint: self.address.to_string(),
                token: self.agent_token.clone(),
            }),
        };
        crate::manager::materialize_mcp_context_file(&context);
        if !launch {
            return Ok(AttachEa::Attached("(not launched)".to_string()));
        }

        let existing = crate::ea::ea_manager_session(ea_id, &config.dashboard.session_prefix);
        if client.has_session(&existing)? {
            if !restart {
                return Ok(AttachEa::AlreadyRunningWithoutServe(existing));
            }
            // Not `ensure_session_not_attached`: that resolves through the
            // prefix-filtered session list, and the manager session is named
            // `<prefix>ea-<id>`, which never matches the agent prefix. Kill it
            // directly, as `ensure_manager_session` itself does.
            client.kill_session(&existing)?;
        }
        let (session, _) = crate::manager::ensure_manager_session(
            &client,
            &config.agent.default_command,
            ea_id,
            &name,
            omar_dir,
            &config.dashboard.session_prefix,
            &crate::manager::ManagerRuntimeOptions {
                default_workdir: config.agent.default_workdir.clone(),
                health_idle_warning: config.health.idle_warning,
                serve: Some(crate::manager::ServeMcpContext {
                    endpoint: self.address.to_string(),
                    token: self.agent_token.clone(),
                }),
            },
        )?;
        match self.verify_ea_context(omar_dir, ea_id) {
            Ok(()) => Ok(AttachEa::Attached(session)),
            Err(reason) => Ok(AttachEa::LaunchedWithoutServe {
                session,
                reason: reason.to_string(),
            }),
        }
    }

    /// Read back what the EA's MCP sidecar will actually load.
    ///
    /// A stale `omar` on PATH writes a context without this field and exposes
    /// no operator tools, and nothing downstream notices: the EA simply answers
    /// where no one is looking. Catch it at startup instead.
    fn verify_ea_context(&self, omar_dir: &Path, ea_id: EaId) -> Result<()> {
        let path = crate::manager::ea_mcp_context_path(omar_dir, ea_id);
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("no MCP context at {}", path.display()))?;
        let context: Value = serde_json::from_str(&raw).context("MCP context is not valid JSON")?;
        match context.get("serve").and_then(|serve| serve.get("token")) {
            Some(Value::String(token)) if *token == self.agent_token => Ok(()),
            Some(_) => anyhow::bail!("{} carries a different server's token", path.display()),
            None => anyhow::bail!(
                "{} has no serve context; the `omar` that launched the agent is \
                 older than this one",
                path.display()
            ),
        }
    }

    /// Block until the accept loop stops, which for the CLI means forever.
    pub fn wait(mut self) -> Result<()> {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        Ok(())
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn run(
    address: SocketAddr,
    config: &Config,
    omar_dir: &Path,
    ea_id: EaId,
    restart_ea: bool,
    launch_ea: bool,
) -> Result<()> {
    let server = Serve::start(address, config, omar_dir, ea_id)?;
    println!("OMAR serve: http://{}", server.address());
    match server.attach_ea(config, omar_dir, ea_id, restart_ea, launch_ea) {
        Ok(AttachEa::Attached(session)) => println!("Executive assistant: {session}"),
        Ok(AttachEa::AlreadyRunningWithoutServe(session)) => eprintln!(
            "Executive assistant '{session}' is already running and was launched without this \
             server, so it cannot reply or propose designs. Restart it with \
             `omar serve --restart-ea` to enable them."
        ),
        Ok(AttachEa::LaunchedWithoutServe { session, reason }) => eprintln!(
            "Executive assistant '{session}' started, but will NOT see omar_reply or \
             omar_propose_design: {reason}.\nIt answers in its terminal instead, where the \
             operator cannot see it. Reinstall the runtime (`cargo install --path . --force`) \
             so every entry point launches agents with this build, then restart with \
             `omar serve --restart-ea`."
        ),
        Err(error) => eprintln!("Executive assistant unavailable: {error:#}"),
    }
    server.wait()
}

fn handle_client(mut stream: TcpStream, context: Arc<Context_>) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();

    let mut content_length = 0usize;
    let mut origin = None;
    let mut origin_header = None;
    let mut websocket_key = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
        // Loopback binding does not stop a page the operator visits from
        // calling this, so only loopback origins are granted CORS.
        if let Some(value) = lower.strip_prefix("origin:") {
            origin_header = Some(value.trim().to_string());
            origin = crate::diagram::allowed_origin(Some(value));
        }
        // Header names are case-insensitive but the key's base64 is not, so it
        // is taken from the original line rather than the lowercased copy.
        if lower.starts_with("sec-websocket-key:") {
            websocket_key = line
                .split_once(':')
                .map(|(_, value)| value.trim().to_string());
        }
    }
    let origin = origin.as_deref();

    // A terminal is the one endpoint where a wrong answer hands an attacker a
    // shell, and CORS does not apply to WebSockets: the browser opens the
    // socket regardless and only the server can refuse it.
    if method == "GET" && path.starts_with(TERMINAL_PREFIX) && path.ends_with(TERMINAL_SUFFIX) {
        let agent = path
            .trim_start_matches(TERMINAL_PREFIX)
            .trim_end_matches(TERMINAL_SUFFIX)
            .to_string();
        let session = format!(
            "{}{}",
            crate::ea::ea_prefix(context.ea_id, &context.session_prefix),
            crate::tmux::flatten_agent_name(&agent)
        );
        return attach_terminal(
            stream,
            &session,
            websocket_key.as_deref(),
            origin_header.as_deref(),
        );
    }
    // The assistant is not one of the agents: its session is named
    // `<base>ea-<id>` rather than `<base><id>-<name>`, so no agent name
    // reaches it and it needs a route of its own.
    if method == "GET" && path == "/v1/agent/terminal" {
        let session = crate::ea::ea_manager_session(context.ea_id, &context.session_prefix);
        return attach_terminal(
            stream,
            &session,
            websocket_key.as_deref(),
            origin_header.as_deref(),
        );
    }

    if method == "OPTIONS" {
        return write_json(&mut stream, 204, &Value::Null, origin);
    }

    // Streams for as long as the operator keeps Mission Control open.
    if method == "GET" && path == "/v1/chat/events" {
        return stream_chat(stream, &context, origin);
    }

    let mut read_body = |length: usize| -> Result<Vec<u8>> {
        let mut raw = vec![0u8; length];
        reader.read_exact(&mut raw)?;
        Ok(raw)
    };

    let (status, body) = match (method.as_str(), path.as_str()) {
        ("GET", "/health") => (
            200,
            json!({"status": "ok", "protocol_version": SERVE_PROTOCOL_VERSION}),
        ),
        ("POST", "/v1/runs") => {
            if content_length > MAX_BODY_BYTES {
                (413, json!({"error": "program too large"}))
            } else {
                start_run(&context, &read_body(content_length)?)
            }
        }
        ("GET", "/v1/chat") => {
            let chat = context.chat.lock().expect("serve chat poisoned");
            (200, json!({"messages": chat.messages}))
        }
        ("POST", "/v1/chat") => send_to_ea(&context, &read_body(content_length)?),
        ("GET", "/v1/agent") => (200, describe_agent(&context)),
        ("POST", "/v1/agent/backend") => switch_backend(&context, &read_body(content_length)?),
        // EA-only, authenticated with the token handed to its MCP sidecar.
        ("POST", "/v1/agent/reply") => agent_reply(&context, &read_body(content_length)?),
        ("POST", "/v1/agent/proposals") => {
            if content_length > MAX_BODY_BYTES {
                (413, json!({"error": "program too large"}))
            } else {
                agent_proposal(&context, &read_body(content_length)?)
            }
        }
        ("GET", "/v1/runs") => {
            let runs = context.runs.lock().expect("serve runs poisoned");
            (200, json!({"runs": runs.values().collect::<Vec<_>>()}))
        }
        ("GET", rest) if rest.starts_with("/v1/runs/") => {
            let id = rest.trim_start_matches("/v1/runs/");
            let runs = context.runs.lock().expect("serve runs poisoned");
            match runs.get(id) {
                Some(record) => (200, json!(record)),
                None => (404, json!({"error": "unknown run"})),
            }
        }
        _ => (404, json!({"error": "not found"})),
    };
    write_json(&mut stream, status, &body, origin)
}

/// Relay an operator message into the EA's tmux session.
/// Attach a WebSocket to an agent's tmux session.
///
/// Steering an agent means typing into it, which is why this is a socket and
/// not a stream: keystrokes and screen output share one connection, and closing
/// it is the detach.
fn attach_terminal(
    mut stream: TcpStream,
    session: &str,
    websocket_key: Option<&str>,
    origin: Option<&str>,
) -> Result<()> {
    // Same-origin policy does not cover WebSockets. Without this check any page
    // the operator happens to visit could open a terminal into their agents and
    // type into it, loopback binding notwithstanding. A missing Origin is not a
    // browser, which cannot suppress it.
    if let Some(origin) = origin {
        if crate::diagram::allowed_origin(Some(origin)).is_none() {
            return write_json(&mut stream, 403, &json!({"error": "origin refused"}), None);
        }
    }
    let Some(key) = websocket_key else {
        return write_json(
            &mut stream,
            400,
            &json!({"error": "terminal requires a WebSocket upgrade"}),
            None,
        );
    };

    let attachment = match crate::terminal::Attachment::open_session(session) {
        Ok(attachment) => attachment,
        // The socket has not been upgraded yet, so this can still be an
        // ordinary HTTP error the client can read.
        Err(error) => {
            return write_json(
                &mut stream,
                404,
                &json!({"error": format!("{error:#}")}),
                None,
            )
        }
    };

    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n\r\n",
        tungstenite::handshake::derive_accept_key(key.as_bytes())
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()?;

    relay_terminal(stream, attachment)
}

/// What the viewer can fit, in characters.
#[derive(Deserialize)]
struct Resize {
    cols: u16,
    rows: u16,
}

/// Pump bytes between the socket and the pseudo-terminal until either ends.
fn relay_terminal(stream: TcpStream, mut attachment: crate::terminal::Attachment) -> Result<()> {
    use tungstenite::{Message, WebSocket};

    // Reads have to give up regularly so the other direction gets a turn; this
    // is one thread serving a full-duplex connection.
    stream.set_read_timeout(Some(TERMINAL_POLL))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut socket = WebSocket::from_raw_socket(stream, tungstenite::protocol::Role::Server, None);

    // The viewer must render at the agent's size rather than resize it, so it
    // is told what that size is before any output arrives.
    let size = attachment.size;
    let _ = socket.send(Message::Text(
        json!({"cols": size.cols, "rows": size.rows}).to_string(),
    ));

    loop {
        // Keystrokes travelling to the agent.
        match socket.read() {
            Ok(Message::Binary(bytes)) => attachment.write(&bytes)?,
            // Text is the control channel: the viewer says what shape it is and
            // the session reflows to match, the way a terminal does when its
            // window changes. Keystrokes are binary.
            Ok(Message::Text(text)) => match serde_json::from_str::<Resize>(&text) {
                Ok(resize) => {
                    attachment.resize(resize.cols, resize.rows)?;
                    let size = attachment.size;
                    let _ = socket.send(Message::Text(
                        json!({"cols": size.cols, "rows": size.rows}).to_string(),
                    ));
                }
                Err(_) => attachment.write(text.as_bytes())?,
            },
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            // Anything else is the viewer going away, which is a detach.
            Err(_) => break,
        }

        // Screen output travelling back.
        while let Some(bytes) = attachment.read(Duration::from_millis(1)) {
            if socket.send(Message::Binary(bytes)).is_err() {
                return Ok(());
            }
        }
        if socket.flush().is_err() {
            break;
        }
    }
    // Dropping the attachment kills `tmux attach`, which detaches the viewer
    // and leaves the agent's session running.
    Ok(())
}

/// What the assistant is running on, and what else it could run on.
fn describe_agent(context: &Arc<Context_>) -> Value {
    let command = context
        .command
        .lock()
        .expect("serve command poisoned")
        .clone();
    json!({
        "backend": crate::config::backend_of_command(&command),
        "available": crate::config::ASSISTANT_BACKENDS,
    })
}

/// Relaunch the assistant on a different backend.
///
/// A backend is chosen when the process starts, so changing it means a new
/// process: the assistant's current session does not survive. That is the
/// operator's call to make, which is why this is an explicit request rather
/// than something inferred.
fn switch_backend(context: &Arc<Context_>, body: &[u8]) -> (u16, Value) {
    #[derive(Deserialize)]
    struct Request {
        backend: String,
    }
    let request: Request = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(error) => return (400, json!({"error": format!("invalid request: {error}")})),
    };
    let command = match crate::config::resolve_backend(&request.backend) {
        Ok(command) => command,
        Err(reason) => return (400, json!({"error": reason})),
    };
    if !crate::config::ASSISTANT_BACKENDS.contains(&request.backend.as_str()) {
        return (
            400,
            json!({"error": format!("'{}' is not an assistant backend", request.backend)}),
        );
    }

    match relaunch_ea(context, &command) {
        Ok(session) => {
            *context.command.lock().expect("serve command poisoned") = command;
            (200, json!({"backend": request.backend, "session": session}))
        }
        Err(error) => (502, json!({"error": format!("{error:#}")})),
    }
}

fn relaunch_ea(context: &Arc<Context_>, command: &str) -> Result<String> {
    let client = TmuxClient::new(crate::ea::ea_prefix(context.ea_id, &context.session_prefix));
    let existing = crate::ea::ea_manager_session(context.ea_id, &context.session_prefix);
    if client.has_session(&existing)? {
        client.kill_session(&existing)?;
    }
    let name = crate::ea::load_registry(&context.omar_dir)
        .into_iter()
        .find(|ea| ea.id == context.ea_id)
        .map(|ea| ea.name)
        .unwrap_or_else(|| format!("ea-{}", context.ea_id));
    let (session, _) = crate::manager::ensure_manager_session(
        &client,
        command,
        context.ea_id,
        &name,
        &context.omar_dir,
        &context.session_prefix,
        &crate::manager::ManagerRuntimeOptions {
            default_workdir: context.default_workdir.clone(),
            health_idle_warning: context.health_idle_warning,
            // Without this the new process has no way to answer the operator.
            serve: Some(crate::manager::ServeMcpContext {
                endpoint: context.address.to_string(),
                token: context.agent_token.clone(),
            }),
        },
    )?;
    Ok(session)
}

fn send_to_ea(context: &Arc<Context_>, body: &[u8]) -> (u16, Value) {
    let request: ChatRequest = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(error) => return (400, json!({"error": format!("invalid request: {error}")})),
    };
    if request.text.trim().is_empty() {
        return (400, json!({"error": "message is empty"}));
    }
    // The selection is echoed into the EA's pane, so it is untrusted input of
    // unbounded size unless bounded here.
    if request.selection.len() > MAX_SELECTION {
        return (
            400,
            json!({"error": format!("selection names more than {MAX_SELECTION} components")}),
        );
    }
    if let Some(name) = request
        .selection
        .iter()
        .find(|name| name.is_empty() || name.len() > MAX_SELECTION_NAME)
    {
        return (
            400,
            json!({"error": format!("selected component name is empty or too long: '{name}'")}),
        );
    }
    let message = context.publish_with_selection(
        "operator",
        request.text.clone(),
        false,
        None,
        request.selection.clone(),
    );
    match deliver_to_ea(context, &request.text, &request.selection) {
        Ok(()) => (202, json!(message)),
        Err(error) => (502, json!({"error": format!("{error:#}")})),
    }
}

/// Mark the message so the EA knows where it came from and how to answer.
///
/// The system prompt alone is not enough: delivered into a pane, an operator
/// message is indistinguishable from any other, and the EA answers in the
/// terminal where nobody is looking. Topology agents get an `OMAR INVOCATION`
/// envelope for the same reason.
fn mission_control_envelope(text: &str, selection: &[String]) -> String {
    // Named on their own line so the EA can tell the operator's words from the
    // components they had highlighted while writing them.
    let selected = if selection.is_empty() {
        String::new()
    } else {
        format!(
            "The operator has selected these components in the diagram: {}. \
             When they say \"this\" or \"these\", that is what they mean.\n\n",
            selection.join(", ")
        )
    };
    format!(
        "OMAR MISSION CONTROL\n\
         Answer by CALLING THE MCP TOOL `omar_reply` on the MCP server named \"omar\". \
         Your backend may list it as `omar__omar_reply` or `mcp__omar__omar_reply`. \
         It is an MCP tool call, NOT a shell command: there is no `omar_reply` \
         executable, and the `omar` CLI cannot send it. Do not search your PATH for it.\n\
         The operator is in Mission Control and cannot see this terminal, so text you \
         print here reaches nobody. Every question, status update, and answer must go \
         through `omar_reply`.\n\
         To offer a workflow, call the MCP tool `omar_propose_design` with a complete \
         OMAR program. The operator approves and runs it; you do not.\n\n\
         {selected}{text}"
    )
}

fn deliver_to_ea(context: &Arc<Context_>, text: &str, selection: &[String]) -> Result<()> {
    let client = TmuxClient::new(crate::ea::ea_prefix(context.ea_id, &context.session_prefix));
    let session = crate::ea::ea_manager_session(context.ea_id, &context.session_prefix);
    anyhow::ensure!(
        client.has_session(&session)?,
        "the executive assistant is not running; start it with `omar` first"
    );
    client.deliver_prompt(
        &session,
        &mission_control_envelope(text, selection),
        &DeliveryOptions::default(),
    )?;
    Ok(())
}

fn agent_reply(context: &Arc<Context_>, body: &[u8]) -> (u16, Value) {
    let reply: AgentReply = match serde_json::from_slice(body) {
        Ok(reply) => reply,
        Err(error) => return (400, json!({"error": format!("invalid request: {error}")})),
    };
    if reply.token != context.agent_token {
        return (403, json!({"error": "forbidden"}));
    }
    context.publish("assistant", reply.text, reply.progress, None);
    (202, json!({"status": "delivered"}))
}

/// A proposal is a program the operator has not approved. It is published to
/// the conversation and nothing more — only the operator can start a run.
fn agent_proposal(context: &Arc<Context_>, body: &[u8]) -> (u16, Value) {
    let proposal: AgentProposal = match serde_json::from_slice(body) {
        Ok(proposal) => proposal,
        Err(error) => return (400, json!({"error": format!("invalid request: {error}")})),
    };
    if proposal.token != context.agent_token {
        return (403, json!({"error": "forbidden"}));
    }
    // Compile before publishing. The operator never sees an unbuildable
    // program, and the EA gets the compiler's diagnostic back as its tool
    // result, so it can correct the program itself.
    let state = match compile_preview(context, &proposal.program) {
        Ok(state) => state,
        Err(error) => return (400, json!({"error": format!("{error:#}")})),
    };
    context.publish(
        "assistant",
        proposal.summary,
        false,
        Some(ProposedDesign {
            program: proposal.program,
            inputs: proposal.inputs,
            preview: crate::diagram::DiagramSnapshot::from_vm_state(&state),
        }),
    );
    (202, json!({"status": "proposed"}))
}

fn compile_preview(context: &Arc<Context_>, program: &str) -> Result<VmState> {
    let dir = crate::ea::ea_state_dir(context.ea_id, &context.omar_dir).join("proposals");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.omar", Uuid::new_v4()));
    fs::write(&path, program)?;
    let bytecode = topology::load_program(&path)?;
    let state = topology::verify(&bytecode)?;
    let _ = fs::remove_file(&path);
    Ok(state)
}

fn stream_chat(mut stream: TcpStream, context: &Arc<Context_>, origin: Option<&str>) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n{}Vary: Origin\r\n\r\n",
        crate::diagram::cors_origin_header(origin)
    )?;
    let (sender, receiver) = mpsc::sync_channel(CHAT_QUEUE);
    let backlog = {
        let mut chat = context.chat.lock().expect("serve chat poisoned");
        chat.subscribers.push(sender);
        chat.messages.clone()
    };
    // Replay so a reload does not lose the conversation.
    stream.write_all(b": connected\n\n")?;
    for message in backlog {
        write_chat_event(&mut stream, &message)?;
    }
    stream.flush()?;
    loop {
        match receiver.recv_timeout(Duration::from_secs(15)) {
            Ok(message) => {
                if write_chat_event(&mut stream, &message)
                    .and_then(|_| stream.flush().map_err(Into::into))
                    .is_err()
                {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if stream
                    .write_all(b": keepalive\n\n")
                    .and_then(|_| stream.flush())
                    .is_err()
                {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

fn write_chat_event(stream: &mut TcpStream, message: &ChatMessage) -> Result<()> {
    let kind = if message.design.is_some() {
        "design_proposed"
    } else {
        "message"
    };
    writeln!(
        stream,
        "id: {}\nevent: {}\ndata: {}\n",
        message.sequence,
        kind,
        serde_json::to_string(message)?
    )?;
    Ok(())
}

fn start_run(context: &Arc<Context_>, body: &[u8]) -> (u16, Value) {
    let request: StartRunRequest = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(error) => return (400, json!({"error": format!("invalid request: {error}")})),
    };

    let run_id = Uuid::new_v4().to_string();
    let run_dir = crate::ea::ea_state_dir(context.ea_id, &context.omar_dir)
        .join("serve")
        .join(&run_id);
    let program_path = run_dir.join("program.omar");
    if let Err(error) =
        fs::create_dir_all(&run_dir).and_then(|_| fs::write(&program_path, &request.program))
    {
        return (
            500,
            json!({"error": format!("failed to stage program: {error}")}),
        );
    }

    // Compile and validate synchronously so a bad program is a 400 rather than
    // a 201 followed by an asynchronous failure the caller has to poll for.
    let bytecode = match topology::load_program(&program_path) {
        Ok(bytecode) => bytecode,
        Err(error) => return (400, json!({"error": format!("{error:#}")})),
    };
    let state = match topology::verify(&bytecode) {
        Ok(state) => state,
        Err(error) => return (400, json!({"error": format!("{error:#}")})),
    };
    let inputs = match encode_inputs(&state, &request.inputs) {
        Ok(inputs) => inputs,
        Err(error) => return (400, json!({"error": format!("{error:#}")})),
    };
    if let Err(error) = topology::parse_inputs(&state, &inputs) {
        return (400, json!({"error": format!("{error:#}")}));
    }

    // Agent sessions are named `prefix + agent`, so two concurrent runs of one
    // team would fight over the same tmux sessions. Serialise per team.
    {
        let runs = context.runs.lock().expect("serve runs poisoned");
        if let Some(active) = find_active_run(&runs, &state.team) {
            return (
                409,
                json!({
                    "error": format!("team '{}' already has an active run", state.team),
                    "run_id": active.run_id,
                }),
            );
        }
    }

    let record = RunRecord {
        run_id: run_id.clone(),
        team: state.team.clone(),
        status: "starting".to_string(),
        diagram_address: None,
        started_at: now_unix(),
        finished_at: None,
        error: None,
    };
    context
        .runs
        .lock()
        .expect("serve runs poisoned")
        .insert(run_id.clone(), record);

    let (ready_sender, ready_receiver) = mpsc::channel();
    spawn_run_thread(context, &run_id, bytecode, inputs, &request, ready_sender);

    match ready_receiver.recv_timeout(DIAGRAM_READY_TIMEOUT) {
        Ok(diagram_address) => {
            let mut runs = context.runs.lock().expect("serve runs poisoned");
            if let Some(record) = runs.get_mut(&run_id) {
                record.diagram_address = Some(diagram_address.to_string());
                if record.status == "starting" {
                    record.status = "running".to_string();
                }
                return (201, json!(record));
            }
            (500, json!({"error": "run vanished"}))
        }
        Err(_) => {
            let runs = context.runs.lock().expect("serve runs poisoned");
            let message = runs
                .get(&run_id)
                .and_then(|record| record.error.clone())
                .unwrap_or_else(|| "run did not start".to_string());
            (500, json!({"error": message, "run_id": run_id}))
        }
    }
}

fn spawn_run_thread(
    context: &Arc<Context_>,
    run_id: &str,
    bytecode: topology::Bytecode,
    inputs: Vec<String>,
    request: &StartRunRequest,
    ready_sender: mpsc::Sender<SocketAddr>,
) {
    let context = context.clone();
    let run_id = run_id.to_string();
    let replace = request.replace;
    let timeout = Duration::from_secs(request.timeout_seconds);
    thread::spawn(move || {
        let diagram_address: SocketAddr = "127.0.0.1:0".parse().expect("loopback address");
        let outcome = topology::run_topology(
            &bytecode,
            TopologyRunConfig {
                ea_id: context.ea_id,
                omar_dir: &context.omar_dir,
                base_prefix: &context.session_prefix,
                default_workdir: &context.default_workdir,
                health_idle_warning: context.health_idle_warning,
                inputs: &inputs,
                replace,
                timeout,
                diagram_address: Some(diagram_address),
                diagram_ready: Some(ready_sender),
            },
        );
        let mut runs = context.runs.lock().expect("serve runs poisoned");
        if let Some(record) = runs.get_mut(&run_id) {
            record.finished_at = Some(now_unix());
            match outcome {
                Ok(()) => record.status = "completed".to_string(),
                Err(error) => {
                    record.status = "failed".to_string();
                    record.error = Some(format!("{error:#}"));
                }
            }
        }
    });
}

/// `parse_inputs` takes `NAME=VALUE`, where a `path` port wants a bare
/// filesystem path and every other type wants JSON. Mirror that asymmetry.
fn encode_inputs(state: &VmState, inputs: &BTreeMap<String, Value>) -> Result<Vec<String>> {
    let mut encoded = Vec::with_capacity(inputs.len());
    for (name, value) in inputs {
        let port = state
            .ports
            .get(name)
            .with_context(|| format!("unknown input port '{name}'"))?;
        anyhow::ensure!(
            port.kind == PortKind::Input,
            "port '{name}' is not an input"
        );
        let raw = match (port.ty.as_str(), value) {
            ("path", Value::String(path)) => path.clone(),
            _ => serde_json::to_string(value)?,
        };
        encoded.push(format!("{name}={raw}"));
    }
    Ok(encoded)
}

fn find_active_run<'a>(runs: &'a BTreeMap<String, RunRecord>, team: &str) -> Option<&'a RunRecord> {
    runs.values()
        .find(|record| record.team == team && is_active(&record.status))
}

fn is_active(status: &str) -> bool {
    status == "starting" || status == "running"
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn write_json(
    stream: &mut TcpStream,
    status: u16,
    body: &Value,
    origin: Option<&str>,
) -> Result<()> {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        502 => "Bad Gateway",
        _ => "Internal Server Error",
    };
    let payload = if status == 204 {
        Vec::new()
    } else {
        serde_json::to_vec(body)?
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}Access-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: content-type\r\nVary: Origin\r\nConnection: close\r\n\r\n",
        payload.len(),
        crate::diagram::cors_origin_header(origin)
    )?;
    stream.write_all(&payload)?;
    stream.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::{AgentState, PortState};

    fn sample_state() -> VmState {
        VmState {
            version: 1,
            team: "Sample".to_string(),
            instances: BTreeMap::new(),
            timers: BTreeMap::new(),
            agents: BTreeMap::from([(
                "worker".to_string(),
                AgentState {
                    backend: "codex".to_string(),
                    instance: String::new(),
                },
            )]),
            ports: BTreeMap::from([
                (
                    "request".to_string(),
                    PortState {
                        kind: PortKind::Input,
                        ty: "string".to_string(),
                        delay: None,
                        instance: String::new(),
                    },
                ),
                (
                    "resume".to_string(),
                    PortState {
                        kind: PortKind::Input,
                        ty: "path".to_string(),
                        delay: None,
                        instance: String::new(),
                    },
                ),
                (
                    "answer".to_string(),
                    PortState {
                        kind: PortKind::Output,
                        ty: "string".to_string(),
                        delay: None,
                        instance: String::new(),
                    },
                ),
            ]),
            connections: Vec::new(),
            reactions: BTreeMap::new(),
        }
    }

    fn record(team: &str, status: &str) -> RunRecord {
        RunRecord {
            run_id: format!("{team}-{status}"),
            team: team.to_string(),
            status: status.to_string(),
            diagram_address: None,
            started_at: 0,
            finished_at: None,
            error: None,
        }
    }

    fn test_server() -> Serve {
        let omar_dir = std::env::temp_dir().join(format!("omar-serve-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&omar_dir).expect("temp omar dir");
        Serve::start(
            "127.0.0.1:0".parse().expect("valid address"),
            &Config::default(),
            &omar_dir,
            0,
        )
        .expect("server starts")
    }

    #[test]
    fn encodes_paths_bare_and_everything_else_as_json() {
        let encoded = encode_inputs(
            &sample_state(),
            &BTreeMap::from([
                ("request".to_string(), json!("Review this plan")),
                ("resume".to_string(), json!("/tmp/resume.txt")),
            ]),
        )
        .expect("inputs encode");
        // `parse_inputs` canonicalises a bare path but JSON-decodes everything
        // else, so quoting has to differ by port type.
        assert!(encoded.contains(&"request=\"Review this plan\"".to_string()));
        assert!(encoded.contains(&"resume=/tmp/resume.txt".to_string()));
    }

    #[test]
    fn rejects_inputs_that_are_not_input_ports() {
        let error = encode_inputs(
            &sample_state(),
            &BTreeMap::from([("answer".to_string(), json!("nope"))]),
        )
        .expect_err("output ports are rejected");
        assert!(error.to_string().contains("not an input"));

        let error = encode_inputs(
            &sample_state(),
            &BTreeMap::from([("missing".to_string(), json!("nope"))]),
        )
        .expect_err("unknown ports are rejected");
        assert!(error.to_string().contains("unknown input port"));
    }

    #[test]
    fn active_run_lookup_ignores_finished_and_other_teams() {
        let runs = BTreeMap::from([
            ("a".to_string(), record("Sample", "completed")),
            ("b".to_string(), record("Other", "running")),
        ]);
        assert!(find_active_run(&runs, "Sample").is_none());

        let mut runs = runs;
        runs.insert("c".to_string(), record("Sample", "starting"));
        assert_eq!(
            find_active_run(&runs, "Sample").map(|record| record.status.as_str()),
            Some("starting")
        );
    }

    #[test]
    fn refuses_non_loopback_bindings() {
        let error = Serve::start(
            "0.0.0.0:0".parse().expect("valid address"),
            &Config::default(),
            Path::new("/tmp"),
            0,
        )
        .err()
        .expect("non-loopback binding is rejected");
        assert!(error
            .to_string()
            .contains("must bind to a loopback address"));
    }

    #[test]
    fn health_reports_the_protocol_version() {
        let server = test_server();
        let response = request(server.address(), "GET", "/health", None);
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("\"status\":\"ok\""));
        assert!(response.contains(&format!("\"protocol_version\":{SERVE_PROTOCOL_VERSION}")));
    }

    #[test]
    fn run_listing_starts_empty_and_unknown_runs_are_404() {
        let server = test_server();
        assert!(request(server.address(), "GET", "/v1/runs", None).contains("\"runs\":[]"));

        let response = request(server.address(), "GET", "/v1/runs/nope", None);
        assert!(response.starts_with("HTTP/1.1 404 Not Found"));
        assert!(response.contains("unknown run"));
    }

    #[test]
    fn malformed_admission_bodies_are_rejected() {
        let server = test_server();
        let response = request(server.address(), "POST", "/v1/runs", Some("not json"));
        assert!(response.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(response.contains("invalid request"));
    }

    #[test]
    fn preflight_is_allowed_so_a_browser_can_admit_programs() {
        let server = test_server();
        let response = request(server.address(), "OPTIONS", "/v1/runs", None);
        assert!(response.starts_with("HTTP/1.1 204 No Content"));
        assert!(response.contains("Access-Control-Allow-Headers: content-type"));
    }

    #[test]
    fn agent_endpoints_reject_a_wrong_token() {
        let server = test_server();
        for path in ["/v1/agent/reply", "/v1/agent/proposals"] {
            let response = request(
                server.address(),
                "POST",
                path,
                Some(
                    r#"{"token":"not-the-token","text":"hi","program":"team T() {}","summary":"s"}"#,
                ),
            );
            assert!(
                response.starts_with("HTTP/1.1 403 Forbidden"),
                "{path} accepted a bad token: {response}"
            );
        }
        // Nothing reached the conversation.
        assert!(request(server.address(), "GET", "/v1/chat", None).contains("\"messages\":[]"));
    }

    #[test]
    fn an_unbuildable_proposal_never_reaches_the_operator() {
        // The EA gets the compiler diagnostic back as its tool result, so it
        // can correct the program rather than the operator seeing a broken one.
        let server = test_server();
        let body = json!({
            "token": server.agent_token,
            "program": "team Broken( {{{",
            "summary": "nonsense",
        })
        .to_string();
        let response = request(server.address(), "POST", "/v1/agent/proposals", Some(&body));
        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request"),
            "{response}"
        );
        assert!(request(server.address(), "GET", "/v1/chat", None).contains("\"messages\":[]"));
    }

    #[test]
    fn the_manager_session_is_not_under_the_agent_prefix() {
        // `TmuxClient` resolves most lookups through a prefix-filtered session
        // list, but the EA is named `<base>ea-<id>` while agents are
        // `<base><id>-<name>`. Anything reaching for the manager session must
        // use an exact tmux query, or it silently reports "not found".
        let prefix = crate::ea::ea_prefix(0, "omar-agent-");
        let manager = crate::ea::ea_manager_session(0, "omar-agent-");
        assert!(
            !manager.starts_with(&prefix),
            "{manager} unexpectedly sits under {prefix}; prefix-scoped lookups \
             would now work and this guard is obsolete"
        );
    }

    #[test]
    fn the_agent_endpoint_names_the_backend_and_the_alternatives() {
        let server = test_server();

        let response = request(server.address(), "GET", "/v1/agent", None);

        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        // Whatever this machine defaults to, the choices are the ones an
        // operator can actually pick.
        assert!(response.contains("\"available\""), "{response}");
        for backend in crate::config::ASSISTANT_BACKENDS {
            assert!(
                response.contains(backend),
                "{backend} missing from {response}"
            );
        }
        // The stub answers invocations without a model; there is nobody to talk
        // to, so it is not on offer.
        assert!(!response.contains("\"stub\""), "{response}");
    }

    #[test]
    fn an_unknown_backend_is_refused_before_anything_is_killed() {
        // Switching restarts the assistant, so a bad name must be caught while
        // the running one is still untouched.
        let server = test_server();

        let refused = request(
            server.address(),
            "POST",
            "/v1/agent/backend",
            Some(&json!({"backend": "notabackend"}).to_string()),
        );

        assert!(refused.starts_with("HTTP/1.1 400"), "{refused}");
        assert!(refused.contains("Unknown backend"), "{refused}");

        // The stub compiles as a backend but is not one an operator talks to.
        let stub = request(
            server.address(),
            "POST",
            "/v1/agent/backend",
            Some(&json!({"backend": "stub"}).to_string()),
        );
        assert!(stub.starts_with("HTTP/1.1 400"), "{stub}");
        assert!(stub.contains("not an assistant backend"), "{stub}");
    }

    #[test]
    fn operator_messages_are_labelled_with_how_to_answer() {
        // Without this the EA answers in its pane, where nobody is looking.
        let envelope = mission_control_envelope("review the release plan", &[]);
        assert!(envelope.starts_with("OMAR MISSION CONTROL"));
        assert!(envelope.contains("omar_reply"));
        assert!(envelope.contains("omar_propose_design"));
        assert!(envelope.ends_with("review the release plan"));
        // A bare tool name reads as a shell command: codex went looking for an
        // `omar_reply` binary on PATH and then answered into its own pane.
        assert!(envelope.contains("MCP TOOL"));
        assert!(envelope.contains("mcp__omar__omar_reply"));
        assert!(envelope.contains("NOT a shell command"));
    }

    #[test]
    fn a_selection_tells_the_ea_what_this_refers_to() {
        // The operator writes "make this one retry" with two nodes highlighted.
        // Without the selection the EA cannot resolve "this one" at all.
        let envelope = mission_control_envelope(
            "give this one a retry",
            &["n1.agent".to_string(), "n1.out".to_string()],
        );

        assert!(envelope.contains("n1.agent, n1.out"), "{envelope}");
        // The operator's words stay last, so the pane ends with what they said.
        assert!(envelope.ends_with("give this one a retry"), "{envelope}");
        let selected = envelope.find("n1.agent").expect("selection is present");
        let message = envelope
            .find("give this one a retry")
            .expect("message is present");
        assert!(selected < message, "the selection introduces the message");
    }

    #[test]
    fn an_empty_selection_adds_nothing_to_the_envelope() {
        // Most messages have no selection; they should read exactly as before.
        assert_eq!(
            mission_control_envelope("plain question", &[]),
            mission_control_envelope("plain question", &[]),
        );
        assert!(!mission_control_envelope("plain question", &[]).contains("selected"));
    }

    #[test]
    fn an_oversized_selection_is_refused() {
        // It is echoed into the EA's pane, so it is bounded input.
        let server = test_server();
        let selection: Vec<String> = (0..MAX_SELECTION + 1).map(|i| format!("p{i}")).collect();
        let body = json!({"text": "hi", "selection": selection}).to_string();

        let response = request(server.address(), "POST", "/v1/chat", Some(&body));

        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        assert!(response.contains("more than"), "{response}");
    }

    #[test]
    fn a_context_without_this_server_is_caught_at_startup() {
        let server = test_server();
        let omar_dir = std::env::temp_dir().join(format!("omar-ctx-{}", Uuid::new_v4()));
        let path = crate::manager::ea_mcp_context_path(&omar_dir, 0);
        std::fs::create_dir_all(path.parent().expect("context dir")).expect("dir");

        // What a runtime older than the EA tools writes: no `serve` at all.
        std::fs::write(&path, r#"{"ea_id":0,"topology":null}"#).expect("write");
        let error = server
            .verify_ea_context(&omar_dir, 0)
            .expect_err("a context without serve is rejected");
        assert!(error.to_string().contains("no serve context"), "{error}");

        // A live context from some other server must not pass either.
        std::fs::write(
            &path,
            r#"{"serve":{"endpoint":"127.0.0.1:1","token":"other"}}"#,
        )
        .expect("write");
        let error = server
            .verify_ea_context(&omar_dir, 0)
            .expect_err("a foreign token is rejected");
        assert!(error.to_string().contains("different server"), "{error}");

        // Our own token passes.
        std::fs::write(
            &path,
            json!({"serve": {"endpoint": "127.0.0.1:1", "token": server.agent_token}}).to_string(),
        )
        .expect("write");
        assert!(server.verify_ea_context(&omar_dir, 0).is_ok());
        let _ = std::fs::remove_dir_all(&omar_dir);
    }

    #[test]
    fn progress_replies_are_marked_so_the_wait_survives_them() {
        // A running commentary must not read as "the assistant is done", or the
        // operator is left thinking it stopped.
        let server = test_server();
        for (progress, expected) in [(true, "\"progress\":true"), (false, "\"progress\":false")] {
            let body = json!({
                "token": server.agent_token,
                "text": "looking at the ports",
                "progress": progress,
            })
            .to_string();
            let response = request(server.address(), "POST", "/v1/agent/reply", Some(&body));
            assert!(response.starts_with("HTTP/1.1 202 Accepted"), "{response}");
            assert!(
                request(server.address(), "GET", "/v1/chat", None).contains(expected),
                "expected {expected}"
            );
        }

        // Omitting it is the safe default: a plain reply ends the turn.
        let body = json!({"token": server.agent_token, "text": "done"}).to_string();
        let posted = request(server.address(), "POST", "/v1/agent/reply", Some(&body));
        assert!(posted.starts_with("HTTP/1.1 202 Accepted"), "{posted}");
        let chat = request(server.address(), "GET", "/v1/chat", None);
        assert!(chat.contains("\"text\":\"done\""), "{chat}");
        assert!(chat.contains("\"progress\":false"), "{chat}");
    }

    #[test]
    fn chat_rejects_an_empty_message() {
        let server = test_server();
        let response = request(
            server.address(),
            "POST",
            "/v1/chat",
            Some(r#"{"text":"  "}"#),
        );
        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request"),
            "{response}"
        );
    }

    #[test]
    fn a_terminal_carries_the_agent_screen_and_the_operator_keystrokes() {
        // The whole point is steering, so this drives a real tmux session
        // through a real socket: the agent's output has to arrive, and typing
        // has to reach the shell.
        if crate::tmux::tmux_command().arg("-V").output().is_err() {
            eprintln!("skipping: tmux is not installed");
            return;
        }
        // The operator's own tmux server, so the name is distinctive and only
        // this session is cleaned up. OMAR_TMUX_SERVER is process-global and
        // would race every other tmux test.
        let prefix = crate::ea::ea_prefix(0, &Config::default().dashboard.session_prefix);
        let session = format!("{prefix}wsprobe");
        let tmux = |args: &[&str]| {
            let _ = crate::tmux::tmux_command().args(args).output();
        };
        tmux(&["kill-session", "-t", &session]);
        tmux(&[
            "new-session",
            "-d",
            "-s",
            &session,
            "-x",
            "120",
            "-y",
            "40",
            "sh",
        ]);

        let server = test_server();
        let url = format!("ws://{}/v1/agents/wsprobe/terminal", server.address());
        let (mut socket, _) = tungstenite::connect(&url).expect("terminal connects");

        // The viewer is told the agent's size before anything else, so it can
        // render at that size instead of resizing the agent.
        let size: Value = serde_json::from_str(
            &socket
                .read()
                .expect("size frame")
                .into_text()
                .expect("text"),
        )
        .expect("json");
        assert_eq!(size["cols"], json!(120));
        assert_eq!(size["rows"], json!(40));

        socket
            .send(tungstenite::Message::Binary(b"echo omar-ws-ok\n".to_vec()))
            .expect("keystrokes");

        let mut seen = String::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !seen.contains("omar-ws-ok") && std::time::Instant::now() < deadline {
            match socket.read() {
                Ok(tungstenite::Message::Binary(bytes)) => {
                    seen.push_str(&String::from_utf8_lossy(&bytes))
                }
                Ok(_) => {}
                Err(error) => panic!("socket ended early: {error} after {seen}"),
            }
        }
        assert!(seen.contains("omar-ws-ok"), "got: {seen}");

        // Closing the socket detaches without taking the agent with it.
        drop(socket);
        std::thread::sleep(Duration::from_millis(500));
        let survived = crate::terminal::window_size(&session).is_ok();
        tmux(&["kill-session", "-t", &session]);
        assert!(survived, "the agent session outlives its viewer");
    }

    /// A WebSocket handshake for the terminal, with whatever Origin is given.
    fn terminal_handshake(address: SocketAddr, agent: &str, origin: Option<&str>) -> String {
        let mut stream = TcpStream::connect(address).expect("connect");
        let origin = origin
            .map(|value| format!("Origin: {value}\r\n"))
            .unwrap_or_default();
        write!(
            stream,
            "GET /v1/agents/{agent}/terminal HTTP/1.1\r\nHost: {address}\r\n\
             Upgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             {origin}\r\n"
        )
        .expect("request");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut response = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stream, &mut response);
        String::from_utf8_lossy(&response).into_owned()
    }

    #[test]
    fn the_assistant_has_a_terminal_of_its_own() {
        // Its session is `<base>ea-<id>`, not `<base><id>-<name>`, so no agent
        // name can reach it however the route is spelled.
        if crate::tmux::tmux_command().arg("-V").output().is_err() {
            eprintln!("skipping: tmux is not installed");
            return;
        }
        let prefix = Config::default().dashboard.session_prefix;
        let session = crate::ea::ea_manager_session(0, &prefix);
        let agent_shaped = format!("{}ea-0", crate::ea::ea_prefix(0, &prefix));
        assert_ne!(
            session, agent_shaped,
            "if these matched, the agent route would already reach it"
        );

        let tmux = |args: &[&str]| {
            let _ = crate::tmux::tmux_command().args(args).output();
        };
        tmux(&["kill-session", "-t", &format!("={session}")]);
        tmux(&[
            "new-session",
            "-d",
            "-s",
            &session,
            "-x",
            "90",
            "-y",
            "26",
            "sh",
        ]);

        let server = test_server();
        let url = format!("ws://{}/v1/agent/terminal", server.address());
        let (mut socket, _) = tungstenite::connect(&url).expect("assistant terminal connects");
        let size: Value = serde_json::from_str(
            &socket
                .read()
                .expect("size frame")
                .into_text()
                .expect("text"),
        )
        .expect("json");

        tmux(&["kill-session", "-t", &format!("={session}")]);
        assert_eq!(size["cols"], json!(90));
        assert_eq!(size["rows"], json!(26));
    }

    #[test]
    fn a_terminal_refuses_an_origin_it_does_not_know() {
        // CORS never reaches a WebSocket: the browser opens the socket whatever
        // the server would have said about cross-origin reads, so a page the
        // operator merely visits could otherwise get a shell in their agent.
        let server = test_server();

        let refused = terminal_handshake(server.address(), "worker", Some("https://evil.example"));

        assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
        assert!(!refused.contains("101 Switching Protocols"), "{refused}");
    }

    #[test]
    fn a_terminal_for_an_agent_that_is_not_running_is_not_an_upgrade() {
        // The failure has to arrive before the upgrade, while the client can
        // still read an ordinary HTTP error.
        let server = test_server();

        let missing = terminal_handshake(server.address(), "nobody", Some("http://localhost:3000"));

        assert!(missing.starts_with("HTTP/1.1 404"), "{missing}");
    }

    #[test]
    fn a_terminal_without_an_upgrade_is_rejected() {
        let server = test_server();

        let plain = request(server.address(), "GET", "/v1/agents/worker/terminal", None);

        assert!(plain.starts_with("HTTP/1.1 400"), "{plain}");
        assert!(plain.contains("WebSocket"), "{plain}");
    }

    fn request(address: SocketAddr, method: &str, path: &str, body: Option<&str>) -> String {
        let mut stream = TcpStream::connect(address).expect("connect");
        let body = body.unwrap_or("");
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .expect("request");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("response");
        response
    }
}
