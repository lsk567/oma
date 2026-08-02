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

/// Events a slow subscriber may fall behind by before it is dropped.
const SUBSCRIBER_QUEUE: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagramAgent {
    pub id: String,
    pub name: String,
    pub backend: String,
    /// The container it is drawn inside.
    pub instance: String,
}

/// A team `main` instantiated: one container in the drawing.
///
/// Names are flattened by the time the VM sees them, so without this a client
/// would have to rediscover the grouping by splitting on '.' — guessing at a
/// convention instead of reading what the program said.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagramInstance {
    pub id: String,
    pub name: String,
    /// The team it was instantiated from.
    pub team: String,
    /// The container this one is drawn inside, or empty for a top-level one.
    ///
    /// A team can instantiate another team, so containers nest. The id is
    /// given rather than the name so a client never has to re-derive one from
    /// the other.
    #[serde(default)]
    pub parent: String,
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
    pub instance: String,
}

/// A trigger the runtime fires from its own clock.
///
/// Drawn as a clock rather than a port, because that is what it is: nothing
/// feeds it and nothing can write to it. `last_tag` is when it last fired, so a
/// client can show the hand where the schedule actually is rather than
/// animating on a guess.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagramTimer {
    pub id: String,
    pub name: String,
    pub offset: u64,
    /// `0` fires once; anything else re-arms forever.
    pub period: u64,
    pub last_tag: Option<DiagramTag>,
    pub instance: String,
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
    pub instance: String,
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
    /// The containers to draw. Empty for a program compiled before instances
    /// were carried through, which is how a client tells the difference.
    pub instances: Vec<DiagramInstance>,
    pub agents: Vec<DiagramAgent>,
    pub ports: Vec<DiagramPort>,
    /// Empty for a program with no timer, and for bytecode that predates them.
    #[serde(default)]
    pub timers: Vec<DiagramTimer>,
    pub reactions: Vec<DiagramReaction>,
    pub edges: Vec<DiagramEdge>,
}

