//! Deliver an event without typing it into the agent's input box.
//!
//! Typing an event into the composer is the worst way to reach an agent: it
//! collides with whatever the user is drafting, and the event then reads as
//! something the user said. Several backends expose a side channel that takes
//! a message directly, leaving the input box alone.
//!
//! Resolution is deliberately dynamic — nothing is cached at launch. A pane
//! whose backend has restarted, or whose socket has gone, simply resolves to
//! `None` and delivery falls back to the input box.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};

/// How long to wait on a socket that has accepted the connection but is not
/// reading. Short: the fallback is a working delivery path, not an error.
const WRITE_TIMEOUT: Duration = Duration::from_secs(3);

/// Loopback HTTP is either immediate or wedged; nothing in between.
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to keep waiting for a freshly launched backend to open its port.
/// opencode takes several seconds to boot; past this it is not coming up.
const PROVISION_TIMEOUT: Duration = Duration::from_secs(90);

/// How long an event may sit in a spool before OMAR stops trusting the hook.
/// A hook that is not firing would otherwise swallow every event silently.
const SPOOL_STALE: Duration = Duration::from_secs(600);

/// A side channel into a running agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Channel {
    /// Claude Code's cross-session peer socket. The message is appended to the
    /// session's command queue, which is a separate structure from the input
    /// buffer, so a draft in the composer is untouched.
    ClaudePeer { socket: PathBuf, token: String },
    /// opencode's HTTP server. A `synthetic` message part is shown to the
    /// model but not rendered in the transcript, and `noReply` seats it
    /// without starting a turn.
    OpencodeHttp { port: u16, session: String },
    /// codex's app-server. `thread/inject_items` appends to the thread the
    /// pane's TUI is showing without starting a turn and without drawing
    /// anything, so the agent reads the event on its next turn and the
    /// composer never moves.
    CodexAppServer { socket: PathBuf },
    /// A file the backend's own hook drains into model context.
    ///
    /// cursor-agent and antigravity take no message from outside, but both run
    /// hooks that may return extra context, and that context is attached to
    /// the turn rather than rendered as something the user said. OMAR appends
    /// events here and `omar hook-drain` hands them over when the hook fires.
    ///
    /// Unlike the other channels this one is reactive: an event waits in the
    /// spool until the agent next runs a tool or the user next submits.
    Spool { path: PathBuf },
}

/// How a backend expects a hook to hand back extra context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookFormat {
    /// cursor-agent: `{"additional_context": "..."}`
    Cursor,
    /// antigravity: `{"injectSteps": [{"ephemeralMessage": "..."}]}`
    Antigravity,
}

impl HookFormat {
    pub fn parse(name: &str) -> Option<HookFormat> {
        match name {
            "cursor" => Some(HookFormat::Cursor),
            "agy" | "antigravity" => Some(HookFormat::Antigravity),
            _ => None,
        }
    }

    /// Render pending events as the hook's reply. An empty spool must still
    /// produce valid JSON, or the backend treats the hook as failed.
    pub fn render(&self, events: &[String]) -> String {
        if events.is_empty() {
            return "{}".to_string();
        }
        let joined = events.join("\n\n");
        match self {
            HookFormat::Cursor => serde_json::json!({ "additional_context": joined }).to_string(),
            HookFormat::Antigravity => {
                serde_json::json!({ "injectSteps": [{ "ephemeralMessage": joined }] }).to_string()
            }
        }
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// Has the oldest queued event been waiting longer than a working hook would
/// ever leave it?
fn spool_is_stale(path: &Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };
    contents
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|event| event.get("at")?.as_u64())
        .min()
        .is_some_and(|oldest| now_secs().saturating_sub(oldest) > SPOOL_STALE.as_secs())
}

/// Take everything queued in a spool, leaving it empty.
///
/// Truncating as we read is what keeps an event from being delivered twice
/// when the hook fires again a moment later.
pub fn drain_spool(path: &Path) -> Vec<String> {
    // Claim the queue by renaming it: cursor registers two hook events, and
    // if both fire at once a read-then-truncate would hand the same events
    // over twice. Only one rename can win.
    let claimed = path.with_extension("draining");
    let Ok(()) = std::fs::rename(path, &claimed) else {
        return Vec::new();
    };
    let contents = std::fs::read_to_string(&claimed).unwrap_or_default();
    let _ = std::fs::remove_file(&claimed);
    contents
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|event| event.get("text")?.as_str().map(str::to_string))
        .collect()
}

impl Channel {
    /// Find the side channel for a pane, if its backend offers one.
    ///
    /// `pane_pid` is the pane's own process. The backend may be a child of it
    /// when the pane runs a shell, so its children are checked too.
    ///
    /// `Some` means a channel looks available, not that a delivery through it
    /// will land. codex resolves on the socket file existing and only
    /// discovers in [`Channel::deliver`] that the pane has more than one
    /// thread loaded and cannot be addressed. Callers must treat the error
    /// from `deliver` as the real answer — the scheduler does, by protecting
    /// the draft again before it falls back to the input box.
    pub fn resolve(backend: &str, pane_pid: u32, stamp: Option<&str>) -> Option<Channel> {
        // A stamp is written when the backend needed provisioning at launch;
        // it names the port and session an event must be addressed to.
        if let Some(channel) = stamp.and_then(Channel::from_stamp) {
            // A spool only works if the backend's hook is draining it. If the
            // oldest event has been waiting too long it plainly is not, so
            // stop feeding it and let delivery fall back to the input box.
            if let Channel::Spool { path } = &channel {
                if spool_is_stale(path) {
                    return None;
                }
            }
            // A pane that has exited takes its app-server with it. Answering
            // with a channel whose socket is gone only costs the caller a
            // round trip before it falls back.
            if let Channel::CodexAppServer { socket } = &channel {
                if !socket.exists() {
                    return None;
                }
            }
            return Some(channel);
        }
        match backend {
            "claude" => {
                let sessions = claude_sessions_dir()?;
                std::iter::once(pane_pid)
                    .chain(child_pids(pane_pid))
                    .find_map(|pid| claude_peer(&sessions, pid))
            }
            _ => None,
        }
    }

