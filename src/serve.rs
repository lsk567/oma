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
use crate::topology::{self, PortKind, TopologyRunConfig, VmState};

pub const SERVE_PROTOCOL_VERSION: u32 = 1;

/// The diagram server binds early in `run_topology`, so this only covers
/// compilation and process start-up.
const DIAGRAM_READY_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BODY_BYTES: usize = 512 * 1024;

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

struct Context_ {
    omar_dir: PathBuf,
    ea_id: EaId,
    session_prefix: String,
    default_workdir: String,
    health_idle_warning: i64,
    runs: Runs,
}

pub struct Serve {
    address: SocketAddr,
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
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let context = Arc::new(Context_ {
            omar_dir: omar_dir.to_path_buf(),
            ea_id,
            session_prefix: config.dashboard.session_prefix.clone(),
            default_workdir: config.agent.default_workdir.clone(),
            health_idle_warning: config.health.idle_warning,
            runs: Runs::default(),
        });
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
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            address,
            running,
            thread: Some(thread),
        })
    }

    pub fn address(&self) -> SocketAddr {
        self.address
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

pub fn run(address: SocketAddr, config: &Config, omar_dir: &Path, ea_id: EaId) -> Result<()> {
    let server = Serve::start(address, config, omar_dir, ea_id)?;
    println!("OMAR serve: http://{}", server.address());
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
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }

    if method == "OPTIONS" {
        return write_json(&mut stream, 204, &Value::Null);
    }

    let (status, body) = match (method.as_str(), path.as_str()) {
        ("GET", "/health") => (
            200,
            json!({"status": "ok", "protocol_version": SERVE_PROTOCOL_VERSION}),
        ),
        ("POST", "/v1/runs") => {
            if content_length > MAX_BODY_BYTES {
                (413, json!({"error": "program too large"}))
            } else {
                let mut raw = vec![0u8; content_length];
                reader.read_exact(&mut raw)?;
                start_run(&context, &raw)
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
    write_json(&mut stream, status, &body)
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

fn write_json(stream: &mut TcpStream, status: u16, body: &Value) -> Result<()> {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        _ => "Internal Server Error",
    };
    let payload = if status == 204 {
        Vec::new()
    } else {
        serde_json::to_vec(body)?
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: content-type\r\nConnection: close\r\n\r\n",
        payload.len()
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
            agents: BTreeMap::from([(
                "worker".to_string(),
                AgentState {
                    backend: "codex".to_string(),
                },
            )]),
            ports: BTreeMap::from([
                (
                    "request".to_string(),
                    PortState {
                        kind: PortKind::Input,
                        ty: "string".to_string(),
                        delay: None,
                    },
                ),
                (
                    "resume".to_string(),
                    PortState {
                        kind: PortKind::Input,
                        ty: "path".to_string(),
                        delay: None,
                    },
                ),
                (
                    "answer".to_string(),
                    PortState {
                        kind: PortKind::Output,
                        ty: "string".to_string(),
                        delay: None,
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