impl DiagramSnapshot {
    pub fn from_vm_state(state: &VmState) -> Self {
        let instances = state
            .instances
            .iter()
            .map(|(name, instance)| DiagramInstance {
                id: instance_id(name),
                name: name.clone(),
                team: instance.team.clone(),
                parent: if instance.parent.is_empty() {
                    String::new()
                } else {
                    instance_id(&instance.parent)
                },
            })
            .collect();
        let agents = state
            .agents
            .iter()
            .map(|(name, agent)| DiagramAgent {
                id: agent_id(name),
                name: name.clone(),
                backend: agent.backend.clone(),
                instance: agent.instance.clone(),
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
                instance: port.instance.clone(),
            })
            .collect();
        let timers = state
            .timers
            .iter()
            .map(|(name, timer)| DiagramTimer {
                id: timer_id(name),
                name: name.clone(),
                offset: timer.offset,
                period: timer.period,
                last_tag: None,
                instance: timer.instance.clone(),
            })
            .collect();
        // A trigger names either a port or a timer, and the two have separate
        // id spaces, so which one it is has to be decided here rather than by
        // the client pattern-matching on the name.
        let trigger_id = |name: &String| {
            if state.timers.contains_key(name) {
                timer_id(name)
            } else {
                port_id(name)
            }
        };
        let reactions = state
            .reactions
            .iter()
            .map(|(name, reaction)| DiagramReaction {
                id: reaction_id(name),
                name: name.clone(),
                agent: agent_id(&reaction.agent),
                order: reaction.order,
                triggers: reaction.triggers.iter().map(&trigger_id).collect(),
                effects: reaction.effects.iter().map(|name| port_id(name)).collect(),
                contract: reaction.contract.clone(),
                status: "idle".to_string(),
                invocation_id: None,
                instance: reaction.instance.clone(),
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
                    source: trigger_id(trigger),
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
            instances,
            agents,
            ports,
            timers,
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
    subscribers: Arc<Mutex<Vec<mpsc::SyncSender<DiagramEvent>>>>,
    sequence: Arc<AtomicU64>,
}

impl DiagramPublisher {
    /// Apply a snapshot change and stamp it with the event's sequence under one
    /// lock. Doing them separately lets `/v1/diagram` return updated state
    /// carrying the previous sequence, which breaks the versioning contract
    /// clients rely on to tell whether they are behind.
    fn publish_with<F>(&self, kind: &str, tag: Option<DiagramTag>, payload: Value, mutate: F)
    where
        F: FnOnce(&mut DiagramSnapshot),
    {
        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst) + 1;
        let team = {
            let mut snapshot = self.snapshot.write().expect("diagram snapshot poisoned");
            mutate(&mut snapshot);
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
        // `try_send` rather than `send`: a full queue means the subscriber has
        // stopped reading, and blocking here would stall the run itself.
        subscribers.retain(|subscriber| subscriber.try_send(event.clone()).is_ok());
    }

    /// Status is part of the snapshot, so it advances with the sequence like
    /// every other field — setting it on its own would let `/v1/diagram`
    /// report a finished run under the sequence it had while running.
    fn publish_status(&self, kind: &str, status: &str, payload: Value) {
        self.publish_with(kind, None, payload, |snapshot| {
            snapshot.status = status.to_string();
        });
    }
}

impl TopologyObserver for DiagramPublisher {
    fn run_started(&self) {
        self.publish_status("run_started", "running", json!({}));
    }

    fn tag_advanced(&self, timestamp: u64, microstep: u64, ports: &BTreeMap<String, Value>) {
        let tag = DiagramTag {
            timestamp,
            microstep,
        };
        self.publish_with(
            "tag_advanced",
            Some(tag),
            json!({ "ports": ports }),
            |snapshot| {
                snapshot.current_tag = Some(tag);
                for (name, value) in ports {
                    if let Some(port) = snapshot.ports.iter_mut().find(|port| port.name == *name) {
                        port.value = Some(value.clone());
                        port.last_tag = Some(tag);
                    }
                    // A timer arrives in the same event map as a port; it is
                    // the one that fired at this tag.
                    if let Some(timer) =
                        snapshot.timers.iter_mut().find(|timer| timer.name == *name)
                    {
                        timer.last_tag = Some(tag);
                    }
                }
            },
        );
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
        let apply = |snapshot: &mut DiagramSnapshot| {
            if let Some(item) = snapshot
                .reactions
                .iter_mut()
                .find(|item| item.name == reaction)
            {
                item.status = "running".to_string();
                item.invocation_id = Some(invocation_id.to_string());
            }
        };
        self.publish_with(
            "reaction_started",
            Some(tag),
            json!({ "reaction": reaction_id(reaction), "invocation_id": invocation_id }),
            apply,
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
        let apply = |snapshot: &mut DiagramSnapshot| {
            if let Some(item) = snapshot
                .reactions
                .iter_mut()
                .find(|item| item.name == reaction)
            {
                item.status = "completed".to_string();
                item.invocation_id = Some(invocation_id.to_string());
            }
        };
        self.publish_with(
            "reaction_completed",
            Some(tag),
            json!({
                "reaction": reaction_id(reaction),
                "invocation_id": invocation_id,
                "writes": writes
            }),
            apply,
        );
    }

    fn run_completed(&self, outputs: &BTreeMap<String, Value>) {
        self.publish_status("run_completed", "completed", json!({ "outputs": outputs }));
    }

    fn run_failed(&self, message: &str) {
        self.publish_status("run_failed", "failed", json!({ "message": message }));
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
        let address = listener.local_addr()?;
        let snapshot = Arc::new(RwLock::new(DiagramSnapshot::from_vm_state(state)));
        let subscribers = Arc::new(Mutex::new(Vec::new()));
        let publisher = DiagramPublisher {
            snapshot: snapshot.clone(),
            subscribers: subscribers.clone(),
            sequence: Arc::new(AtomicU64::new(0)),
        };
        // Blocking accept rather than a polling loop: `Drop` wakes it with a
        // self-connection, so there is no need to spin, and no added latency on
        // every connection from a poll interval.
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
                    // A signal interrupting `accept` is not a reason to stop
                    // serving.
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
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
    subscribers: Arc<Mutex<Vec<mpsc::SyncSender<DiagramEvent>>>>,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    // The request's own origin decides what CORS we grant it.
    let mut origin = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("origin:") {
            origin = allowed_origin(Some(value));
        }
    }
    let origin = origin.as_deref();

    if method == "OPTIONS" {
        write_headers(&mut stream, "204 No Content", "text/plain", 0, origin)?;
        return Ok(());
    }
    match (method, path) {
        ("GET", "/health") => {
            let body = br#"{"status":"ok"}"#;
            write_headers(
                &mut stream,
                "200 OK",
                "application/json",
                body.len(),
                origin,
            )?;
            stream.write_all(body)?;
        }
        ("GET", "/v1/diagram") => {
            let body = serde_json::to_vec(&*snapshot.read().expect("diagram snapshot poisoned"))?;
            write_headers(
                &mut stream,
                "200 OK",
                "application/json",
                body.len(),
                origin,
            )?;
            stream.write_all(&body)?;
        }
        ("GET", "/v1/events") => {
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n{}Vary: Origin\r\n\r\n",
                cors_origin_header(origin)
            )?;
            // Bounded: a subscriber that stops reading is dropped rather than
            // queueing events until it exhausts memory.
            let (sender, receiver) = mpsc::sync_channel(SUBSCRIBER_QUEUE);
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
            write_headers(
                &mut stream,
                "404 Not Found",
                "application/json",
                body.len(),
                origin,
            )?;
            stream.write_all(body)?;
        }
    }
    Ok(())
}

/// Origins allowed to read the diagram API from a browser.
///
/// `*` would let any page the user happens to visit read their local topology,
/// which loopback binding does nothing to prevent. Mission Control is served
/// from localhost, so echo only loopback origins back.
pub(crate) fn allowed_origin(origin: Option<&str>) -> Option<String> {
    let origin = origin?.trim();
    let host = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))?;
    let host = match host.strip_prefix('[') {
        // Bracketed IPv6: the address itself contains colons.
        Some(rest) => rest.split(']').next()?,
        None => host.split(':').next()?,
    };
    if matches!(host, "localhost" | "127.0.0.1" | "::1") {
        Some(origin.to_string())
    } else {
        None
    }
}

fn write_headers(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    content_length: usize,
    origin: Option<&str>,
) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {content_length}\r\n{}Access-Control-Allow-Methods: GET, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type\r\nVary: Origin\r\nConnection: close\r\n\r\n",
        cors_origin_header(origin)
    )?;
    Ok(())
}