    /// Parse a stamp written at launch, e.g. `opencode:47455:ses_abc`.
    fn from_stamp(stamp: &str) -> Option<Channel> {
        let (kind, rest) = stamp.split_once(':')?;
        match kind {
            "opencode" => {
                let (port, session) = rest.split_once(':')?;
                (!session.is_empty()).then_some(Channel::OpencodeHttp {
                    port: port.parse().ok()?,
                    session: session.to_string(),
                })
            }
            "spool" => (!rest.is_empty()).then(|| Channel::Spool {
                path: PathBuf::from(rest),
            }),
            "codex" => (!rest.is_empty()).then(|| Channel::CodexAppServer {
                socket: PathBuf::from(rest),
            }),
            _ => None,
        }
    }

    /// Hand the event to the agent. Errors are the caller's cue to fall back.
    pub fn deliver(&self, text: &str) -> Result<()> {
        match self {
            Channel::ClaudePeer { socket, token } => {
                let mut stream = UnixStream::connect(socket)
                    .with_context(|| format!("connect to {}", socket.display()))?;
                stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
                stream
                    .write_all(claude_peer_frames(token, text).as_bytes())
                    .context("write peer message")?;
                stream.flush().context("flush peer message")?;
                Ok(())
            }
            Channel::OpencodeHttp { port, session } => {
                let body = serde_json::json!({
                    "noReply": true,
                    "parts": [{ "type": "text", "text": text, "synthetic": true }],
                })
                .to_string();
                let (status, _) = http_json(
                    *port,
                    "POST",
                    &format!("/session/{}/message", session),
                    Some(&body),
                )
                .context("post message to opencode")?;
                if status != 200 {
                    anyhow::bail!("opencode answered {}", status);
                }
                Ok(())
            }
            Channel::CodexAppServer { socket } => {
                let mut session = CodexSession::open(socket)?;
                let thread = session.only_thread()?;
                session.inject(&thread, text)
            }
            Channel::Spool { path } => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).context("create spool directory")?;
                }
                let line = serde_json::json!({ "at": now_secs(), "text": text }).to_string();
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .with_context(|| format!("open spool {}", path.display()))?;
                writeln!(file, "{}", line).context("append to spool")?;
                Ok(())
            }
        }
    }

    pub fn describe(&self) -> &'static str {
        match self {
            Channel::ClaudePeer { .. } => "claude peer socket",
            Channel::OpencodeHttp { .. } => "opencode http api",
            Channel::CodexAppServer { .. } => "codex app-server",
            Channel::Spool { .. } => "hook spool",
        }
    }
}

/// Where a pane's pending events queue up, for backends reached by a hook.
pub fn reset_spool(session: &str) {
    // Session names are reused when a pane is killed and recreated. Left
    // alone, the new agent would be handed the old one's events, or find a
    // backlog already old enough to be treated as a dead hook.
    let _ = std::fs::remove_file(spool_path(session));
}

pub fn spool_path(session: &str) -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".omar")
        .join("events")
        .join(format!("{}.jsonl", session))
}

/// The tail of the command OMAR installs, and the only thing it will replace
/// when re-installing.
const CURSOR_HOOK_ARGS: &str = "hook-drain --format cursor";

/// Install OMAR's hook so cursor-agent will collect events mid-turn.
///
/// The hook is one entry in the operator's own `~/.cursor/hooks.json`, added
/// without disturbing whatever else is in there. It runs for every pane and
/// reads `$OMAR_EVENT_SPOOL`, so one entry serves them all.
///
/// Returns false if the file cannot be written — the caller must then leave
/// the pane on the input box, because a spool nothing drains is a black hole.
pub fn install_cursor_hook() -> bool {
    let Some(exe) = std::env::current_exe().ok() else {
        return false;
    };
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    let path = home.join(".cursor").join("hooks.json");

    let mut config: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|body| serde_json::from_str(&body).ok())
        .unwrap_or_else(|| serde_json::json!({ "version": 1 }));
    if !config.is_object() {
        return false;
    }

    let command = format!("{} {}", exe.display(), CURSOR_HOOK_ARGS);
    let entry = serde_json::json!({ "command": command });
    let hooks = config
        .as_object_mut()
        .expect("checked above")
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    let Some(hooks) = hooks.as_object_mut() else {
        return false;
    };

    // Two events: one fires while the agent works, the other when the user
    // next submits. Between them an event is never left waiting on an agent
    // that has gone quiet.
    for event in ["postToolUse", "beforeSubmitPrompt"] {
        let list = hooks
            .entry(event)
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .map(std::mem::take)
            .unwrap_or_default();
        let mut kept: Vec<serde_json::Value> = list
            .into_iter()
            .filter(|hook| {
                !hook
                    .get("command")
                    .and_then(|command| command.as_str())
                    // Match our own entry precisely. A bare "hook-drain"
                    // would also drop an operator hook that merely mentions
                    // it — theirs is not ours to remove.
                    .is_some_and(|command| command.ends_with(CURSOR_HOOK_ARGS))
            })
            .collect();
        kept.push(entry.clone());
        hooks.insert(event.to_string(), serde_json::Value::Array(kept));
    }

    write_json_atomically(&path, &config)
}

/// Install OMAR's hook so antigravity will collect events before each turn.
///
/// Hooks are a map of named hooks that the CLI merges, so OMAR claims one name
/// and leaves the rest of the file alone. `PreInvocation` runs just before the
/// model is called, which is the moment queued events are worth handing over.
///
/// The file is the shared one under `~/.gemini/config` rather than the
/// per-workspace `.agents/hooks.json`. Both are read — the workspace one on a
/// second load, once the folder is resolved and trusted — but a copy per
/// workspace would leave an untracked file in the operator's repository, and
/// one shared entry already serves every pane.
pub fn install_antigravity_hook() -> bool {
    let Some(exe) = std::env::current_exe().ok() else {
        return false;
    };
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    let path = home.join(".gemini").join("config").join("hooks.json");

    let mut config: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|body| serde_json::from_str(&body).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let Some(hooks) = config.as_object_mut() else {
        return false;
    };

    hooks.insert(
        "omar".to_string(),
        serde_json::json!({
            "PreInvocation": [{
                "type": "command",
                "command": format!("{} hook-drain --format agy", exe.display()),
            }]
        }),
    );

    write_json_atomically(&path, &config)
}

