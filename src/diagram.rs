use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::topology::{PortKind, VmState};

pub const DIAGRAM_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagramAgent {
    pub id: String,
    pub name: String,
    pub backend: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagramPort {
    pub id: String,
    pub name: String,
    pub kind: PortKind,
    #[serde(rename = "type")]
    pub ty: String,
    pub delay: Option<u64>,
    pub value: Option<Value>,
    pub last_tag: Option<DiagramTag>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagramReaction {
    pub id: String,
    pub name: String,
    pub agent: String,
    pub order: usize,
    pub triggers: Vec<String>,
    pub effects: Vec<String>,
    pub contract: String,
    pub status: String,
    pub invocation_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagramEdge {
    pub id: String,
    pub kind: String,
    pub source: String,
    pub target: String,
    pub delay: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagramTag {
    pub timestamp: u64,
    pub microstep: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagramSnapshot {
    pub protocol_version: u32,
    pub team: String,
    pub sequence: u64,
    pub status: String,
    pub current_tag: Option<DiagramTag>,
    pub agents: Vec<DiagramAgent>,
    pub ports: Vec<DiagramPort>,
    pub reactions: Vec<DiagramReaction>,
    pub edges: Vec<DiagramEdge>,
}

impl DiagramSnapshot {
    pub fn from_vm_state(state: &VmState) -> Self {
        let agents = state
            .agents
            .iter()
            .map(|(name, agent)| DiagramAgent {
                id: agent_id(name),
                name: name.clone(),
                backend: agent.backend.clone(),
            })
            .collect();
        let ports = state
            .ports
            .iter()
            .map(|(name, port)| DiagramPort {
                id: port_id(name),
                name: name.clone(),
                kind: port.kind,
                ty: port.ty.clone(),
                delay: port.delay,
                value: None,
                last_tag: None,
            })
            .collect();
        let reactions = state
            .reactions
            .iter()
            .map(|(name, reaction)| DiagramReaction {
                id: reaction_id(name),
                name: name.clone(),
                agent: agent_id(&reaction.agent),
                order: reaction.order,
                triggers: reaction.triggers.iter().map(|name| port_id(name)).collect(),
                effects: reaction.effects.iter().map(|name| port_id(name)).collect(),
                contract: reaction.contract.clone(),
                status: "idle".to_string(),
                invocation_id: None,
            })
            .collect();
        let mut edges = Vec::new();
        for connection in &state.connections {
            edges.push(DiagramEdge {
                id: format!("connection::{}::{}", connection.source, connection.target),
                kind: "connection".to_string(),
                source: port_id(&connection.source),
                target: port_id(&connection.target),
                delay: connection.delay,
            });
        }
        for (name, reaction) in &state.reactions {
            for trigger in &reaction.triggers {
                edges.push(DiagramEdge {
                    id: format!("trigger::{trigger}::{name}"),
                    kind: "trigger".to_string(),
                    source: port_id(trigger),
                    target: reaction_id(name),
                    delay: 0,
                });
            }
            for effect in &reaction.effects {
                edges.push(DiagramEdge {
                    id: format!("effect::{name}::{effect}"),
                    kind: "effect".to_string(),
                    source: reaction_id(name),
                    target: port_id(effect),
                    delay: 0,
                });
            }
        }
        Self {
            protocol_version: DIAGRAM_PROTOCOL_VERSION,
            team: state.team.clone(),
            sequence: 0,
            status: "ready".to_string(),
            current_tag: None,
            agents,
            ports,
            reactions,
            edges,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagramEvent {
    pub protocol_version: u32,
    pub sequence: u64,
    pub team: String,
    pub tag: Option<DiagramTag>,
    pub kind: String,
    pub payload: Value,
}

pub trait TopologyObserver: Send + Sync {
    fn run_started(&self) {}
    fn tag_advanced(&self, _timestamp: u64, _microstep: u64, _ports: &BTreeMap<String, Value>) {}
    fn reaction_started(
        &self,
        _timestamp: u64,
        _microstep: u64,
        _reaction: &str,
        _invocation_id: &str,
    ) {
    }
    fn reaction_completed(
        &self,
        _timestamp: u64,
        _microstep: u64,
        _reaction: &str,
        _invocation_id: &str,
        _writes: &BTreeMap<String, Value>,
    ) {
    }
    fn run_completed(&self, _outputs: &BTreeMap<String, Value>) {}
    fn run_failed(&self, _message: &str) {}
}

pub struct NoopTopologyObserver;

impl TopologyObserver for NoopTopologyObserver {}

#[derive(Clone)]
pub struct DiagramPublisher {
    snapshot: Arc<RwLock<DiagramSnapshot>>,
    subscribers: Arc<Mutex<Vec<mpsc::Sender<DiagramEvent>>>>,
    sequence: Arc<AtomicU64>,
}

impl DiagramPublisher {
    fn publish(&self, kind: &str, tag: Option<DiagramTag>, payload: Value) {
        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst) + 1;
        let team = {
            let mut snapshot = self.snapshot.write().expect("diagram snapshot poisoned");
            snapshot.sequence = sequence;
            snapshot.team.clone()
        };
        let event = DiagramEvent {
            protocol_version: DIAGRAM_PROTOCOL_VERSION,
            sequence,
            team,
            tag,
            kind: kind.to_string(),
            payload,
        };
        let mut subscribers = self
            .subscribers
            .lock()
            .expect("diagram subscribers poisoned");
        subscribers.retain(|subscriber| subscriber.send(event.clone()).is_ok());
    }

    fn set_status(&self, status: &str) {
        self.snapshot
            .write()
            .expect("diagram snapshot poisoned")
            .status = status.to_string();
    }
}

impl TopologyObserver for DiagramPublisher {
    fn run_started(&self) {
        self.set_status("running");
        self.publish("run_started", None, json!({}));
    }

    fn tag_advanced(&self, timestamp: u64, microstep: u64, ports: &BTreeMap<String, Value>) {
        let tag = DiagramTag {
            timestamp,
            microstep,
        };
        {
            let mut snapshot = self.snapshot.write().expect("diagram snapshot poisoned");
            snapshot.current_tag = Some(tag);
            for (name, value) in ports {
                if let Some(port) = snapshot.ports.iter_mut().find(|port| port.name == *name) {
                    port.value = Some(value.clone());
                    port.last_tag = Some(tag);
                }
            }
        }
        self.publish("tag_advanced", Some(tag), json!({ "ports": ports }));
    }

    fn reaction_started(
        &self,
        timestamp: u64,
        microstep: u64,
        reaction: &str,
        invocation_id: &str,
    ) {
        let tag = DiagramTag {
            timestamp,
            microstep,
        };
        if let Some(item) = self
            .snapshot
            .write()
            .expect("diagram snapshot poisoned")
            .reactions
            .iter_mut()
            .find(|item| item.name == reaction)
        {
            item.status = "running".to_string();
            item.invocation_id = Some(invocation_id.to_string());
        }
        self.publish(
            "reaction_started",
            Some(tag),
            json!({ "reaction": reaction_id(reaction), "invocation_id": invocation_id }),
        );
    }

    fn reaction_completed(
        &self,
        timestamp: u64,
        microstep: u64,
        reaction: &str,
        invocation_id: &str,
        writes: &BTreeMap<String, Value>,
    ) {
        let tag = DiagramTag {
            timestamp,
            microstep,
        };
        if let Some(item) = self
            .snapshot
            .write()
            .expect("diagram snapshot poisoned")
            .reactions
            .iter_mut()
            .find(|item| item.name == reaction)
        {
            item.status = "completed".to_string();
            item.invocation_id = Some(invocation_id.to_string());
        }
        self.publish(
            "reaction_completed",
            Some(tag),
            json!({
                "reaction": reaction_id(reaction),
                "invocation_id": invocation_id,
                "writes": writes
            }),
        );
    }

    fn run_completed(&self, outputs: &BTreeMap<String, Value>) {
        self.set_status("completed");
        self.publish("run_completed", None, json!({ "outputs": outputs }));
    }

    fn run_failed(&self, message: &str) {
        self.set_status("failed");
        self.publish("run_failed", None, json!({ "message": message }));
    }
}

pub struct DiagramServer {
    address: SocketAddr,
    publisher: DiagramPublisher,
    running: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl DiagramServer {
    pub fn start(state: &VmState, address: SocketAddr) -> Result<Self> {
        anyhow::ensure!(
            address.ip().is_loopback(),
            "diagram server must bind to a loopback address"
        );
        let listener = TcpListener::bind(address)
            .with_context(|| format!("failed to bind diagram server at {address}"))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let snapshot = Arc::new(RwLock::new(DiagramSnapshot::from_vm_state(state)));
        let subscribers = Arc::new(Mutex::new(Vec::new()));
        let publisher = DiagramPublisher {
            snapshot: snapshot.clone(),
            subscribers: subscribers.clone(),
            sequence: Arc::new(AtomicU64::new(0)),
        };
        let running = Arc::new(AtomicBool::new(true));
        let thread_running = running.clone();
        let thread = thread::spawn(move || {
            while thread_running.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let snapshot = snapshot.clone();
                        let subscribers = subscribers.clone();
                        thread::spawn(move || {
                            let _ = handle_client(stream, snapshot, subscribers);
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
            publisher,
            running,
            thread: Some(thread),
        })
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    pub fn publisher(&self) -> DiagramPublisher {
        self.publisher.clone()
    }
}

impl Drop for DiagramServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn handle_client(
    mut stream: TcpStream,
    snapshot: Arc<RwLock<DiagramSnapshot>>,
    subscribers: Arc<Mutex<Vec<mpsc::Sender<DiagramEvent>>>>,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    while {
        let mut line = String::new();
        reader.read_line(&mut line)? > 0 && line != "\r\n"
    } {}

    if method == "OPTIONS" {
        write_headers(&mut stream, "204 No Content", "text/plain", 0)?;
        return Ok(());
    }
    match (method, path) {
        ("GET", "/health") => {
            let body = br#"{"status":"ok"}"#;
            write_headers(&mut stream, "200 OK", "application/json", body.len())?;
            stream.write_all(body)?;
        }
        ("GET", "/v1/diagram") => {
            let body = serde_json::to_vec(&*snapshot.read().expect("diagram snapshot poisoned"))?;
            write_headers(&mut stream, "200 OK", "application/json", body.len())?;
            stream.write_all(&body)?;
        }
        ("GET", "/v1/events") => {
            stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\nAccess-Control-Allow-Origin: *\r\n\r\n",
            )?;
            let (sender, receiver) = mpsc::channel();
            subscribers
                .lock()
                .expect("diagram subscribers poisoned")
                .push(sender);
            stream.write_all(b": connected\n\n")?;
            stream.flush()?;
            loop {
                match receiver.recv_timeout(Duration::from_secs(15)) {
                    Ok(event) => {
                        let payload = serde_json::to_string(&event)?;
                        if writeln!(
                            stream,
                            "id: {}\nevent: {}\ndata: {}\n",
                            event.sequence, event.kind, payload
                        )
                        .and_then(|_| stream.flush())
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
        }
        _ => {
            let body = br#"{"error":"not found"}"#;
            write_headers(&mut stream, "404 Not Found", "application/json", body.len())?;
            stream.write_all(body)?;
        }
    }
    Ok(())
}

fn write_headers(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    content_length: usize,
) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {content_length}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type\r\nConnection: close\r\n\r\n"
    )?;
    Ok(())
}

fn agent_id(name: &str) -> String {
    format!("agent::{name}")
}

fn port_id(name: &str) -> String {
    format!("port::{name}")
}

fn reaction_id(name: &str) -> String {
    format!("reaction::{name}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::{AgentState, ConnectionState, PortState, ReactionState};

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
                    "answer".to_string(),
                    PortState {
                        kind: PortKind::Output,
                        ty: "string".to_string(),
                        delay: None,
                    },
                ),
            ]),
            connections: vec![ConnectionState {
                source: "request".to_string(),
                target: "answer".to_string(),
                delay: 1,
            }],
            reactions: BTreeMap::from([(
                "respond".to_string(),
                ReactionState {
                    order: 0,
                    agent: "worker".to_string(),
                    triggers: vec!["request".to_string()],
                    effects: vec!["answer".to_string()],
                    contract: "answer".to_string(),
                    prompt: "Respond".to_string(),
                },
            )]),
            executed_instructions: 0,
        }
    }

    #[test]
    fn snapshot_contains_semantic_nodes_and_edges() {
        let snapshot = DiagramSnapshot::from_vm_state(&sample_state());
        assert_eq!(snapshot.protocol_version, DIAGRAM_PROTOCOL_VERSION);
        assert_eq!(snapshot.agents[0].id, "agent::worker");
        assert_eq!(snapshot.ports.len(), 2);
        assert!(snapshot.edges.iter().any(|edge| edge.kind == "trigger"));
        assert!(snapshot.edges.iter().any(|edge| edge.kind == "effect"));
        assert!(snapshot.edges.iter().any(|edge| edge.kind == "connection"));
    }

    #[test]
    fn server_exposes_snapshot_and_health() {
        let server = DiagramServer::start(
            &sample_state(),
            "127.0.0.1:0".parse().expect("valid address"),
        )
        .expect("server starts");
        let response = get(server.address(), "/v1/diagram");
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("\"team\":\"Sample\""));
        assert!(get(server.address(), "/health").contains("\"status\":\"ok\""));
    }

    #[test]
    fn server_refuses_non_loopback_bindings() {
        let error =
            DiagramServer::start(&sample_state(), "0.0.0.0:0".parse().expect("valid address"))
                .err()
                .expect("non-loopback binding is rejected");
        assert!(error
            .to_string()
            .contains("must bind to a loopback address"));
    }

    fn get(address: SocketAddr, path: &str) -> String {
        let mut stream = TcpStream::connect(address).expect("connect");
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
        )
        .expect("request");
        let mut response = String::new();
        std::io::Read::read_to_string(&mut stream, &mut response).expect("response");
        response
    }
}