/// Emits nothing when the origin is not one we grant, so the browser applies
/// its default same-origin rule rather than being handed a blanket allowance.
pub(crate) fn cors_origin_header(origin: Option<&str>) -> String {
    match origin {
        Some(origin) => format!("Access-Control-Allow-Origin: {origin}\r\n"),
        None => String::new(),
    }
}

fn agent_id(name: &str) -> String {
    format!("agent::{name}")
}

fn instance_id(name: &str) -> String {
    format!("instance::{name}")
}

fn port_id(name: &str) -> String {
    format!("port::{name}")
}

fn reaction_id(name: &str) -> String {
    format!("reaction::{name}")
}

fn timer_id(name: &str) -> String {
    format!("timer::{name}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::{
        AgentState, ConnectionState, InstanceState, PortState, ReactionState, TimerState,
    };

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
                    "answer".to_string(),
                    PortState {
                        kind: PortKind::Output,
                        ty: "string".to_string(),
                        delay: None,
                        instance: String::new(),
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
                    instance: String::new(),
                    triggers: vec!["request".to_string()],
                    effects: vec!["answer".to_string()],
                    contract: "answer".to_string(),
                    prompt: "Respond".to_string(),
                },
            )]),
        }
    }

    /// Two instances of two teams, as `main { writer = Drafter() … }` gives.
    fn two_instance_state() -> VmState {
        let member = |instance: &str, kind, ty: &str| PortState {
            kind,
            ty: ty.to_string(),
            delay: None,
            instance: instance.to_string(),
        };
        VmState {
            version: 1,
            team: "SimpleBrief".to_string(),
            timers: BTreeMap::new(),
            instances: BTreeMap::from([
                (
                    "writer".to_string(),
                    InstanceState {
                        team: "Drafter".to_string(),
                        parent: String::new(),
                    },
                ),
                (
                    "reviewer".to_string(),
                    InstanceState {
                        team: "Reviewer".to_string(),
                        parent: String::new(),
                    },
                ),
            ]),
            agents: BTreeMap::from([(
                "writer.agent".to_string(),
                AgentState {
                    backend: "codex".to_string(),
                    instance: "writer".to_string(),
                },
            )]),
            ports: BTreeMap::from([
                (
                    "writer.topic".to_string(),
                    member("writer", PortKind::Input, "string"),
                ),
                (
                    "writer.draft".to_string(),
                    member("writer", PortKind::Output, "string"),
                ),
                (
                    "reviewer.draft".to_string(),
                    member("reviewer", PortKind::Input, "string"),
                ),
            ]),
            connections: Vec::new(),
            reactions: BTreeMap::from([(
                "writer.reaction.0".to_string(),
                ReactionState {
                    order: 0,
                    instance: "writer".to_string(),
                    agent: "writer.agent".to_string(),
                    triggers: vec!["writer.topic".to_string()],
                    effects: vec!["writer.draft".to_string()],
                    contract: "writer.draft".to_string(),
                    prompt: "Draft".to_string(),
                },
            )]),
        }
    }

    #[test]
    fn a_timer_is_carried_as_a_timer_and_not_as_a_port() {
        // A timer shares the trigger namespace with ports but nothing else: no
        // value feeds it and no reaction can write to it. If the snapshot gave
        // it a port id, a client would draw it as a port and wire an inbound
        // edge to something that can never have one.
        let mut state = sample_state();
        state.timers.insert(
            "beat".to_string(),
            TimerState {
                offset: 0,
                period: 10,
                instance: String::new(),
            },
        );
        state
            .reactions
            .get_mut("respond")
            .expect("sample reaction")
            .triggers = vec!["beat".to_string()];

        let snapshot = DiagramSnapshot::from_vm_state(&state);

        let timer = snapshot.timers.first().expect("the timer is carried");
        assert_eq!(timer.id, "timer::beat");
        assert_eq!((timer.offset, timer.period), (0, 10));
        assert_eq!(timer.last_tag, None, "it has not fired yet");
        assert!(
            !snapshot.ports.iter().any(|port| port.name == "beat"),
            "a timer is not also a port"
        );

        // The reaction and its edge both reach for the timer's own id.
        assert_eq!(
            snapshot.reactions[0].triggers,
            vec!["timer::beat".to_string()]
        );
        let trigger = snapshot
            .edges
            .iter()
            .find(|edge| edge.kind == "trigger")
            .expect("the timer triggers the reaction");
        assert_eq!(trigger.source, "timer::beat");

        // Firing is what the clock face reads, so it has to survive the wire.
        let publisher = DiagramPublisher {
            snapshot: Arc::new(RwLock::new(snapshot)),
            subscribers: Arc::new(Mutex::new(Vec::new())),
            sequence: Arc::new(AtomicU64::new(0)),
        };
        publisher.tag_advanced(30, 0, &BTreeMap::from([("beat".into(), json!(30))]));
        let fired = publisher.snapshot.read().expect("snapshot").clone();
        assert_eq!(
            fired.timers[0].last_tag,
            Some(DiagramTag {
                timestamp: 30,
                microstep: 0
            })
        );
    }

    #[test]
    fn a_snapshot_names_the_containers_rather_than_implying_them() {
        // A client should not have to rediscover the grouping by splitting
        // names on '.'. That is a convention it does not own, and it would
        // break the moment a name legitimately contained a dot.
        let snapshot = DiagramSnapshot::from_vm_state(&two_instance_state());

        let containers: Vec<_> = snapshot
            .instances
            .iter()
            .map(|instance| {
                (
                    instance.id.as_str(),
                    instance.name.as_str(),
                    instance.team.as_str(),
                )
            })
            .collect();
        assert_eq!(
            containers,
            [
                ("instance::reviewer", "reviewer", "Reviewer"),
                ("instance::writer", "writer", "Drafter"),
            ]
        );

        // And every node says which container it is drawn in.
        for port in &snapshot.ports {
            assert!(!port.instance.is_empty(), "{} has no container", port.id);
        }
        assert_eq!(
            snapshot
                .ports
                .iter()
                .filter(|port| port.instance == "writer")
                .count(),
            2
        );
        assert_eq!(snapshot.reactions[0].instance, "writer");
        assert_eq!(snapshot.agents[0].instance, "writer");
        // The program is still named, so the drawing has an outer title.
        assert_eq!(snapshot.team, "SimpleBrief");
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
    fn events_stream_delivers_and_keeps_the_connection_open() {
        let server = DiagramServer::start(
            &sample_state(),
            "127.0.0.1:0".parse().expect("valid address"),
        )
        .expect("server starts");
        let publisher = server.publisher();

        let mut stream = TcpStream::connect(server.address()).expect("connect");
        write!(
            stream,
            "GET /v1/events HTTP/1.1\r\nHost: {}\r\n\r\n",
            server.address()
        )
        .expect("request");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");

        // The stream carries no history, so the subscriber must be registered
        // before anything is published. `: connected` is written immediately
        // after registration, which makes it a signal rather than a guess —
        // sleeping instead is a race that a slower machine loses.
        let mut reader = BufReader::new(stream);
        let mut seen = String::new();
        read_until(&mut reader, &mut seen, ": connected");
        publisher.run_started();
        // Read to the payload, not to the `event:` line — a frame can arrive
        // split across segments, and stopping at the header leaves the `data:`
        // line still in flight.
        read_until(&mut reader, &mut seen, "\"protocol_version\":1");
        assert!(seen.contains("text/event-stream"), "{seen}");
        assert!(seen.contains(": connected"), "{seen}");
        assert!(seen.contains("event: run_started"), "{seen}");
        assert!(seen.contains("\"protocol_version\":1"), "{seen}");
    }

    #[test]
    fn every_status_change_is_announced_and_advances_the_sequence() {
        // A reader polling /v1/diagram uses `sequence` to tell whether it has
        // seen the current state, so a status that changes without advancing it
        // is a status the reader never learns about.
        //
        // This does not cover the ordering of the two — that window is a few
        // instructions wide and no polling client can reliably observe it. It
        // is closed by construction instead: `publish_status` is the only way
        // to set status, and it mutates under the same lock that stamps the
        // sequence.
        let server = DiagramServer::start(
            &sample_state(),
            "127.0.0.1:0".parse().expect("valid address"),
        )
        .expect("server starts");
        let publisher = server.publisher();

        for (act, status) in [
            (
                Box::new(|p: &DiagramPublisher| p.run_started()) as Box<dyn Fn(&DiagramPublisher)>,
                "running",
            ),
            (
                Box::new(|p: &DiagramPublisher| p.run_failed("boom")),
                "failed",
            ),
            (
                Box::new(|p: &DiagramPublisher| p.run_completed(&BTreeMap::new())),
                "completed",
            ),
        ] {
            let before = get(server.address(), "/v1/diagram");
            act(&publisher);
            let after = get(server.address(), "/v1/diagram");
            assert!(
                after.contains(&format!("\"status\":\"{status}\"")),
                "{after}"
            );
            assert_ne!(
                sequence_of(&before),
                sequence_of(&after),
                "status became {status} without advancing the sequence"
            );
        }
    }

    /// The `sequence` field of a `/v1/diagram` response body.
    fn sequence_of(response: &str) -> u64 {
        let body = response
            .split_once("\r\n\r\n")
            .expect("response has a body")
            .1;
        serde_json::from_str::<Value>(body).expect("valid json")["sequence"]
            .as_u64()
            .expect("sequence is a number")
    }

    #[test]
    fn cors_is_granted_to_loopback_origins_only() {
        // Loopback binding does not stop a page the operator visits from
        // reading this API, so a blanket `*` would leak their topology.
        for origin in [
            "http://localhost:3000",
            "http://127.0.0.1:8080",
            "https://[::1]",
        ] {
            assert_eq!(
                allowed_origin(Some(origin)).as_deref(),
                Some(origin),
                "{origin} should be allowed"
            );
        }
        for origin in [
            "http://evil.example",
            "https://localhost.attacker.com",
            "null",
        ] {
            assert_eq!(
                allowed_origin(Some(origin)),
                None,
                "{origin} must be refused"
            );
        }
        assert_eq!(allowed_origin(None), None);
        // A refused origin gets no header at all, so the browser falls back to
        // its own same-origin rule.
        assert_eq!(cors_origin_header(None), "");
        assert!(cors_origin_header(Some("http://localhost:3000"))
            .starts_with("Access-Control-Allow-Origin: http://localhost:3000"));
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

    /// Accumulate from the stream until `marker` appears or the deadline passes.
    fn read_until(reader: &mut BufReader<TcpStream>, seen: &mut String, marker: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !seen.contains(marker) && std::time::Instant::now() < deadline {
            let mut chunk = [0u8; 512];
            match std::io::Read::read(reader, &mut chunk) {
                Ok(0) => break,
                Ok(read) => seen.push_str(&String::from_utf8_lossy(&chunk[..read])),
                Err(_) => break,
            }
        }
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