/// Replace a file in one step.
///
/// These hook files are shared with the operator and with other panes: a
/// half-written one reads as invalid JSON, and the next writer would treat it
/// as absent and overwrite whatever the operator had configured.
fn write_json_atomically(path: &Path, value: &serde_json::Value) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return false;
    }
    let temp = parent.join(format!(".omar-{}.tmp", std::process::id()));
    if std::fs::write(&temp, value.to_string()).is_err() {
        return false;
    }
    if std::fs::rename(&temp, path).is_err() {
        let _ = std::fs::remove_file(&temp);
        return false;
    }
    true
}

/// Claim a free loopback port for a backend that must be told one at launch.
///
/// The socket is closed immediately, so this reserves nothing — it only picks
/// a number the OS was willing to hand out. If something else takes it first
/// opencode cannot bind; observed behaviour is that it keeps running without a
/// listener, so provisioning times out and the pane falls back to the input
/// box, but that is the backend's choice rather than a guarantee.
pub fn free_port() -> Option<u16> {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .ok()?
        .local_addr()
        .ok()
        .map(|addr| addr.port())
}

/// Give a freshly launched pane a side channel, once its backend is listening.
///
/// Runs in the background: opencode takes seconds to boot, and a launch must
/// not block on it. Until the stamp lands, deliveries fall back to the input
/// box, so being slow is safe and failing is safe.
pub fn provision_in_background(backend: Option<&str>, session: String, command: String) {
    // Gate on the backend, not on the flag: plenty of other things are
    // launched with a `--port`, and polling one of those for 90 seconds — then
    // stamping whatever answered as a delivery channel — would be worse than
    // having no channel at all.
    let stamped: Box<dyn FnOnce() -> Option<String> + Send> = match backend {
        Some("opencode") => match opencode_port(&command) {
            Some(port) => Box::new(move || provision_opencode(port)),
            None => return,
        },
        Some("codex") => match codex_home(&command) {
            Some(home) => {
                // Which pane this home belongs to, so a later launch can tell
                // that it is finished and reclaim the disk.
                crate::manager::claim_codex_home(&home, &session);
                Box::new(move || provision_codex(&home))
            }
            None => return,
        },
        _ => return,
    };
    std::thread::spawn(move || {
        if let Some(stamp) = stamped() {
            let _ = crate::tmux::TmuxClient::new("").set_session_delivery(&session, &stamp);
        }
    });
}

/// `export CODEX_HOME='<dir>'` as it appears in a launch command.
///
/// The launch command is where OMAR records which per-pane codex home this
/// pane was given, the same way `--port` records opencode's port.
///
/// The value is read back through the quoting `shell_single_quote` put on it,
/// rather than split on whitespace: an operator whose home directory has a
/// space in it is ordinary, and half a path here is worse than none. A path
/// that does not come back whole yields `None`, so provisioning is skipped and
/// the pane falls back to the input box.
pub(crate) fn codex_home(command: &str) -> Option<PathBuf> {
    let assignment = command.split_once("export CODEX_HOME=")?.1;
    let dir = unquote_single(assignment)?;
    (!dir.is_empty()).then(|| PathBuf::from(dir))
}

/// Undo `shell_single_quote`: read one `'...'` word, in which a literal quote
/// appears as `'\''`.
fn unquote_single(text: &str) -> Option<String> {
    let mut rest = text.strip_prefix('\'')?;
    let mut out = String::new();
    loop {
        let (chunk, tail) = rest.split_once('\'')?;
        out.push_str(chunk);
        match tail.strip_prefix("\\''") {
            // An escaped quote: the word continues.
            Some(tail) => {
                out.push('\'');
                rest = tail;
            }
            // Anything else closes the word.
            None => return Some(out),
        }
    }
}

/// Where a codex home's app-server listens.
pub fn codex_socket_path(home: &Path) -> PathBuf {
    home.join("app-server-control")
        .join("app-server-control.sock")
}

