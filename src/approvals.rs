//! Read-only permission observation. Execution and approval are independent:
//! one pane can need input while the rest of a topology keeps running.
//!
//! No terminal scraping and no approval responses: the backend's existing TUI
//! remains the authority. A disconnected observer retains its pending requests.

use std::collections::{BTreeMap, VecDeque};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use ts_rs::TS;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalConnection {
    Connecting,
    Connected,
    Disconnected,
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalOutcome {
    Resolved,
    Denied,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalResolutionMode {
    Terminal,
}

#[derive(Clone, Debug, PartialEq, Serialize, TS)]
pub struct PendingApproval {
    pub request_id: String,
    pub agent_id: String,
    pub agent_name: String,
    pub run_id: Option<String>,
    pub invocation_id: Option<String>,
    // Unix milliseconds, preserved when a request is replayed.
    pub requested_at: u64,
    pub summary: String,
    pub tool_name: String,
    pub scope: String,
    pub command: Option<String>,
    pub cwd: Option<String>,
    pub resolution_mode: ApprovalResolutionMode,
}

#[derive(Clone, Debug, PartialEq, Serialize, TS)]
pub struct ApprovalMonitor {
    pub agent_id: String,
    pub run_id: Option<String>,
    pub state: ApprovalConnection,
}

#[derive(Clone, Debug, PartialEq, Serialize, TS)]
pub struct ApprovalResolution {
    pub request: PendingApproval,
    pub outcome: ApprovalOutcome,
    pub resolved_at: u64,
}

#[derive(Clone, Debug, Default, Serialize, TS)]
pub struct ApprovalSnapshot {
    pub sequence: u64,
    pub requests: Vec<PendingApproval>,
    pub monitors: Vec<ApprovalMonitor>,
    pub recent: Vec<ApprovalResolution>,
}

#[derive(Default)]
struct State {
    snapshot: ApprovalSnapshot,
    watches: BTreeMap<String, Watch>,
    stopped: bool,
}

#[derive(Clone)]
struct Watch {
    monitor: ApprovalMonitor,
    name: String,
    invocation: Option<String>,
}

#[derive(Clone, Default)]
pub struct ApprovalHub(Arc<Mutex<State>>);

#[derive(Clone)]
pub struct ApprovalRun {
    pub hub: ApprovalHub,
    pub run_id: String,
}

impl ApprovalHub {
    pub fn snapshot(&self) -> ApprovalSnapshot {
        let state = self.0.lock().expect("approval state poisoned");
        let mut snapshot = state.snapshot.clone();
        snapshot.monitors = state.watches.values().map(|w| w.monitor.clone()).collect();
        snapshot
    }

    pub fn shutdown(&self) {
        self.0.lock().expect("approval state poisoned").stopped = true;
    }

    pub fn stopped(&self) -> bool {
        self.0.lock().expect("approval state poisoned").stopped
    }

    pub fn watch(
        &self,
        session: String,
        agent: String,
        name: String,
        run: Option<String>,
        socket: Option<PathBuf>,
        supported: bool,
    ) {
        let key = Uuid::new_v4().to_string();
        {
            let mut state = self.0.lock().expect("approval state poisoned");
            state.watches.insert(
                key.clone(),
                Watch {
                    monitor: ApprovalMonitor {
                        agent_id: agent,
                        run_id: run,
                        state: if supported {
                            ApprovalConnection::Connecting
                        } else {
                            ApprovalConnection::Unsupported
                        },
                    },
                    name,
                    invocation: None,
                },
            );
            state.snapshot.sequence += 1;
        }
        if supported {
            let hub = self.clone();
            thread::spawn(move || monitor(hub, key, session, socket));
        }
    }

    fn active(&self, key: &str) -> bool {
        let state = self.0.lock().expect("approval state poisoned");
        !state.stopped && state.watches.contains_key(key)
    }

    fn connection(&self, key: &str, connection: ApprovalConnection) {
        let mut state = self.0.lock().expect("approval state poisoned");
        if let Some(watch) = state.watches.get_mut(key) {
            if watch.monitor.state != connection {
                watch.monitor.state = connection;
                state.snapshot.sequence += 1;
            }
        }
    }

    fn add(&self, key: &str, detail: Detail) -> Option<PendingApproval> {
        let mut state = self.0.lock().expect("approval state poisoned");
        let watch = state.watches.get(key)?;
        let request = PendingApproval {
            request_id: Uuid::new_v4().to_string(),
            agent_id: watch.monitor.agent_id.clone(),
            agent_name: watch.name.clone(),
            run_id: watch.monitor.run_id.clone(),
            invocation_id: watch.invocation.clone(),
            requested_at: detail.started.unwrap_or_else(now),
            summary: detail.summary,
            tool_name: detail.tool,
            scope: detail.scope,
            command: detail.command,
            cwd: detail.cwd,
            resolution_mode: ApprovalResolutionMode::Terminal,
        };
        state.snapshot.requests.push(request.clone());
        state.snapshot.sequence += 1;
        Some(request)
    }

    fn resolve(&self, id: &str, outcome: ApprovalOutcome) {
        let mut state = self.0.lock().expect("approval state poisoned");
        if let Some(index) = state
            .snapshot
            .requests
            .iter()
            .position(|r| r.request_id == id)
        {
            let request = state.snapshot.requests.remove(index);
            state.snapshot.recent.push(ApprovalResolution {
                request,
                outcome,
                resolved_at: now(),
            });
            if state.snapshot.recent.len() > 64 {
                state.snapshot.recent.remove(0);
            }
            state.snapshot.sequence += 1;
        } else if outcome != ApprovalOutcome::Resolved {
            // request/resolved does not include a decision. A following item
            // completion can confirm denial; never call a bare ACK "approved".
            if let Some(recent) = state
                .snapshot
                .recent
                .iter_mut()
                .find(|r| r.request.request_id == id)
            {
                if recent.outcome != outcome {
                    recent.outcome = outcome;
                    state.snapshot.sequence += 1;
                }
            }
        }
    }

    fn supersede(&self, id: &str) {
        let mut state = self.0.lock().expect("approval state poisoned");
        state.snapshot.requests.retain(|r| r.request_id != id);
        state.snapshot.sequence += 1;
    }

    pub fn finish_run(&self, run: &str) {
        let mut state = self.0.lock().expect("approval state poisoned");
        state
            .watches
            .retain(|_, w| w.monitor.run_id.as_deref() != Some(run));
        let ids: Vec<_> = state
            .snapshot
            .requests
            .iter()
            .filter(|r| r.run_id.as_deref() == Some(run))
            .map(|r| r.request_id.clone())
            .collect();
        state.snapshot.sequence += 1;
        drop(state);
        for id in ids {
            self.resolve(&id, ApprovalOutcome::Cancelled);
        }
    }
}

impl ApprovalRun {
    pub fn watch(&self, session: &str, agent: &str, command: &str, backend: &str) {
        let socket =
            crate::channel::codex_home(command).map(|p| crate::channel::codex_socket_path(&p));
        self.hub.watch(
            session.into(),
            format!("agent::{agent}"),
            agent.into(),
            Some(self.run_id.clone()),
            socket,
            backend == "codex",
        );
    }

    pub fn invocation(&self, agent: &str, invocation: &str) -> InvocationGuard {
        let mut state = self.hub.0.lock().expect("approval state poisoned");
        for watch in state.watches.values_mut() {
            if watch.monitor.run_id.as_deref() == Some(&self.run_id)
                && watch.monitor.agent_id == format!("agent::{agent}")
            {
                watch.invocation = Some(invocation.into());
            }
        }
        InvocationGuard {
            run: self.clone(),
            agent: format!("agent::{agent}"),
            invocation: invocation.into(),
        }
    }
}

pub struct InvocationGuard {
    run: ApprovalRun,
    agent: String,
    invocation: String,
}
impl Drop for InvocationGuard {
    fn drop(&mut self) {
        let mut state = self.run.hub.0.lock().expect("approval state poisoned");
        for watch in state.watches.values_mut() {
            if watch.monitor.run_id.as_deref() == Some(&self.run.run_id)
                && watch.monitor.agent_id == self.agent
                && watch.invocation.as_deref() == Some(&self.invocation)
            {
                watch.invocation = None;
            }
        }
        // Finishing an OMAR invocation is not an approval acknowledgement.
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

struct Detail {
    summary: String,
    tool: String,
    scope: String,
    command: Option<String>,
    cwd: Option<String>,
    started: Option<u64>,
}

/// Keep the permission explanation, never an arbitrary tool argument object.
/// Commands containing credential-like material are reviewed only in the TUI.
fn display_text(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    if [
        "password",
        "passwd",
        "secret",
        "token",
        "api_key",
        "api-key",
        "authorization",
        "bearer ",
        "private key",
        "sk-",
    ]
    .iter()
    .any(|word| lower.contains(word))
    {
        return "Sensitive details hidden — review in the agent terminal".into();
    }
    text.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .take(2000)
        .collect()
}

fn field(value: &Value, key: &str) -> Option<String> {
    value[key].as_str().map(display_text)
}

fn detail(method: &str, p: &Value, items: &BTreeMap<String, Value>) -> Option<Detail> {
    let mut result = Detail {
        summary: String::new(),
        tool: String::new(),
        scope: "Review the requested scope in the agent terminal".into(),
        command: None,
        cwd: field(p, "cwd"),
        started: p["startedAtMs"].as_u64().filter(|t| *t <= now()),
    };
    match method {
        "item/commandExecution/requestApproval" => {
            result.summary = "Run a command".into();
            result.tool = "Command execution".into();
            result.command = field(p, "command");
            result.scope = if p
                .get("networkApprovalContext")
                .is_some_and(|v| !v.is_null())
            {
                "Network access requested by this command"
            } else {
                "Execute locally with the permissions shown in the terminal"
            }
            .into();
        }
        "item/fileChange/requestApproval" => {
            result.summary = "Apply file changes".into();
            result.tool = "File changes".into();
            result.scope = field(p, "grantRoot")
                .map(|root| format!("Write access: {root}"))
                .unwrap_or_else(|| {
                    "Write the files listed in the terminal approval request".into()
                });
        }
        "item/permissions/requestApproval" => {
            result.summary = "Grant additional permissions".into();
            result.tool = "Additional permissions".into();
        }
        "mcpServer/elicitation/request" => {
            // The app-server explicitly tracks elicitation as a permission
            // request. The actual form/URL stays in the backend's trusted UI.
            result.summary = "Respond to a server permission request".into();
            result.tool = field(p, "serverName").unwrap_or_else(|| "MCP server".into());
            let request = &p["request"];
            if let Some(message) = field(request, "message") {
                result.summary = message;
            }
            let meta = &request["_meta"];
            if meta["codex_approval_kind"] == "mcp_tool_call" {
                result.tool = field(meta, "tool_name")
                    .or_else(|| field(meta, "tool_title"))
                    .unwrap_or(result.tool);
                if p["serverName"] == "omar" {
                    if let Some(port) = field(&meta["tool_params"], "port") {
                        result.scope = format!("This workflow's {port} output");
                    }
                }
            }
        }
        "item/tool/requestUserInput" => {
            // Ordinary questions share this method. Only the backend's tool
            // approval marker is evidence of a permission request.
            if !p["questions"].as_array()?.iter().any(|q| {
                q["id"]
                    .as_str()
                    .is_some_and(|id| id.starts_with("mcp_tool_call_approval_"))
            }) {
                return None;
            }
            result.summary = "Allow an app tool call".into();
            result.tool = "App tool".into();
            if let Some(question) = p["questions"].as_array().and_then(|questions| {
                questions.iter().find(|q| {
                    q["id"]
                        .as_str()
                        .is_some_and(|id| id.starts_with("mcp_tool_call_approval_"))
                })
            }) {
                if let Some(message) = field(question, "question") {
                    result.summary = message;
                }
            }
            if let Some(item) = p["itemId"].as_str().and_then(|id| items.get(id)) {
                result.tool = field(item, "tool").unwrap_or(result.tool);
                result.summary = format!("Allow {}", result.tool);
                if matches!(result.tool.as_str(), "omar_set_port" | "omar_complete") {
                    result.scope = field(&item["arguments"], "port")
                        .map(|port| format!("This workflow's {port} output"))
                        .unwrap_or_else(|| "This workflow's invocation output".into());
                }
            }
        }
        _ => return None,
    }
    // Reasons can contain raw command arguments; use a bounded, redacted view.
    if let Some(reason) = field(p, "reason").filter(|r| !r.is_empty()) {
        result.summary = reason;
    }
    Some(result)
}

#[derive(Default)]
struct Tracker {
    thread: String,
    items: BTreeMap<String, Value>,
    // Retain resolved identities briefly: replay must not resurrect an ACK.
    requests: VecDeque<(String, String, String)>, // RPC id, public id, item id
    fallback: Option<String>,
}

impl Tracker {
    fn event(&mut self, hub: &ApprovalHub, key: &str, event: &Value) {
        let Some(method) = event["method"].as_str() else {
            return;
        };
        let p = &event["params"];
        if p["threadId"].as_str() != Some(&self.thread) {
            return;
        }
        match method {
            "item/started" | "item/completed" => {
                let item = &p["item"];
                if let Some(id) = item["id"].as_str() {
                    // Keep only metadata needed for approval display, not tool
                    // results, private reasoning, or arbitrary arguments.
                    self.items.insert(id.into(), json!({"tool":item["tool"], "arguments":{"port":item["arguments"]["port"]}}));
                    if self.items.len() > 256 {
                        self.items.pop_first();
                    }
                    if method == "item/completed" {
                        let outcome = if item["status"] == "declined" {
                            ApprovalOutcome::Denied
                        } else {
                            ApprovalOutcome::Resolved
                        };
                        for (_, public, item_id) in &self.requests {
                            if item_id == id {
                                hub.resolve(public, outcome.clone());
                            }
                        }
                    }
                }
            }
            "serverRequest/resolved" => {
                let rpc = p["requestId"].to_string();
                for (id, public, _) in &self.requests {
                    if *id == rpc {
                        hub.resolve(public, ApprovalOutcome::Resolved);
                    }
                }
            }
            "turn/completed" => {
                let outcome = if p["turn"]["status"] == "interrupted" {
                    ApprovalOutcome::Cancelled
                } else {
                    ApprovalOutcome::Resolved
                };
                for (_, public, _) in &self.requests {
                    hub.resolve(public, outcome.clone());
                }
            }
            _ => {
                if event.get("id").is_none() {
                    return;
                }
                let rpc = event["id"].to_string();
                if self.requests.iter().any(|(id, _, _)| *id == rpc) {
                    return;
                }
                if let Some(detail) = detail(method, p, &self.items) {
                    if let Some(request) = hub.add(key, detail) {
                        if let Some(fallback) = self.fallback.take() {
                            hub.supersede(&fallback);
                        }
                        self.requests.push_back((
                            rpc,
                            request.request_id,
                            p["itemId"].as_str().unwrap_or("").into(),
                        ));
                        if self.requests.len() > 256 {
                            self.requests.pop_front();
                        }
                    }
                }
            }
        }
    }

    fn reconcile(&mut self, hub: &ApprovalHub, key: &str, thread: &Value) {
        if let Some(turns) = thread["turns"].as_array() {
            for turn in turns {
                if let Some(items) = turn["items"].as_array() {
                    for item in items {
                        if let Some(id) = item["id"].as_str() {
                            self.items.insert(id.into(), json!({"tool":item["tool"], "arguments":{"port":item["arguments"]["port"]}}));
                        }
                    }
                }
            }
            while self.items.len() > 256 {
                self.items.pop_first();
            }
        }
        let status = &thread["status"];
        let Some(kind) = status["type"].as_str() else {
            return;
        };
        let waiting = kind == "active"
            && status["activeFlags"]
                .as_array()
                .is_some_and(|flags| flags.iter().any(|f| f == "waitingOnApproval"));
        let snapshot = hub.snapshot();
        let has_request = self
            .requests
            .iter()
            .any(|(_, public, _)| snapshot.requests.iter().any(|r| &r.request_id == public));
        if waiting && !has_request && self.fallback.is_none() {
            // Older app-servers can expose the explicit waiting flag without
            // replaying request details. This is evidence of permission, not
            // an inference from elapsed time. Details remain in the terminal.
            self.fallback = hub
                .add(
                    key,
                    Detail {
                        summary: "The backend is waiting for permission".into(),
                        tool: "Backend permission request".into(),
                        scope: "Review the action and requested permissions in the agent terminal"
                            .into(),
                        command: None,
                        cwd: None,
                        started: None,
                    },
                )
                .map(|r| r.request_id);
        }
        if kind == "idle"
            || (kind == "active"
                && status["activeFlags"].as_array().is_some_and(|flags| {
                    !flags
                        .iter()
                        .any(|f| f == "waitingOnApproval" || f == "waitingOnUserInput")
                }))
        {
            // Authoritative backend state after reconnect can acknowledge a
            // response whose notification was lost. Silence cannot.
            for (_, public, _) in &self.requests {
                hub.resolve(public, ApprovalOutcome::Resolved);
            }
            if let Some(id) = self.fallback.take() {
                hub.resolve(&id, ApprovalOutcome::Resolved);
            }
        }
    }
}

/// Separate observer connection: never emits a response to a server request,
/// never starts/steers a turn, and resumes with no model/permission overrides.
struct Connection {
    socket: tungstenite::WebSocket<UnixStream>,
    next: u64,
    queued: Vec<Value>,
}
impl Connection {
    fn open(path: &PathBuf) -> Result<Self> {
        let stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        let (socket, _) = tungstenite::client("ws://localhost/", stream)
            .map_err(|e| anyhow::anyhow!("approval observer handshake: {e}"))?;
        let mut this = Self {
            socket,
            next: 0,
            queued: Vec::new(),
        };
        this.call("initialize", json!({"clientInfo":{"name":"omar-approvals","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}))?;
        this.send(json!({"method":"initialized"}))?;
        Ok(this)
    }
    fn send(&mut self, value: Value) -> Result<()> {
        self.socket
            .send(tungstenite::Message::Text(value.to_string()))?;
        Ok(())
    }
    fn read(&mut self) -> Result<Option<Value>> {
        match self.socket.read() {
            Ok(tungstenite::Message::Text(body)) => Ok(serde_json::from_str(&body).ok()),
            Ok(tungstenite::Message::Close(_)) => bail!("approval connection closed"),
            Ok(_) => Ok(None),
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }
    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next += 1;
        // String namespace cannot collide with the backend's numeric request
        // ids. A server request may arrive before our call's response.
        let id = format!("omar-approval-{}", self.next);
        self.send(json!({"id":id,"method":method,"params":params}))?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(value) = self.read()? {
                if value.get("method").is_none() && value["id"] == id {
                    if value.get("error").is_some() {
                        bail!("approval subscription unavailable");
                    }
                    return Ok(value["result"].clone());
                }
                if self.queued.len() >= 512 {
                    bail!("approval event backlog exceeded");
                }
                self.queued.push(value);
            }
        }
        bail!("approval subscription timed out")
    }
}

fn monitor(hub: ApprovalHub, key: String, session: String, fixed_socket: Option<PathBuf>) {
    let client = crate::tmux::TmuxClient::new("");
    let mut tracker = Tracker::default();
    let started = Instant::now();
    while hub.active(&key) {
        let socket = fixed_socket.clone().or_else(|| {
            client
                .session_delivery(&session)
                .and_then(|s| s.strip_prefix("codex:").map(PathBuf::from))
        });
        let result = (|| -> Result<()> {
            let socket = socket.context("no approval channel")?;
            let mut connection = Connection::open(&socket)?;
            let listed = connection.call("thread/loaded/list", json!({}))?;
            let ids = listed["data"].as_array().context("no loaded thread list")?;
            if ids.len() != 1 {
                bail!("pane does not have exactly one loaded thread");
            }
            let id = ids[0].as_str().context("invalid thread id")?.to_string();
            if tracker.thread != id {
                if let Some(public) = &tracker.fallback {
                    hub.resolve(public, ApprovalOutcome::Cancelled);
                }
                for (_, public, _) in &tracker.requests {
                    hub.resolve(public, ApprovalOutcome::Cancelled);
                }
                tracker = Tracker {
                    thread: id.clone(),
                    ..Tracker::default()
                };
            }
            let read =
                connection.call("thread/read", json!({"threadId":id,"includeTurns":true}))?;
            tracker.reconcile(&hub, &key, &read["thread"]);
            // Resume subscribes this connection and replays unresolved server
            // requests. Omitted options preserve the pane's configuration.
            connection.call("thread/resume", json!({"threadId":id,"excludeTurns":true}))?;
            for event in connection.queued.drain(..) {
                tracker.event(&hub, &key, &event);
            }
            hub.connection(&key, ApprovalConnection::Connected);
            let mut checked = Instant::now();
            while hub.active(&key) {
                if let Some(event) = connection.read()? {
                    tracker.event(&hub, &key, &event);
                }
                if checked.elapsed() >= Duration::from_secs(5) {
                    let listed = connection.call("thread/loaded/list", json!({}))?;
                    if listed["data"] != json!([id]) {
                        bail!("pane thread changed");
                    }
                    let read = connection.call("thread/read", json!({"threadId":id}))?;
                    for event in connection.queued.drain(..) {
                        tracker.event(&hub, &key, &event);
                    }
                    tracker.reconcile(&hub, &key, &read["thread"]);
                    checked = Instant::now();
                }
            }
            Ok(())
        })();
        if result.is_err() {
            let status = if !tracker.thread.is_empty() {
                ApprovalConnection::Disconnected
            } else if started.elapsed() > Duration::from_secs(90) {
                ApprovalConnection::Unsupported
            } else {
                ApprovalConnection::Connecting
            };
            hub.connection(&key, status);
        }
        for _ in 0..10 {
            if !hub.active(&key) {
                return;
            }
            thread::sleep(Duration::from_millis(200));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (ApprovalHub, Tracker) {
        let hub = ApprovalHub::default();
        for key in ["one", "two"] {
            hub.0.lock().unwrap().watches.insert(
                key.into(),
                Watch {
                    monitor: ApprovalMonitor {
                        agent_id: format!("agent::{key}"),
                        run_id: Some("run".into()),
                        state: ApprovalConnection::Connected,
                    },
                    name: key.into(),
                    invocation: Some(format!("invocation-{key}")),
                },
            );
        }
        (
            hub,
            Tracker {
                thread: "thread".into(),
                ..Tracker::default()
            },
        )
    }

    fn command() -> Value {
        json!({"id":7,"method":"item/commandExecution/requestApproval","params":{"threadId":"thread","turnId":"turn","itemId":"test","startedAtMs":1000,"command":"python3 -m unittest discover -s tests -v","cwd":"/workspace","reason":"Run the backend tests"}})
    }

    #[test]
    fn requests_are_scoped_deduplicated_and_kept_until_backend_ack() {
        let (hub, mut tracker) = fixture();
        tracker.event(&hub, "one", &command());
        tracker.event(&hub, "one", &command());
        let request = hub.snapshot().requests[0].clone();
        assert_eq!(hub.snapshot().requests.len(), 1);
        assert_eq!(request.invocation_id.as_deref(), Some("invocation-one"));
        assert_eq!(request.requested_at, 1000);
        let mut other = Tracker {
            thread: "thread".into(),
            ..Tracker::default()
        };
        other.event(&hub, "two", &command());
        assert_ne!(hub.snapshot().requests[1].request_id, request.request_id);
        hub.connection("one", ApprovalConnection::Disconnected);
        assert_eq!(hub.snapshot().requests.len(), 2);
        tracker.event(&hub, "one", &json!({"method":"serverRequest/resolved","params":{"threadId":"another-thread","requestId":7}}));
        assert_eq!(hub.snapshot().requests.len(), 2);
        tracker.event(&hub, "one", &json!({"method":"serverRequest/resolved","params":{"threadId":"thread","requestId":7}}));
        assert_eq!(hub.snapshot().requests.len(), 1);
        assert_eq!(hub.snapshot().requests[0].agent_name, "two");
        tracker.event(&hub, "one", &command());
        assert_eq!(
            hub.snapshot().requests.len(),
            1,
            "replay must not resurrect an acknowledged request"
        );
        assert_eq!(hub.snapshot().recent[0].outcome, ApprovalOutcome::Resolved);
    }

    #[test]
    fn ordinary_questions_and_silence_are_not_permission_requests() {
        let (hub, mut tracker) = fixture();
        tracker.event(&hub, "one", &json!({"id":1,"method":"item/tool/requestUserInput","params":{"threadId":"thread","questions":[{"id":"question","question":"Which database?"}]}}));
        tracker.reconcile(
            &hub,
            "one",
            &json!({"status":{"type":"active","activeFlags":[]}}),
        );
        tracker.reconcile(
            &hub,
            "one",
            &json!({"status":{"type":"active","activeFlags":["waitingOnUserInput"]}}),
        );
        assert!(hub.snapshot().requests.is_empty());
    }

    #[test]
    fn mcp_approval_explains_the_output_without_exposing_arguments() {
        let (hub, mut tracker) = fixture();
        tracker.event(&hub, "one", &json!({"method":"item/started","params":{"threadId":"thread","item":{"id":"mcp","type":"mcpToolCall","server":"omar","tool":"omar_set_port","arguments":{"port":"contract_milestone","value":"SECRET_VALUE"}}}}));
        tracker.event(&hub, "one", &json!({"id":2,"method":"item/tool/requestUserInput","params":{"threadId":"thread","itemId":"mcp","questions":[{"id":"mcp_tool_call_approval_123"}]}}));
        let snapshot = hub.snapshot();
        assert_eq!(snapshot.requests[0].tool_name, "omar_set_port");
        assert_eq!(
            snapshot.requests[0].scope,
            "This workflow's contract_milestone output"
        );
        assert!(!serde_json::to_string(&snapshot)
            .unwrap()
            .contains("SECRET_VALUE"));
        assert!(!serde_json::to_string(&tracker.items)
            .unwrap()
            .contains("SECRET_VALUE"));
    }

    #[test]
    fn denial_after_ack_is_not_reported_as_completion() {
        let (hub, mut tracker) = fixture();
        tracker.event(&hub, "one", &command());
        tracker.event(&hub, "one", &json!({"method":"serverRequest/resolved","params":{"threadId":"thread","requestId":7}}));
        tracker.event(&hub, "one", &json!({"method":"item/completed","params":{"threadId":"thread","item":{"id":"test","status":"declined"}}}));
        assert!(hub.snapshot().requests.is_empty());
        assert_eq!(hub.snapshot().recent[0].outcome, ApprovalOutcome::Denied);
    }

    #[test]
    fn mcp_elicitation_metadata_keeps_only_the_display_scope() {
        let (hub, mut tracker) = fixture();
        tracker.event(&hub, "one", &json!({"id":"elicitation","method":"mcpServer/elicitation/request","params":{"threadId":"thread","serverName":"omar","request":{"mode":"form","message":"Allow omar_set_port to publish the milestone?","_meta":{"codex_approval_kind":"mcp_tool_call","tool_name":"omar_set_port","tool_params":{"port":"contract_milestone","value":"PRIVATE_RESULT"}}}}}));
        let snapshot = hub.snapshot();
        assert_eq!(snapshot.requests[0].tool_name, "omar_set_port");
        assert_eq!(
            snapshot.requests[0].scope,
            "This workflow's contract_milestone output"
        );
        assert!(!serde_json::to_string(&snapshot)
            .unwrap()
            .contains("PRIVATE_RESULT"));
    }

    #[test]
    fn reconnect_reconciles_only_authoritative_state_and_run_end_cancels() {
        let (hub, mut tracker) = fixture();
        tracker.event(&hub, "one", &command());
        tracker.reconcile(&hub, "one", &json!({"status":{"type":"systemError"}}));
        assert_eq!(hub.snapshot().requests.len(), 1);
        tracker.reconcile(
            &hub,
            "one",
            &json!({"status":{"type":"active","activeFlags":["waitingOnApproval"]}}),
        );
        assert_eq!(hub.snapshot().requests.len(), 1);
        tracker.reconcile(&hub, "one", &json!({"status":{"type":"idle"}}));
        assert!(hub.snapshot().requests.is_empty());
        let mut next = command();
        next["id"] = json!(8);
        tracker.event(&hub, "one", &next);
        hub.finish_run("run");
        assert!(hub.snapshot().requests.is_empty());
        assert!(hub.snapshot().monitors.is_empty());
        assert_eq!(
            hub.snapshot().recent.last().unwrap().outcome,
            ApprovalOutcome::Cancelled
        );
    }

    #[test]
    fn commands_with_obvious_credentials_are_only_reviewed_in_terminal() {
        for value in [
            "curl -H 'Authorization: Bearer abc' example.org",
            "API_KEY=abc python3 test.py",
            "echo sk-secret",
        ] {
            assert_eq!(
                display_text(value),
                "Sensitive details hidden — review in the agent terminal"
            );
        }
        assert_eq!(display_text("python3 -m unittest"), "python3 -m unittest");
    }

    #[test]
    fn explicit_backend_waiting_flag_survives_reconnect_and_gains_replayed_details() {
        let (hub, mut tracker) = fixture();
        let waiting = json!({"status":{"type":"active","activeFlags":["waitingOnApproval"]}});
        tracker.reconcile(&hub, "one", &waiting);
        assert_eq!(hub.snapshot().requests.len(), 1);
        assert_eq!(
            hub.snapshot().requests[0].tool_name,
            "Backend permission request"
        );
        hub.connection("one", ApprovalConnection::Disconnected);
        tracker.reconcile(&hub, "one", &waiting);
        assert_eq!(hub.snapshot().requests.len(), 1);
        tracker.event(&hub, "one", &command());
        assert_eq!(hub.snapshot().requests.len(), 1);
        assert_eq!(hub.snapshot().requests[0].tool_name, "Command execution");
        assert!(
            hub.snapshot().recent.is_empty(),
            "gaining details is not an approval acknowledgement"
        );
    }

    #[test]
    fn websocket_observer_preserves_replayed_requests_and_sends_no_decisions() {
        use std::os::unix::net::UnixListener;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("approval.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            let mut methods = Vec::new();
            loop {
                let tungstenite::Message::Text(raw) = socket.read().unwrap() else {
                    continue;
                };
                let value: Value = serde_json::from_str(&raw).unwrap();
                let method = value["method"]
                    .as_str()
                    .expect("observer must not send approval responses")
                    .to_string();
                methods.push(method.clone());
                if method == "initialized" {
                    continue;
                }
                if method == "thread/resume" {
                    assert_eq!(
                        value["params"],
                        json!({"threadId":"thread","excludeTurns":true})
                    );
                    socket
                        .send(tungstenite::Message::Text(command().to_string()))
                        .unwrap();
                }
                socket
                    .send(tungstenite::Message::Text(
                        json!({"id":value["id"],"result":{}}).to_string(),
                    ))
                    .unwrap();
                if method == "thread/resume" {
                    return methods;
                }
            }
        });
        let mut connection = Connection::open(&path).unwrap();
        connection
            .call(
                "thread/resume",
                json!({"threadId":"thread","excludeTurns":true}),
            )
            .unwrap();
        let (hub, mut tracker) = fixture();
        for event in connection.queued {
            tracker.event(&hub, "one", &event);
        }
        assert_eq!(hub.snapshot().requests.len(), 1);
        assert_eq!(
            server.join().unwrap(),
            ["initialize", "initialized", "thread/resume"]
        );
    }
}