/// Wait for a pane's app-server to come up with the TUI attached to it.
///
/// The socket file appears before the server is answering, and the server
/// answers before the TUI has opened its thread — an empty thread list is the
/// signal that the pane is not attached yet, so both are waited out here.
fn provision_codex(home: &Path) -> Option<String> {
    let socket = codex_socket_path(home);
    let deadline = std::time::Instant::now() + PROVISION_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if socket.exists()
            && CodexSession::open(&socket)
                .and_then(|mut session| session.only_thread())
                .is_ok()
        {
            return Some(format!("codex:{}", socket.display()));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    None
}

/// A JSON-RPC conversation with a codex app-server.
///
/// The transport is a WebSocket over a Unix socket. `tungstenite::client`
/// performs the upgrade: it wants a URL only to build the request line and
/// `Host` header, so a placeholder is fine — the bytes go wherever the stream
/// already points, which here is a socket path.
pub(crate) struct CodexSession {
    socket: tungstenite::WebSocket<UnixStream>,
    next_id: u64,
    notifications: Vec<serde_json::Value>,
}

impl CodexSession {
    /// Connect, upgrade, and complete the app-server's opening handshake.
    pub(crate) fn open(path: &Path) -> Result<CodexSession> {
        let stream =
            UnixStream::connect(path).with_context(|| format!("connect to {}", path.display()))?;
        stream.set_read_timeout(Some(WRITE_TIMEOUT))?;
        stream.set_write_timeout(Some(WRITE_TIMEOUT))?;

        let (socket, _) = tungstenite::client("ws://localhost/", stream).map_err(|error| {
            match error {
                // The read timeout set above surfaces as WouldBlock, which
                // tungstenite renders as a bare "Interrupted handshake" —
                // say what actually ran out, since provisioning retries this
                // for 90s and every line would otherwise read the same.
                tungstenite::HandshakeError::Interrupted(_) => anyhow::anyhow!(
                    "app-server did not answer the websocket upgrade within {}s",
                    WRITE_TIMEOUT.as_secs()
                ),
                tungstenite::HandshakeError::Failure(error) => {
                    anyhow::anyhow!("websocket upgrade failed: {error}")
                }
            }
        })?;

        let mut session = CodexSession {
            socket,
            next_id: 1,
            notifications: Vec::new(),
        };
        session
            .call(
                "initialize",
                serde_json::json!({
                    "clientInfo": {
                        "name": "omar",
                        "title": "omar",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }),
            )
            .context("initialize the app-server session")?;
        session.notify("initialized")?;
        Ok(session)
    }

    fn send(&mut self, message: serde_json::Value) -> Result<()> {
        self.socket
            .send(tungstenite::Message::Text(message.to_string()))
            .context("write to the app-server")
    }

    fn notify(&mut self, method: &str) -> Result<()> {
        self.send(serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": {},
        }))
    }

    /// Send a request and read until its answer arrives.
    ///
    /// The server pushes notifications of its own down the same socket, so a
    /// reply is found by id rather than by being the next message.
    pub(crate) fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;

        loop {
            let message = self.socket.read().context("read from the app-server")?;
            let tungstenite::Message::Text(body) = message else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&body) else {
                continue;
            };
            if value.get("method").is_some() {
                // Bounded: readers project only allowlisted lifecycle events.
                if matches!(
                    value["method"].as_str(),
                    Some("turn/started" | "turn/completed" | "item/started" | "item/completed")
                ) && self.notifications.len() < 2048
                {
                    self.notifications.push(value);
                }
                continue;
            }
            if value.get("id").and_then(serde_json::Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = value.get("error") {
                anyhow::bail!("{} failed: {}", method, error);
            }
            return Ok(value.get("result").cloned().unwrap_or_default());
        }
    }

    pub(crate) fn take_notifications(&mut self) -> Vec<serde_json::Value> {
        std::mem::take(&mut self.notifications)
    }

    /// The thread the pane is showing, when that is unambiguous.
    pub(crate) fn only_thread(&mut self) -> Result<String> {
        let listed = self.call("thread/loaded/list", serde_json::json!({}))?;
        only_thread(&listed).context("the pane has no single loaded thread to inject into")
    }

    /// Append the event to a thread without starting a turn.
    fn inject(&mut self, thread: &str, text: &str) -> Result<()> {
        self.call(
            "thread/inject_items",
            serde_json::json!({
                "threadId": thread,
                "items": [{
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": text }],
                }],
            }),
        )?;
        Ok(())
    }
}

/// The thread an event belongs in, out of a `thread/loaded/list` reply — and
/// only when there is no doubt which that is.
///
/// The app-server will say which threads a pane has loaded but not which one
/// it is showing, and neither creation order nor activity separates them:
/// `/new` leaves the old thread loaded under a newer id, and `/resume` puts
/// the pane back on an *older* id while the abandoned new one stays loaded. A
/// guess that lands on the thread the user has moved on from is delivered,
/// acknowledged, and never read.
///
/// So OMAR only claims a channel it is sure of. More than one loaded thread
/// means no channel, and delivery goes back through the input box — which
/// since the draft is protected is a working path, not a failure.
fn only_thread(listed: &serde_json::Value) -> Option<String> {
    let threads = listed.get("data")?.as_array()?;
    match threads.as_slice() {
        [only] => only.as_str().map(str::to_string),
        _ => None,
    }
}

/// `--port N` as it appears in a launch command.
fn opencode_port(command: &str) -> Option<u16> {
    let mut tokens = command.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == "--port" {
            return tokens.next()?.parse().ok();
        }
        if let Some(value) = token.strip_prefix("--port=") {
            return value.parse().ok();
        }
    }
    None
}

/// Create a session on a running opencode server and point its TUI at it.
///
/// opencode's API cannot say which session a given pane is showing, and a pane
/// only creates one once the user speaks. So OMAR makes the session itself:
/// the id it gets back is then unambiguously this pane's, even when several
/// agents share a directory.
fn provision_opencode(port: u16) -> Option<String> {
    let deadline = std::time::Instant::now() + PROVISION_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if let Ok((200, body)) = http_json(port, "POST", "/session", Some("{}")) {
            let session = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|value| value.get("id")?.as_str().map(str::to_string))?;
            let select = serde_json::json!({ "sessionID": session }).to_string();
            // Without this the pane keeps showing a different session and the
            // user never sees what the agent was told — so a refusal here must
            // not be stamped as a working channel.
            match http_json(port, "POST", "/tui/select-session", Some(&select)) {
                Ok((200, _)) => return Some(format!("opencode:{}:{}", port, session)),
                _ => return None,
            }
        }
        // Tight, because this is a race: the pane's first prompt is typed in
        // once the backend looks ready, and if that lands before the session
        // swap the first task goes to a session later events do not.
        std::thread::sleep(Duration::from_millis(250));
    }
    None
}

/// POST JSON to opencode's loopback server and return the status code.
///
/// Hand-rolled rather than pulling in an HTTP stack: the server is on
/// 127.0.0.1, the request shape is fixed, and the response is discarded.
fn http_json(port: u16, method: &str, path: &str, body: Option<&str>) -> Result<(u16, String)> {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port))
        .with_context(|| format!("connect to 127.0.0.1:{}", port))?;
    stream.set_read_timeout(Some(HTTP_TIMEOUT))?;
    stream.set_write_timeout(Some(HTTP_TIMEOUT))?;

    let body = body.unwrap_or("");
    let request = format!(
        "{} {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        method,
        path,
        port,
        body.len(),
        body
    );
    stream.write_all(request.as_bytes())?;
    stream.flush()?;

    let mut response = Vec::new();
    std::io::Read::read_to_end(&mut stream, &mut response)?;
    let response = String::from_utf8_lossy(&response).into_owned();
    let (status, body) = split_response(&response).context("parse opencode response")?;
    Ok((status, body))
}

/// Split an HTTP/1.x response into its status code and body.
fn split_response(response: &str) -> Option<(u16, String)> {
    let status = response.split_whitespace().nth(1)?.parse().ok()?;
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or_default();
    Some((status, body))
}

/// The two newline-delimited JSON frames a peer sends: authenticate, then the
/// message itself.
///
/// `priority: "next"` queues the event behind any turn already running instead
/// of preempting it — an event is news, not an interrupt.
fn claude_peer_frames(token: &str, text: &str) -> String {
    let auth = serde_json::json!({ "type": "auth", "token": token });
    let message = serde_json::json!({
        "type": "user",
        "priority": "next",
        "message": { "role": "user", "content": text },
    });
    format!("{}\n{}\n", auth, message)
}

fn claude_sessions_dir() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".claude").join("sessions"))
}

/// Read one session's registry entry and its peer token.
///
/// Claude Code writes `<pid>.json` describing the session and a sibling
/// `<pid>.<hash>.key` holding the token a peer must present.
fn claude_peer(sessions: &Path, pid: u32) -> Option<Channel> {
    let registry = std::fs::read_to_string(sessions.join(format!("{}.json", pid))).ok()?;
    let registry: serde_json::Value = serde_json::from_str(&registry).ok()?;
    let socket = PathBuf::from(registry.get("messagingSocketPath")?.as_str()?);
    if !socket.exists() {
        return None;
    }

    let prefix = format!("{}.", pid);
    let token = std::fs::read_dir(sessions)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| {
            path.extension().is_some_and(|ext| ext == "key")
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&prefix))
        })
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok())
        .and_then(|key| key.get("peerToken")?.as_str().map(str::to_string))?;

    Some(Channel::ClaudePeer { socket, token })
}

fn child_pids(parent: u32) -> Vec<u32> {
    Command::new("pgrep")
        .args(["-P", &parent.to_string()])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.trim().parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixListener;

    #[test]
    fn a_peer_message_is_two_json_frames_and_never_touches_the_composer() {
        let frames = claude_peer_frames("deadbeef", "standup in 5 minutes");
        let mut lines = frames.lines();

        let auth: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(auth["type"], "auth");
        assert_eq!(auth["token"], "deadbeef");

        let message: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(message["type"], "user");
        assert_eq!(message["message"]["role"], "user");
        assert_eq!(message["message"]["content"], "standup in 5 minutes");
        // Queued behind a running turn rather than preempting it.
        assert_eq!(message["priority"], "next");

        assert!(lines.next().is_none());
        assert!(frames.ends_with('\n'), "frames are newline-delimited");
    }

    #[test]
    fn delivery_writes_both_frames_to_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("peer.sock");
        let listener = UnixListener::bind(&socket).unwrap();

        let reader = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut received = String::new();
            let _ = stream.read_to_string(&mut received);
            received
        });

        Channel::ClaudePeer {
            socket: socket.clone(),
            token: "t0ken".to_string(),
        }
        .deliver("ship it")
        .expect("deliver over the peer socket");

        let received = reader.join().unwrap();
        assert_eq!(received, claude_peer_frames("t0ken", "ship it"));
    }

    #[test]
    fn a_launch_stamp_names_the_port_and_session_to_address() {
        assert_eq!(
            Channel::from_stamp("opencode:47455:ses_abc"),
            Some(Channel::OpencodeHttp {
                port: 47455,
                session: "ses_abc".to_string()
            })
        );
        // A stamp wins over backend sniffing, so a malformed one must not be
        // silently treated as a working channel.
        for bad in [
            "opencode:47455:",
            "opencode:notaport:ses_abc",
            "opencode:47455",
            "something-else:1:2",
            "",
        ] {
            assert_eq!(
                Channel::from_stamp(bad),
                None,
                "stamp {bad:?} must not parse"
            );
        }
    }

    #[test]
    fn a_port_is_read_back_out_of_a_launch_command() {
        assert_eq!(opencode_port("opencode --port 47455"), Some(47455));
        assert_eq!(opencode_port("FOO=1 opencode --port=47455"), Some(47455));
        assert_eq!(opencode_port("opencode"), None);
        assert_eq!(opencode_port("opencode --port bogus"), None);
    }

    #[test]
    fn an_http_response_yields_its_status_and_body() {
        assert_eq!(
            split_response("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}"),
            Some((200, "{}".to_string()))
        );
        assert_eq!(
            split_response("HTTP/1.1 404 Not Found\r\n\r\n").map(|(status, _)| status),
            Some(404)
        );
    }

    #[test]
    fn a_hook_reply_carries_every_queued_event_in_the_backends_own_shape() {
        let events = vec!["standup in 5".to_string(), "CI went red".to_string()];

        let cursor: serde_json::Value =
            serde_json::from_str(&HookFormat::Cursor.render(&events)).unwrap();
        assert_eq!(cursor["additional_context"], "standup in 5\n\nCI went red");

        let agy: serde_json::Value =
            serde_json::from_str(&HookFormat::Antigravity.render(&events)).unwrap();
        assert_eq!(
            agy["injectSteps"][0]["ephemeralMessage"],
            "standup in 5\n\nCI went red"
        );
    }

    #[test]
    fn an_empty_spool_still_answers_with_valid_json() {
        // A hook that prints nothing reads as a failure to the backend.
        for format in [HookFormat::Cursor, HookFormat::Antigravity] {
            let reply = format.render(&[]);
            serde_json::from_str::<serde_json::Value>(&reply)
                .unwrap_or_else(|_| panic!("{format:?} must render JSON, got {reply:?}"));
        }
        assert_eq!(HookFormat::parse("nonsense"), None);
    }

    #[test]
    fn a_spool_hands_over_each_event_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pane.jsonl");
        let channel = Channel::Spool { path: path.clone() };

        channel.deliver("first").unwrap();
        channel.deliver("second").unwrap();
        assert_eq!(drain_spool(&path), vec!["first", "second"]);

        // Draining empties it, so the next hook call does not repeat them.
        assert!(drain_spool(&path).is_empty());

        channel.deliver("third").unwrap();
        assert_eq!(drain_spool(&path), vec!["third"]);
    }

    #[test]
    fn an_event_with_newlines_survives_the_spool() {
        // Events are one JSON object per line; a multi-line event must not
        // become several events.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pane.jsonl");
        Channel::Spool { path: path.clone() }
            .deliver("line one\nline two")
            .unwrap();
        assert_eq!(drain_spool(&path), vec!["line one\nline two"]);
    }

    #[test]
    fn installing_the_cursor_hook_keeps_the_operators_own_hooks() {
        let _guard = crate::test_env_lock();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var("HOME").ok();
        std::env::set_var("HOME", home.path());

        let path = home.path().join(".cursor").join("hooks.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"version":1,"hooks":{"postToolUse":[{"command":"their-own-linter"}],
                "beforeShellExecution":[{"command":"their-audit"}]}}"#,
        )
        .unwrap();

        assert!(install_cursor_hook());
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();

        let commands = |event: &str| -> Vec<String> {
            config["hooks"][event]
                .as_array()
                .unwrap()
                .iter()
                .map(|hook| hook["command"].as_str().unwrap().to_string())
                .collect()
        };

        // Theirs survives, on the event they put it on and on ours.
        assert!(commands("postToolUse")
            .iter()
            .any(|c| c == "their-own-linter"));
        assert_eq!(commands("beforeShellExecution"), vec!["their-audit"]);
        for event in ["postToolUse", "beforeSubmitPrompt"] {
            assert!(
                commands(event).iter().any(|c| c.contains("hook-drain")),
                "{event} must call OMAR"
            );
        }

        // Installing again must not stack up duplicates.
        assert!(install_cursor_hook());
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let drains = config["hooks"]["postToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|hook| hook["command"].as_str().unwrap().contains("hook-drain"))
            .count();
        assert_eq!(drains, 1, "reinstalling must replace, not append");

        match previous {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn installing_the_antigravity_hook_claims_one_name_and_leaves_the_rest() {
        let _guard = crate::test_env_lock();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var("HOME").ok();
        std::env::set_var("HOME", home.path());
        let path = home
            .path()
            .join(".gemini")
            .join("config")
            .join("hooks.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"safety-gate":{"enabled":false,"PreToolUse":[{"matcher":"run_command",
                "hooks":[{"command":"./scripts/safety-check.sh"}]}]}}"#,
        )
        .unwrap();

        assert!(install_antigravity_hook());
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();

        // Their named hook is untouched — the CLI merges names, so ours sits
        // alongside rather than replacing the file.
        assert_eq!(config["safety-gate"]["enabled"], false);
        assert_eq!(
            config["safety-gate"]["PreToolUse"][0]["matcher"],
            "run_command"
        );

        let ours = &config["omar"]["PreInvocation"][0];
        assert_eq!(ours["type"], "command");
        assert!(ours["command"].as_str().unwrap().contains("hook-drain"));
        assert!(ours["command"].as_str().unwrap().contains("--format agy"));

        match previous {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn a_spool_nothing_is_draining_stops_being_used() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pane.jsonl");
        let stamp = format!("spool:{}", path.display());

        // Nothing queued: the hook has nothing to prove yet.
        Channel::Spool { path: path.clone() }
            .deliver("fresh")
            .unwrap();
        assert!(
            Channel::resolve("cursor", 0, Some(&stamp)).is_some(),
            "a spool with recent events is still usable"
        );

        // An event that has been sitting there far too long means the hook is
        // not running, so delivery must go back through the input box.
        let stale = serde_json::json!({ "at": 1_000, "text": "ancient" });
        std::fs::write(&path, format!("{}\n", stale)).unwrap();
        assert_eq!(
            Channel::resolve("cursor", 0, Some(&stamp)),
            None,
            "a backed-up spool must not swallow further events"
        );
    }

    #[test]
    fn a_reused_session_name_does_not_inherit_the_last_agents_events() {
        let _guard = crate::test_env_lock();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var("HOME").ok();
        std::env::set_var("HOME", home.path());

        let path = spool_path("omar-agent-1-work");
        Channel::Spool { path: path.clone() }
            .deliver("meant for the previous agent")
            .unwrap();
        assert!(!drain_spool(&path).is_empty() || path.exists());

        Channel::Spool { path: path.clone() }
            .deliver("still here")
            .unwrap();
        reset_spool("omar-agent-1-work");
        assert!(drain_spool(&path).is_empty(), "a new pane starts clean");

        match previous {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn a_half_written_hook_file_never_reaches_disk() {
        // The file is shared with the operator; a truncated one reads as
        // absent and the next writer would overwrite their configuration.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("hooks.json");
        assert!(write_json_atomically(
            &path,
            &serde_json::json!({ "theirs": 1 })
        ));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&path).unwrap())
                .unwrap()["theirs"],
            1
        );
        // No temp files left behind.
        let strays: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(strays.is_empty(), "temp file left behind");
    }

    #[test]
    fn an_operator_hook_that_merely_mentions_hook_drain_is_left_alone() {
        let _guard = crate::test_env_lock();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var("HOME").ok();
        std::env::set_var("HOME", home.path());

        let path = home.path().join(".cursor").join("hooks.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Theirs mentions the subcommand but is not ours.
        std::fs::write(
            &path,
            r#"{"hooks":{"postToolUse":[{"command":"log 'omar hook-drain ran'"}]}}"#,
        )
        .unwrap();

        assert!(install_cursor_hook());
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let commands: Vec<&str> = config["hooks"]["postToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .map(|hook| hook["command"].as_str().unwrap())
            .collect();
        assert!(
            commands.iter().any(|c| c.contains("log '")),
            "the operator's hook must survive: {commands:?}"
        );
        assert_eq!(commands.len(), 2, "ours is added, theirs is kept");

        match previous {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }

    /// Play the app-server's half of one delivery and report what it was
    /// asked, so the framing is checked against a real socket rather than a
    /// string.
    fn fake_app_server(listener: UnixListener, threads: Vec<&'static str>) -> Vec<String> {
        let (stream, _) = listener.accept().unwrap();
        // `tungstenite::accept` answers the upgrade, so the test exercises the
        // same handshake the app-server does rather than a hand-made reply.
        let mut socket = tungstenite::accept(stream).unwrap();
        let answer = |socket: &mut tungstenite::WebSocket<UnixStream>,
                      request: &serde_json::Value,
                      result: serde_json::Value| {
            let reply = serde_json::json!({ "id": request["id"], "result": result });
            socket
                .send(tungstenite::Message::Text(reply.to_string()))
                .unwrap();
        };

        let mut asked = Vec::new();
        loop {
            let tungstenite::Message::Text(body) = socket.read().unwrap() else {
                continue;
            };
            asked.push(body.clone());
            let request: serde_json::Value = serde_json::from_str(&body).unwrap();
            match request["method"].as_str().unwrap_or_default() {
                "initialize" => {
                    // The server talks unprompted; a reply is found by id, not
                    // by being the next thing to arrive.
                    socket
                        .send(tungstenite::Message::Text(
                            serde_json::json!({
                                "method": "remoteControl/status/changed",
                                "params": { "status": "disabled" },
                            })
                            .to_string(),
                        ))
                        .unwrap();
                    answer(
                        &mut socket,
                        &request,
                        serde_json::json!({ "userAgent": "codex-tui/0.147.0" }),
                    );
                }
                "thread/loaded/list" => answer(
                    &mut socket,
                    &request,
                    serde_json::json!({ "data": threads, "nextCursor": null }),
                ),
                "thread/inject_items" => {
                    answer(&mut socket, &request, serde_json::json!({}));
                    return asked;
                }
                _ => {}
            }
        }
    }

    #[test]
    fn an_event_reaches_codex_as_an_injected_item_over_the_app_server_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("app-server-control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            fake_app_server(listener, vec!["01a02051-f65b-7260-9afe-13ffe5229bf6"])
        });

        Channel::CodexAppServer {
            socket: socket.clone(),
        }
        .deliver("CI went red on main")
        .expect("deliver over the app-server");

        let asked: Vec<serde_json::Value> = server
            .join()
            .unwrap()
            .iter()
            .map(|body| serde_json::from_str(body).unwrap())
            .collect();
        let methods: Vec<&str> = asked
            .iter()
            .map(|request| request["method"].as_str().unwrap())
            .collect();
        assert_eq!(
            methods,
            vec![
                "initialize",
                "initialized",
                "thread/loaded/list",
                "thread/inject_items"
            ]
        );

        let inject = asked.last().unwrap();
        assert_eq!(
            inject["params"]["threadId"], "01a02051-f65b-7260-9afe-13ffe5229bf6",
            "the event belongs in the thread the pane is showing"
        );
        let item = &inject["params"]["items"][0];
        assert_eq!(item["role"], "user");
        assert_eq!(item["content"][0]["text"], "CI went red on main");
        // Every request is JSON-RPC; a notification carries no id.
        assert_eq!(asked[1].get("id"), None);
    }

    #[test]
    fn a_dead_app_server_is_a_failed_delivery_rather_than_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Channel::CodexAppServer {
            socket: dir.path().join("gone.sock"),
        }
        .deliver("standup in 5")
        .is_err());
    }

    /// Provisioning retries `open` for 90s, so a bare "Interrupted handshake"
    /// would be 90s of identical lines that name neither the socket nor the
    /// timeout that produced them.
    #[test]
    fn a_silent_app_server_names_the_timeout_rather_than_the_symptom() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mute.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        // Accept, then say nothing: the upgrade waits out the read timeout.
        let server = std::thread::spawn(move || {
            let held = listener.accept();
            std::thread::sleep(WRITE_TIMEOUT + Duration::from_secs(1));
            drop(held);
        });

        let error = Channel::CodexAppServer { socket }
            .deliver("standup in 5")
            .expect_err("a server that never answers cannot take a delivery");
        let error = format!("{error:#}");
        assert!(
            error.contains("did not answer the websocket upgrade"),
            "want the timeout named, got: {error}"
        );
        server.join().unwrap();
    }

    #[test]
    fn a_codex_stamp_names_the_socket_to_inject_through() {
        assert_eq!(
            Channel::from_stamp("codex:/Users/ke/.omar/codex/ab12/app.sock"),
            Some(Channel::CodexAppServer {
                socket: PathBuf::from("/Users/ke/.omar/codex/ab12/app.sock")
            })
        );
        assert_eq!(Channel::from_stamp("codex:"), None);
    }

    #[test]
    fn a_codex_home_is_read_back_out_of_a_launch_command() {
        assert_eq!(
            codex_home("export CODEX_HOME='/Users/ke/.omar/codex/ab12'; codex --no-alt-screen"),
            Some(PathBuf::from("/Users/ke/.omar/codex/ab12"))
        );
        assert_eq!(codex_home("codex --no-alt-screen"), None);
        assert_eq!(codex_home("export CODEX_HOME=''; codex"), None);
    }

    #[test]
    fn a_home_under_a_directory_with_a_space_comes_back_whole() {
        // Read back through the quoting rather than split on whitespace. Half
        // a path parses as a plausible home, and provisioning would then claim
        // and poll somewhere the pane never was — while the pane's real home,
        // never claimed, ages into the prune.
        for home in [
            "/Users/ke/My Home/.omar/codex/ab12",
            "/Users/ke/it's mine/.omar/codex/ab12",
            "/Users/ke/two  spaces/ab12",
        ] {
            let command = format!(
                "export CODEX_HOME={}; codex app-server --listen unix://",
                crate::manager::shell_single_quote(home)
            );
            assert_eq!(
                codex_home(&command),
                Some(PathBuf::from(home)),
                "in {command}"
            );
        }
        // A word that never closes is not a path worth guessing at.
        assert_eq!(
            codex_home("export CODEX_HOME='/Users/ke/unterminated"),
            None
        );
    }

    #[test]
    fn an_event_goes_only_to_a_thread_there_is_no_doubt_about() {
        let listed = serde_json::json!({ "data": ["01a0204b-f2e8-73e3-b95a-09abf7616b22"], "nextCursor": null });
        assert_eq!(
            only_thread(&listed).as_deref(),
            Some("01a0204b-f2e8-73e3-b95a-09abf7616b22")
        );

        // Two loaded threads and the app-server will not say which the pane is
        // showing. Creation order does not settle it: `/new` moves the pane to
        // the newer id, `/resume` moves it back to the older one while the
        // abandoned new thread stays loaded. Guessing wrong is delivered,
        // acknowledged, and never read — so OMAR declines and the event goes
        // through the input box instead.
        let ambiguous = serde_json::json!({
            "data": ["01a0204b-f2e8-73e3-b95a-09abf7616b22", "01a02051-f65b-7260-9afe-13ffe5229bf6"],
        });
        assert_eq!(only_thread(&ambiguous), None);

        // A pane whose TUI has not opened a thread yet is not a channel.
        assert_eq!(only_thread(&serde_json::json!({ "data": [] })), None);
        assert_eq!(only_thread(&serde_json::json!({})), None);
    }

    #[test]
    fn an_ambiguous_pane_reports_a_failure_rather_than_injecting_somewhere() {
        // The caller's cue to fall back is an error. Returning `Ok` after
        // injecting into a thread nobody is reading would lose the event with
        // no sign that anything went wrong.
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("app-server-control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            fake_app_server(
                listener,
                vec![
                    "01a0204b-f2e8-73e3-b95a-09abf7616b22",
                    "01a02051-f65b-7260-9afe-13ffe5229bf6",
                ],
            )
        });

        let failure = Channel::CodexAppServer {
            socket: socket.clone(),
        }
        .deliver("CI went red")
        .expect_err("two loaded threads must not be guessed between");
        assert!(
            failure.to_string().contains("no single loaded thread"),
            "unexpected error: {failure}"
        );
        drop(server);
    }

    #[test]
    fn a_pane_that_is_gone_takes_its_app_server_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("app.sock");
        let stamp = format!("codex:{}", socket.display());
        assert_eq!(Channel::resolve("codex", 0, Some(&stamp)), None);

        let _listener = UnixListener::bind(&socket).unwrap();
        assert!(Channel::resolve("codex", 0, Some(&stamp)).is_some());
    }

    #[test]
    fn a_spool_stamp_round_trips() {
        assert_eq!(
            Channel::from_stamp("spool:/tmp/omar/events/pane.jsonl"),
            Some(Channel::Spool {
                path: PathBuf::from("/tmp/omar/events/pane.jsonl")
            })
        );
        assert_eq!(Channel::from_stamp("spool:"), None);
    }

    #[test]
    fn provisioning_is_gated_on_the_backend_not_on_the_command() {
        // Plenty of things are launched with a `--port` — tunnels, notebook
        // servers, tensorboard. Polling one of those and then stamping
        // whatever answered as a delivery channel would send events into it.
        // The same goes for an inherited `CODEX_HOME`.
        assert_eq!(opencode_port("tensorboard --port 6006"), Some(6006));
        assert!(codex_home("export CODEX_HOME='/somewhere'; jupyter lab").is_some());
        for backend in [None, Some("claude"), Some("cursor"), Some("agy")] {
            // Nothing is spawned and nothing is stamped: the guard is on the
            // backend, and the flag alone must never be enough.
            provision_in_background(
                backend,
                "unused-session".to_string(),
                "export CODEX_HOME='/somewhere'; tensorboard --port 6006".to_string(),
            );
        }
        // And a backend that is provisioned still needs its command to say so.
        for backend in [Some("opencode"), Some("codex")] {
            provision_in_background(backend, "unused-session".to_string(), "bare".to_string());
        }
    }

    #[test]
    fn taking_the_spool_twice_at_once_hands_each_event_over_once() {
        // cursor registers two hook events; if both fire together a
        // read-then-truncate would deliver the same events twice.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pane.jsonl");
        let channel = Channel::Spool { path: path.clone() };
        for event in ["one", "two", "three"] {
            channel.deliver(event).unwrap();
        }

        let racers: Vec<_> = (0..4)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || drain_spool(&path))
            })
            .collect();

        let mut seen: Vec<String> = racers
            .into_iter()
            .flat_map(|racer| racer.join().unwrap())
            .collect();
        seen.sort();
        assert_eq!(seen, vec!["one", "three", "two"]);
    }

    #[test]
    fn a_backend_without_a_side_channel_resolves_to_nothing() {
        // The fallback is the input box, so "no channel" must be an ordinary
        // answer rather than an error.
        for backend in ["codex", "opencode", "cursor", "agy", "stub"] {
            assert_eq!(Channel::resolve(backend, std::process::id(), None), None);
        }
    }

    #[test]
    fn a_session_whose_socket_is_gone_is_not_offered_as_a_channel() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("4242.json"),
            r#"{"pid":4242,"messagingSocketPath":"/tmp/does-not-exist-omar.sock"}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("4242.abc.key"), r#"{"peerToken":"unused"}"#).unwrap();

        assert_eq!(claude_peer(dir.path(), 4242), None);
    }

    /// Guards against the real registry drifting from the shape we parse.
    /// Skips when no Claude Code session is running, so CI stays green.
    #[test]
    fn a_live_claude_session_resolves_to_a_channel() {
        let Some(sessions) = claude_sessions_dir().filter(|dir| dir.is_dir()) else {
            eprintln!("Skipping test: no Claude Code sessions directory");
            return;
        };

        let live: Vec<u32> = std::fs::read_dir(&sessions)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                name.strip_suffix(".json")?.parse::<u32>().ok()
            })
            .filter(|pid| claude_peer(&sessions, *pid).is_some())
            .collect();

        if live.is_empty() {
            eprintln!("Skipping test: no running Claude Code session to resolve");
            return;
        }

        for pid in live {
            // A session can exit between listing and reading it; that is the
            // registry behaving correctly, not a parse failure.
            let Some(channel) = claude_peer(&sessions, pid) else {
                continue;
            };
            match channel {
                Channel::ClaudePeer { socket, token } => {
                    assert!(!token.is_empty(), "peer token for {pid} must be non-empty");
                    assert!(
                        socket.to_string_lossy().ends_with(".sock"),
                        "peer socket path looks wrong: {}",
                        socket.display()
                    );
                }
                other => panic!("claude must resolve to a peer socket, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_registry_entry_and_its_key_resolve_to_a_channel() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("live.sock");
        let _listener = UnixListener::bind(&socket).unwrap();

        std::fs::write(
            dir.path().join("77.json"),
            serde_json::json!({ "pid": 77, "messagingSocketPath": socket }).to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("77.9f3.key"),
            r#"{"peerToken":"s3cret","procStart":"now"}"#,
        )
        .unwrap();

        assert_eq!(
            claude_peer(dir.path(), 77),
            Some(Channel::ClaudePeer {
                socket,
                token: "s3cret".to_string()
            })
        );
    }
}
