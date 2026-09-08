//! Evidence-only invocation telemetry. No terminal scraping, reasoning, tool
//! arguments, command output, or filesystem-wide attribution enters this API.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use ts_rs::TS;

use crate::channel::CodexSession;
use crate::tmux::TmuxClient;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActivityExecution {
    Running,
    Completed,
    Failed,
}
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActivityConnection {
    Connecting,
    Connected,
    Disconnected,
    Unsupported,
}
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct ActivityEvent {
    pub id: String,
    pub at: u64,
    // Backend lifecycle timestamp when present; otherwise server observation time.
    pub time_source: String,
    pub kind: String,
    pub summary: String,
    pub tool_id: Option<String>,
    pub exit_code: Option<i64>,
}
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct ObservedTool {
    pub id: String,
    pub summary: String,
    pub started_at: Option<u64>,
    pub observed_at: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct ChangedArtifact {
    pub id: String,
    pub path: String,
    pub change: String,
    pub observed_at: u64,
    // A bounded, redacted patch from this agent's successful file-change item.
    pub diff: Option<String>,
    pub diff_truncated: bool,
    pub verification: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq, Eq)]
pub struct RoleSelection {
    pub model: Option<String>,
    pub effort: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct InvocationActivity {
    pub invocation_id: String,
    pub reaction_id: String,
    pub agent_name: String,
    pub backend: String,
    pub started_at: u64,
    pub finished_at: Option<u64>,
    pub execution: ActivityExecution,
    pub connection: ActivityConnection,
    pub last_activity_at: u64,
    pub events: Vec<ActivityEvent>,
    pub active_tools: Vec<ObservedTool>,
    pub artifacts: Vec<ChangedArtifact>,
    pub requested: RoleSelection,
    // Only per-invocation backend execution telemetry can confirm these.
    pub confirmed: RoleSelection,
    // thread/read reports configuration, not per-turn execution telemetry.
    pub reported_thread_settings: RoleSelection,
    pub settings_application: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct ActivitySnapshot {
    pub run_id: String,
    pub sequence: u64,
    pub server_time: u64,
    pub invocations: Vec<InvocationActivity>,
}
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct SupportedModel {
    pub model: String,
    pub name: String,
    pub efforts: Vec<String>,
    pub default_effort: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct SavedRoleSettings {
    pub team: String,
    pub agent: String,
    pub backend: String,
    pub selection: RoleSelection,
}
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct RoleSettingsSnapshot {
    pub roles: Vec<SavedRoleSettings>,
    pub codex_models: Vec<SupportedModel>,
    pub capabilities_available: bool,
}

#[derive(Clone)]
pub struct RoleSettings {
    roles: Arc<Mutex<Vec<SavedRoleSettings>>>,
    path: PathBuf,
}
impl RoleSettings {
    pub fn load(path: PathBuf) -> Self {
        let roles = std::fs::read(&path)
            .ok()
            .and_then(|v| serde_json::from_slice(&v).ok())
            .unwrap_or_default();
        Self {
            roles: Arc::new(Mutex::new(roles)),
            path,
        }
    }
    pub fn all(&self) -> Vec<SavedRoleSettings> {
        self.roles.lock().expect("role settings poisoned").clone()
    }
    pub fn requested(&self, team: &str, agent: &str, backend: &str) -> RoleSelection {
        self.all()
            .into_iter()
            .find(|r| r.team == team && r.agent == agent && r.backend == backend)
            .map(|r| r.selection)
            .unwrap_or_default()
    }
    pub fn save(&self, role: SavedRoleSettings, models: &[SupportedModel]) -> Result<()> {
        anyhow::ensure!(
            !role.team.is_empty()
                && role.team.len() <= 256
                && !role.agent.is_empty()
                && role.agent.len() <= 256,
            "invalid role identity"
        );
        anyhow::ensure!(
            role.backend == "codex",
            "this backend does not support model settings through Omar"
        );
        validate_selection(&role.selection, models)?;
        let mut roles = self.roles.lock().expect("role settings poisoned");
        let mut next = roles.clone();
        next.retain(|r| {
            !(r.team == role.team && r.agent == role.agent && r.backend == role.backend)
        });
        next.push(role);
        write_private_json(&self.path, &next)?;
        *roles = next;
        Ok(())
    }
}
fn write_private_json(path: &Path, value: &impl Serialize) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::create_dir_all(path.parent().context("missing parent")?)?;
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
fn validate_selection(selection: &RoleSelection, models: &[SupportedModel]) -> Result<()> {
    let Some(model) = &selection.model else {
        anyhow::ensure!(
            selection.effort.is_none(),
            "select a model before an effort"
        );
        return Ok(());
    };
    let supported = models
        .iter()
        .find(|m| &m.model == model)
        .context("model is not currently supported")?;
    if let Some(effort) = &selection.effort {
        anyhow::ensure!(
            supported.efforts.contains(effort),
            "effort is not supported by this model"
        );
    }
    Ok(())
}
pub fn supported_models(session: &mut CodexSession) -> Result<Vec<SupportedModel>> {
    let mut result = Vec::new();
    let mut cursor = Value::Null;
    // Bound a misbehaving/paginating server; never present a partial catalog.
    for _ in 0..20 {
        let page = session.call("model/list", json!({"cursor": cursor, "limit": 100}))?;
        for model in page["data"].as_array().context("missing model catalog")? {
            if model["hidden"].as_bool() == Some(true) {
                continue;
            }
            let Some(id) = model["model"].as_str().filter(|s| safe_identifier(s)) else {
                continue;
            };
            let efforts = model["supportedReasoningEfforts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|e| e["reasoningEffort"].as_str())
                .filter(|s| safe_identifier(s))
                .map(str::to_string)
                .collect();
            result.push(SupportedModel {
                model: id.into(),
                name: id.into(),
                efforts,
                default_effort: model["defaultReasoningEffort"]
                    .as_str()
                    .filter(|s| safe_identifier(s))
                    .map(str::to_string),
            });
        }
        cursor = page["nextCursor"].clone();
        if cursor.is_null() {
            return Ok(result);
        }
    }
    bail!("model catalog pagination did not finish")
}
pub fn codex_socket(client: &TmuxClient, agent: &str) -> Option<PathBuf> {
    client
        .session_delivery(&client.session_for(agent))?
        .strip_prefix("codex:")
        .map(PathBuf::from)
}
pub fn models_from_socket(path: &Path) -> Result<Vec<SupportedModel>> {
    supported_models(&mut CodexSession::open(path)?)
}

#[derive(Clone)]
pub struct ActivityRun {
    snapshot: Arc<Mutex<ActivitySnapshot>>,
    pub settings: RoleSettings,
    pub team: String,
    pub backends: BTreeMap<String, String>,
    pub workdir: PathBuf,
}
impl ActivityRun {
    pub fn new(
        run_id: String,
        team: String,
        backends: BTreeMap<String, String>,
        settings: RoleSettings,
        workdir: PathBuf,
    ) -> Self {
        Self {
            snapshot: Arc::new(Mutex::new(ActivitySnapshot {
                run_id,
                sequence: 0,
                server_time: now_ms(),
                invocations: Vec::new(),
            })),
            settings,
            team,
            backends,
            workdir,
        }
    }
    pub fn snapshot(&self) -> ActivitySnapshot {
        let mut snapshot = self.snapshot.lock().expect("activity poisoned").clone();
        snapshot.server_time = now_ms();
        snapshot
    }
    pub fn start(&self, id: &str, reaction: &str, agent: &str) {
        let mut snapshot = self.snapshot.lock().expect("activity poisoned");
        if snapshot.invocations.iter().any(|i| i.invocation_id == id) {
            return;
        }
        let at = now_ms();
        let backend = self.backends.get(agent).cloned().unwrap_or_default();
        let requested = self.settings.requested(&self.team, agent, &backend);
        snapshot.invocations.push(InvocationActivity {
            invocation_id: id.into(), reaction_id: reaction.into(), agent_name: agent.into(),
            connection: if backend == "codex" { ActivityConnection::Connecting } else { ActivityConnection::Unsupported },
            backend, started_at: at, finished_at: None, execution: ActivityExecution::Running,
            last_activity_at: at, events: vec![ActivityEvent { id: format!("{id}:start"), at,
                time_source: "runtime".into(), kind: "invocation_started".into(), summary: "Invocation started".into(), tool_id: None, exit_code: None }],
            active_tools: Vec::new(), artifacts: Vec::new(), requested,
            confirmed: RoleSelection::default(), reported_thread_settings: RoleSelection::default(), settings_application: "No override; keeps current backend configuration; active execution settings not reported".into(),
        });
        // Bounded replay, preserving running invocations even for large teams.
        if snapshot.invocations.len() > 100 {
            if let Some(index) = snapshot
                .invocations
                .iter()
                .position(|i| i.finished_at.is_some())
            {
                snapshot.invocations.remove(index);
            }
        }
        snapshot.sequence += 1;
    }
    fn update(&self, id: &str, f: impl FnOnce(&mut InvocationActivity)) {
        let mut snapshot = self.snapshot.lock().expect("activity poisoned");
        let Some(invocation) = snapshot
            .invocations
            .iter_mut()
            .find(|i| i.invocation_id == id)
        else {
            return;
        };
        // Late events must never reopen an invocation, overwrite its outcome,
        // or attach another invocation's artifacts to it.
        if invocation.finished_at.is_some() {
            return;
        }
        f(invocation);
        snapshot.sequence += 1;
    }
    pub fn finish(&self, id: &str, success: bool, effects: usize) {
        self.finish_at(id, success, effects, now_ms());
    }
    pub fn finish_at(&self, id: &str, success: bool, effects: usize, at: u64) {
        self.update(id, |i| {
            i.execution = if success { ActivityExecution::Completed } else { ActivityExecution::Failed };
            i.finished_at = Some(at);
            i.active_tools.clear();
            event(i, ActivityEvent { id: format!("{id}:end"), at, time_source: "runtime".into(),
                kind: "invocation_ended".into(), summary: if success { format!("Invocation completed · {effects} workflow effects; file verification is separate") } else { "Invocation failed · inspect the agent terminal".into() }, tool_id: None, exit_code: None });
        });
    }
    pub fn connection(&self, id: &str, state: ActivityConnection) {
        self.update(id, |i| i.connection = state);
    }
    pub fn active(&self, id: &str) -> bool {
        self.snapshot
            .lock()
            .expect("activity poisoned")
            .invocations
            .iter()
            .any(|i| i.invocation_id == id && i.finished_at.is_none())
    }
    pub fn requested(&self, id: &str) -> RoleSelection {
        self.snapshot()
            .invocations
            .into_iter()
            .find(|i| i.invocation_id == id)
            .map(|i| i.requested)
            .unwrap_or_default()
    }
    pub fn application(&self, id: &str, text: &str) {
        self.update(id, |i| i.settings_application = text.into());
    }
}
fn event(i: &mut InvocationActivity, e: ActivityEvent) {
    if i.events.iter().any(|old| old.id == e.id) {
        return;
    }
    i.last_activity_at = i.last_activity_at.max(e.at);
    i.events.push(e);
    i.events
        .sort_by(|a, b| a.at.cmp(&b.at).then(a.id.cmp(&b.id)));
    if i.events.len() > 200 {
        i.events.remove(0);
    }
}
fn safe_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-/.:".contains(c))
}
fn tool_summary(item: &Value) -> Option<String> {
    match item["type"].as_str()? {
        "commandExecution" => {
            // Display categories, never command text, shell assignments or args.
            let read = item["commandActions"]
                .as_array()
                .is_some_and(|a| !a.is_empty() && a.iter().all(|a| a["type"] == "read"));
            Some(
                if read {
                    "Reading files"
                } else {
                    "Running command"
                }
                .into(),
            )
        }
        "fileChange" => Some("Editing files".into()),
        "mcpToolCall" => Some(
            item["tool"]
                .as_str()
                .filter(|s| safe_identifier(s))
                .map(|s| format!("Tool: {s}"))
                .unwrap_or_else(|| "Running tool".into()),
        ),
        "webSearch" => Some("Searching the web".into()),
        "dynamicToolCall" => Some("Running tool".into()),
        _ => None,
    }
}
fn scoped_path(path: &str, root: &Path) -> Option<String> {
    let path = Path::new(path);
    let relative = if path.is_absolute() {
        path.strip_prefix(root).ok()?
    } else {
        path
    };
    if relative
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return None;
    }
    // A lexically in-workspace path can traverse a symlink into credentials
    // elsewhere. File contents are never read; conservatively omit symlinks.
    let mut checked = root.to_path_buf();
    for component in relative.components() {
        checked.push(component);
        if std::fs::symlink_metadata(&checked).is_ok_and(|m| m.file_type().is_symlink()) {
            return None;
        }
    }
    let text = relative.to_str()?;
    let lower = text.to_ascii_lowercase();
    if text.len() > 512
        || text.chars().any(char::is_control)
        || lower.split('/').any(|p| {
            p.starts_with('.')
                || [
                    "credentials",
                    "credentials.json",
                    "secrets",
                    "auth.json",
                    "id_rsa",
                    "id_ed25519",
                ]
                .contains(&p)
        })
        || [".pem", ".key", ".p12", ".pfx", ".env"]
            .iter()
            .any(|ext| lower.ends_with(ext))
    {
        return None;
    }
    Some(text.into())
}
fn redacted_diff(diff: &str) -> (String, bool) {
    let mut output = String::new();
    let mut truncated = false;
    let secret = regex::Regex::new(r"(?i)(api.?key|secret|password|passwd|authorization|bearer\s|access.?token|refresh.?token|\btoken\b|private.?key|sk-[a-z0-9]|gh[pousr]_|AKIA[0-9A-Z])").expect("constant regex");
    let mut private_key = false;
    for line in diff.lines() {
        if line.contains("-----BEGIN ") && line.contains("PRIVATE KEY-----") {
            private_key = true;
        }
        let redact = private_key || secret.is_match(line);
        if line.contains("-----END ") && line.contains("PRIVATE KEY-----") {
            private_key = false;
        }
        let line = if redact {
            "[sensitive line redacted]"
        } else {
            line
        };
        if output.len() + line.len() > 16_384 {
            truncated = true;
            break;
        }
        output.extend(line.chars().filter(|c| !c.is_control() || *c == '\t'));
        output.push('\n');
    }
    (output, truncated)
}

/// Keeps backend turn identity separate from the runtime invocation. A pane
/// may have bootstrap turns; history is never attributed merely by proximity.
struct Capture {
    run: ActivityRun,
    invocation: String,
    thread: String,
    baseline: BTreeSet<String>,
    turns: BTreeSet<String>,
    finished_items: BTreeSet<String>,
    started_items: BTreeSet<String>,
}
impl Capture {
    fn accept_turn(&mut self, turn: &Value) -> bool {
        let Some(id) = turn["id"].as_str() else {
            return false;
        };
        if self.turns.contains(id) {
            return true;
        }
        if self.baseline.contains(id) {
            return false;
        }
        // The delivered user item carries the runtime's opaque invocation ID.
        // Time alone cannot attribute bootstrap/manual turns in the same pane.
        let marker = format!("invocation_id: {}", self.invocation);
        if turn["items"].as_array().into_iter().flatten().any(|item| {
            item["type"] == "userMessage"
                && item["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|part| {
                        part["text"]
                            .as_str()
                            .is_some_and(|text| text.lines().any(|line| line.trim() == marker))
                    })
        }) {
            self.turns.insert(id.into());
            return true;
        }
        false
    }
    fn notification(&mut self, value: &Value) {
        let p = &value["params"];
        if p["threadId"].as_str() != Some(&self.thread) {
            return;
        }
        match value["method"].as_str().unwrap_or("") {
            "turn/started" | "turn/completed" => {
                self.accept_turn(&p["turn"]);
            }
            "item/started" | "item/completed" => {
                // Live user-item events also establish attribution when the
                // backend can subscribe but cannot hydrate full history.
                self.accept_turn(&json!({"id":p["turnId"], "items":[p["item"].clone()]}));
                let Some(turn) = p["turnId"].as_str().filter(|id| self.turns.contains(*id)) else {
                    return;
                };
                let completed = value["method"] == "item/completed";
                let at = p[if completed {
                    "completedAtMs"
                } else {
                    "startedAtMs"
                }]
                .as_u64();
                self.item(turn, &p["item"], completed, at, false);
            }
            // Reasoning and raw deltas intentionally have no path into the API.
            _ => {}
        }
    }
    fn reconcile(&mut self, thread: &Value) {
        self.run.update(&self.invocation, |i| {
            i.reported_thread_settings = RoleSelection {
                model: thread["model"]
                    .as_str()
                    .filter(|s| safe_identifier(s))
                    .map(str::to_string),
                effort: thread["reasoningEffort"]
                    .as_str()
                    .filter(|s| safe_identifier(s))
                    .map(str::to_string),
            };
        });
        for turn in thread["turns"].as_array().into_iter().flatten() {
            if !self.accept_turn(turn) {
                continue;
            }
            let id = turn["id"].as_str().unwrap_or_default();
            for item in turn["items"].as_array().into_iter().flatten() {
                let status = item["status"].as_str().unwrap_or("");
                if matches!(status, "inProgress" | "completed" | "failed" | "declined") {
                    self.item(id, item, status != "inProgress", None, true);
                }
            }
            if matches!(turn["status"].as_str(), Some("failed" | "interrupted")) {
                self.run.update(&self.invocation, |i| event(i, ActivityEvent {
                    id: format!("{id}:error"), at: now_ms(), time_source: "observed".into(), kind: "error".into(), summary: "Backend turn ended with an error or interruption · inspect terminal".into(), tool_id: None, exit_code: None,
                }));
            }
        }
    }
    fn item(
        &mut self,
        turn: &str,
        item: &Value,
        completed: bool,
        at: Option<u64>,
        recovered: bool,
    ) {
        let Some(summary) = tool_summary(item) else {
            return;
        };
        let Some(item_id) = item["id"].as_str() else {
            return;
        };
        let key = format!("{}:{turn}:{item_id}", self.thread);
        if completed {
            if !self.finished_items.insert(key.clone()) {
                return;
            }
        } else if self.finished_items.contains(&key) || !self.started_items.insert(key.clone()) {
            return;
        }
        let root = &self.run.workdir;
        self.run.update(&self.invocation, |i| {
            let observed = now_ms();
            if completed {
                i.active_tools.retain(|tool| tool.id != key);
            } else if !i.active_tools.iter().any(|tool| tool.id == key) {
                i.active_tools.push(ObservedTool {
                    id: key.clone(),
                    summary: summary.clone(),
                    started_at: at,
                    observed_at: observed,
                });
            }
            let failed = !item["error"].is_null()
                || item["result"]["isError"] == true
                || matches!(item["status"].as_str(), Some("failed" | "declined"))
                || item["exitCode"].as_i64().is_some_and(|c| c != 0);
            let description = if completed {
                format!("{summary} · {}", if failed { "failed" } else { "finished" })
            } else {
                summary
            };
            event(
                i,
                ActivityEvent {
                    id: format!("{key}:{}", if completed { "end" } else { "start" }),
                    at: at.unwrap_or(observed),
                    time_source: if at.is_some() {
                        "backend"
                    } else if recovered {
                        "recovered"
                    } else {
                        "observed"
                    }
                    .into(),
                    kind: if failed {
                        "error"
                    } else if completed {
                        "tool_finished"
                    } else {
                        "tool_started"
                    }
                    .into(),
                    summary: description,
                    tool_id: Some(key.clone()),
                    exit_code: item["exitCode"].as_i64(),
                },
            );
            if completed && !failed && item["type"] == "fileChange" && item["status"] == "completed"
            {
                for change in item["changes"].as_array().into_iter().flatten().take(100) {
                    let Some(path) = change["path"].as_str().and_then(|s| scoped_path(s, root))
                    else {
                        continue;
                    };
                    let id = format!("{key}:{path}");
                    if i.artifacts.iter().any(|a| a.id == id) {
                        continue;
                    }
                    let (diff, diff_truncated) =
                        redacted_diff(change["diff"].as_str().unwrap_or_default());
                    let kind = match change["kind"]["type"].as_str() {
                        Some("add") => "created",
                        Some("delete") => "deleted",
                        _ => "modified",
                    };
                    i.artifacts.push(ChangedArtifact {
                        id,
                        path,
                        change: kind.into(),
                        observed_at: at.unwrap_or(observed),
                        diff: (!diff.is_empty()).then_some(diff),
                        diff_truncated,
                        verification:
                            "Not verified by Omar; inspect check outcomes in the timeline".into(),
                    });
                    if i.artifacts.len() > 30 {
                        i.artifacts.remove(0);
                    }
                }
            }
        });
    }
}

/// New or paginated threads may support metadata while refusing full
/// history. Keep configuration and connectivity usable in that case.
fn read_activity(session: &mut CodexSession, thread: &str) -> Result<(Value, bool)> {
    match session.call(
        "thread/read",
        json!({"threadId":thread, "includeTurns":true}),
    ) {
        Ok(read) => Ok((read, true)),
        Err(_) => session
            .call(
                "thread/read",
                json!({"threadId":thread, "includeTurns":false}),
            )
            .map(|read| (read, false)),
    }
}

/// One monitor per active invocation. Read-only except an explicitly requested
/// next-invocation model/effort delivered with turn/start by `deliver_configured`.
pub struct InvocationMonitor {
    run: ActivityRun,
    id: String,
    socket: Option<PathBuf>,
    source: Option<(TmuxClient, String)>,
    capture: Option<Capture>,
    session: Option<CodexSession>,
    subscribed: bool,
}
impl InvocationMonitor {
    pub fn prepare(run: ActivityRun, id: &str, client: &TmuxClient, agent: &str) -> Self {
        let mut monitor = Self {
            run,
            id: id.into(),
            socket: codex_socket(client, agent),
            source: Some((client.clone(), agent.into())),
            capture: None,
            session: None,
            subscribed: false,
        };
        if monitor
            .run
            .backends
            .get(agent)
            .is_some_and(|b| b == "codex")
            && monitor.connect(true).is_err()
        {
            monitor.run.connection(id, ActivityConnection::Disconnected);
        }
        monitor
    }
    fn connect(&mut self, baseline: bool) -> Result<()> {
        if let Some((client, agent)) = &self.source {
            self.socket = codex_socket(client, agent);
        }
        let mut session =
            CodexSession::open(self.socket.as_deref().context("no app-server socket")?)?;
        let thread_id = session.only_thread()?;
        let (read, history_available) = read_activity(&mut session, &thread_id)?;
        if self.capture.as_ref().is_some_and(|c| c.thread != thread_id) {
            bail!("pane changed threads; activity attribution unavailable");
        }
        if self.capture.is_none() {
            self.capture = Some(Capture {
                run: self.run.clone(),
                invocation: self.id.clone(),
                thread: thread_id.clone(),
                baseline: if baseline {
                    read["thread"]["turns"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|t| t["id"].as_str().map(str::to_string))
                        .collect()
                } else {
                    BTreeSet::new()
                },
                turns: BTreeSet::new(),
                finished_items: BTreeSet::new(),
                started_items: BTreeSet::new(),
            });
        }
        // No overrides: this only subscribes to the pane's existing thread.
        self.subscribed = session
            .call(
                "thread/resume",
                json!({"threadId": thread_id, "excludeTurns": true}),
            )
            .is_ok();
        if !self.subscribed {
            // A history capability refusal is not a lost connection. Verify
            // the metadata channel still works before retaining this session.
            session.call(
                "thread/read",
                json!({"threadId": thread_id, "includeTurns": false}),
            )?;
        }
        let capture = self.capture.as_mut().unwrap();
        for value in session.take_notifications() {
            capture.notification(&value);
        }
        capture.reconcile(&read["thread"]);
        self.run.connection(
            &self.id,
            if history_available || self.subscribed {
                ActivityConnection::Connected
            } else {
                ActivityConnection::Unsupported
            },
        );
        self.session = Some(session);
        Ok(())
    }
    pub fn deliver_configured(&mut self, message: &str) -> Result<bool> {
        let requested = self.run.requested(&self.id);
        if requested.model.is_none() {
            return Ok(false);
        }
        let session = self.session.as_mut().context(
            "requested settings require a connected app-server; invocation was not sent",
        )?;
        let capture = self.capture.as_mut().context("missing monitor")?;
        let models = supported_models(session)?;
        validate_selection(&requested, &models)?;
        let effort = requested.effort.clone().or_else(|| {
            models
                .iter()
                .find(|m| Some(&m.model) == requested.model.as_ref())
                .and_then(|m| m.default_effort.clone())
        });
        let read = session.call(
            "thread/read",
            json!({"threadId": capture.thread, "includeTurns": false}),
        )?;
        anyhow::ensure!(
            read["thread"]["status"]["type"] == "idle",
            "agent is already active; next-invocation settings were not sent"
        );
        // Never retry a turn/start failure with terminal delivery: a lost reply
        // may already have started the turn. Nor change permission settings.
        let response = session.call("turn/start", json!({"threadId": capture.thread, "input": [{"type":"text", "text":message, "text_elements":[]}], "model": requested.model, "effort": effort}))?;
        let turn = response["turn"]["id"]
            .as_str()
            .context("turn/start did not return a turn id")?;
        capture.turns.insert(turn.into());
        self.run.application(
            &self.id,
            "Requested settings accepted by turn/start; active execution settings not reported",
        );
        Ok(true)
    }
    pub fn spawn(mut self) -> MonitorGuard {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped = stop.clone();
        if !self
            .run
            .snapshot()
            .invocations
            .iter()
            .any(|i| i.invocation_id == self.id && i.backend == "codex")
        {
            return MonitorGuard { stop, thread: None };
        }
        let thread = std::thread::spawn(move || {
            while self.run.active(&self.id) {
                let last = stopped.load(std::sync::atomic::Ordering::Acquire);
                if self.session.is_none() && self.connect(false).is_err() {
                    self.run
                        .connection(&self.id, ActivityConnection::Disconnected);
                    if last {
                        break;
                    }
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                }
                let session = self.session.as_mut().unwrap();
                let capture = self.capture.as_mut().unwrap();
                match read_activity(session, &capture.thread) {
                    Ok((read, history_available)) => {
                        if !self.subscribed && history_available {
                            self.subscribed = session
                                .call(
                                    "thread/resume",
                                    json!({"threadId": capture.thread, "excludeTurns":true}),
                                )
                                .is_ok();
                        }
                        for turn in read["thread"]["turns"].as_array().into_iter().flatten() {
                            capture.accept_turn(turn);
                        }
                        for value in session.take_notifications() {
                            capture.notification(&value);
                        }
                        capture.reconcile(&read["thread"]);
                        self.run.connection(
                            &self.id,
                            if history_available || self.subscribed {
                                ActivityConnection::Connected
                            } else {
                                ActivityConnection::Unsupported
                            },
                        );
                    }
                    Err(_) => {
                        self.session = None;
                        self.run
                            .connection(&self.id, ActivityConnection::Disconnected);
                    }
                }
                if last {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        });
        MonitorGuard {
            stop,
            thread: Some(thread),
        }
    }
}
pub struct MonitorGuard {
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Drop for MonitorGuard {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn run(root: &Path) -> ActivityRun {
        ActivityRun::new(
            "run".into(),
            "Team".into(),
            BTreeMap::from([
                ("one".into(), "codex".into()),
                ("two".into(), "codex".into()),
            ]),
            RoleSettings::load(root.join("roles.json")),
            root.into(),
        )
    }
    fn capture(run: &ActivityRun, id: &str) -> Capture {
        Capture {
            run: run.clone(),
            invocation: id.into(),
            thread: format!("thread-{id}"),
            baseline: BTreeSet::new(),
            turns: BTreeSet::from(["turn".into()]),
            finished_items: BTreeSet::new(),
            started_items: BTreeSet::new(),
        }
    }
    fn command(status: &str, exit: Value) -> Value {
        json!({"id":"same-item", "type":"commandExecution", "command":"SECRET=do-not-display command --token=hidden", "aggregatedOutput":"private output", "status":status, "exitCode":exit})
    }
    #[test]
    fn concurrent_agents_tools_disconnect_and_late_events_stay_independent() {
        let dir = tempfile::tempdir().unwrap();
        let run = run(dir.path());
        run.start("a", "r.0", "one");
        run.start("b", "r.1", "two");
        let mut a = capture(&run, "a");
        let mut b = capture(&run, "b");
        a.item(
            "turn",
            &command("inProgress", Value::Null),
            false,
            Some(1000),
            false,
        );
        b.item(
            "turn",
            &command("inProgress", Value::Null),
            false,
            Some(2000),
            false,
        );
        run.connection("a", ActivityConnection::Disconnected);
        assert_eq!(
            run.snapshot().invocations[0].active_tools[0].started_at,
            Some(1000)
        );
        // Snapshot/reload is non-destructive, and does not refresh last activity.
        let before = run.snapshot().invocations[0].last_activity_at;
        run.connection("a", ActivityConnection::Connected);
        assert_eq!(run.snapshot().invocations[0].last_activity_at, before);
        b.item(
            "turn",
            &command("completed", json!(0)),
            true,
            Some(3000),
            false,
        );
        run.finish("b", true, 1);
        let finished = run.snapshot().invocations[1].finished_at;
        b.item(
            "turn",
            &command("inProgress", Value::Null),
            false,
            Some(4000),
            false,
        );
        run.start("b", "r.1", "two");
        let snapshot = run.snapshot();
        assert_eq!(
            snapshot.invocations[0].execution,
            ActivityExecution::Running
        );
        assert_eq!(snapshot.invocations[0].active_tools.len(), 1);
        assert_eq!(snapshot.invocations[1].finished_at, finished);
        assert!(snapshot.invocations[1].active_tools.is_empty());
        assert_eq!(snapshot.invocations.len(), 2);
        assert!(!serde_json::to_string(&snapshot)
            .unwrap()
            .contains("do-not-display"));
        assert!(!serde_json::to_string(&snapshot)
            .unwrap()
            .contains("private output"));
    }
    #[test]
    fn duplicate_and_delayed_starts_do_not_reopen_finished_tools() {
        let dir = tempfile::tempdir().unwrap();
        let run = run(dir.path());
        run.start("a", "r", "one");
        let mut c = capture(&run, "a");
        c.item(
            "turn",
            &command("completed", json!(1)),
            true,
            Some(3000),
            false,
        );
        c.item(
            "turn",
            &command("completed", json!(1)),
            true,
            Some(3000),
            false,
        );
        c.item(
            "turn",
            &command("inProgress", Value::Null),
            false,
            Some(2000),
            false,
        );
        let i = &run.snapshot().invocations[0];
        assert!(i.active_tools.is_empty());
        assert_eq!(i.events.len(), 2);
        assert!(i
            .events
            .iter()
            .any(|e| e.kind == "error" && e.exit_code == Some(1)));
    }
    #[test]
    fn artifacts_need_successful_scoped_tool_evidence_and_redact_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let run = run(dir.path());
        run.start("a", "r", "one");
        let mut c = capture(&run, "a");
        let mut item = json!({"id":"edit","type":"fileChange", "status":"inProgress", "changes":[
            {"path":"src/api.rs","kind":{"type":"add"},"diff":"+ fn works() {}\n+ api_key=SECRET\n+ -----BEGIN PRIVATE KEY-----\n+ hidden-key\n+ -----END PRIVATE KEY-----\n"},
            {"path":".env","kind":{"type":"add"},"diff":"secret"},
            {"path":"../other.rs","kind":{"type":"update"},"diff":"outside"},
            {"path":"/etc/passwd","kind":{"type":"update"},"diff":"outside"}
        ]});
        c.item("turn", &item, false, Some(2000), false);
        assert!(run.snapshot().invocations[0].artifacts.is_empty());
        item["status"] = json!("completed");
        c.item("turn", &item, true, Some(3000), false);
        let snapshot = run.snapshot();
        let artifacts = &snapshot.invocations[0].artifacts;
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].change, "created");
        assert!(artifacts[0].diff.as_ref().unwrap().contains("fn works"));
        let serialized = serde_json::to_string(&snapshot).unwrap();
        for secret in ["SECRET", "hidden-key", "outside", ".env"] {
            assert!(!serialized.contains(secret));
        }
        assert!(artifacts[0].verification.starts_with("Not verified"));
    }
    #[test]
    fn recovery_only_attributes_turns_with_the_runtime_invocation_marker() {
        let dir = tempfile::tempdir().unwrap();
        let run = run(dir.path());
        run.start("a", "r", "one");
        let mut c = capture(&run, "a");
        let bootstrap = json!({"id":"bootstrap", "startedAt": now_ms()/1000, "items": [command("completed", json!(0))]});
        assert!(!c.accept_turn(&bootstrap));
        let matched = json!({"id":"matching", "items":[{"type":"userMessage","content":[{"type":"text","text":"OMAR INVOCATION\ninvocation_id: a\n"}]}, command("inProgress", Value::Null)]});
        c.reconcile(
            &json!({"turns":[bootstrap, matched], "model":"supported", "reasoningEffort":"high"}),
        );
        let i = &run.snapshot().invocations[0];
        assert_eq!(i.active_tools.len(), 1);
        assert_eq!(i.active_tools[0].started_at, None);
        assert!(i.events.iter().any(|e| e.time_source == "recovered"));
        assert_eq!(i.confirmed, RoleSelection::default());
        assert_eq!(
            i.reported_thread_settings.model.as_deref(),
            Some("supported")
        );
    }
    #[test]
    fn reasoning_and_ordinary_questions_have_no_activity_projection() {
        let dir = tempfile::tempdir().unwrap();
        let run = run(dir.path());
        run.start("a", "r", "one");
        let mut c = capture(&run, "a");
        for method in [
            "item/reasoning/textDelta",
            "item/tool/requestUserInput",
            "item/agentMessage/delta",
        ] {
            c.notification(&json!({"method":method,"params":{"threadId":"thread-a","turnId":"turn","text":"private reasoning"}}));
        }
        assert_eq!(run.snapshot().invocations[0].events.len(), 1);
    }
    #[test]
    fn requested_roles_persist_but_cannot_change_an_active_invocation() {
        let dir = tempfile::tempdir().unwrap();
        let run = run(dir.path());
        run.start("a", "r", "one");
        let models = vec![SupportedModel {
            model: "available".into(),
            name: "Available".into(),
            efforts: vec!["medium".into()],
            default_effort: Some("medium".into()),
        }];
        let role = SavedRoleSettings {
            team: "Team".into(),
            agent: "one".into(),
            backend: "codex".into(),
            selection: RoleSelection {
                model: Some("available".into()),
                effort: Some("medium".into()),
            },
        };
        run.settings.save(role.clone(), &models).unwrap();
        assert_eq!(run.requested("a"), RoleSelection::default());
        run.finish("a", true, 0);
        run.start("b", "r", "one");
        assert_eq!(run.requested("b"), role.selection);
        let reloaded = RoleSettings::load(dir.path().join("roles.json"));
        assert_eq!(reloaded.requested("Team", "one", "codex"), role.selection);
        let mut unsupported = role.clone();
        unsupported.selection.effort = Some("ultra".into());
        assert!(run.settings.save(unsupported, &models).is_err());
        let mut unsupported = role;
        unsupported.backend = "stub".into();
        assert!(run.settings.save(unsupported, &models).is_err());
    }
    #[test]
    fn unix_socket_fixture_tests_subscription_and_confirmed_turn_start_without_permission_overrides(
    ) {
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("app.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            let mut methods = Vec::new();
            loop {
                let message = ws.read().unwrap();
                let tungstenite::Message::Text(body) = message else {
                    continue;
                };
                let request: Value = serde_json::from_str(&body).unwrap();
                let method = request["method"].as_str().unwrap();
                methods.push(method.to_string());
                let result = match method {
                    "initialize" => json!({}),
                    "initialized" => continue,
                    "thread/loaded/list" => json!({"data":["thread-a"]}),
                    "thread/read" => {
                        if request["params"]["includeTurns"] == true {
                            ws.send(tungstenite::Message::Text(json!({"id":request["id"], "error":{"code":-32600,"message":"not materialized yet"}}).to_string())).unwrap();
                            continue;
                        }
                        json!({"thread":{"id":"thread-a","status":{"type":"idle"},"turns":[]}})
                    }
                    "thread/resume" => {
                        assert_eq!(
                            request["params"],
                            json!({"threadId":"thread-a", "excludeTurns":true})
                        );
                        ws.send(tungstenite::Message::Text(json!({"id":request["id"], "error":{"code":-32600,"message":"no rollout found"}}).to_string())).unwrap();
                        continue;
                    }
                    "model/list" => {
                        json!({"data":[{"model":"available","defaultReasoningEffort":"medium","supportedReasoningEfforts":[{"reasoningEffort":"medium"}]}],"nextCursor":null})
                    }
                    "turn/start" => {
                        assert_eq!(request["params"]["model"], "available");
                        assert_eq!(request["params"]["effort"], "medium");
                        for forbidden in ["approvalPolicy", "approvalsReviewer", "sandboxPolicy"] {
                            assert!(request["params"].get(forbidden).is_none());
                        }
                        json!({"turn":{"id":"turn"}})
                    }
                    _ => panic!("unexpected request {method}"),
                };
                ws.send(tungstenite::Message::Text(
                    json!({"id":request["id"],"result":result}).to_string(),
                ))
                .unwrap();
                if method == "turn/start" {
                    return methods;
                }
            }
        });
        let run = run(dir.path());
        run.settings
            .save(
                SavedRoleSettings {
                    team: "Team".into(),
                    agent: "one".into(),
                    backend: "codex".into(),
                    selection: RoleSelection {
                        model: Some("available".into()),
                        effort: None,
                    },
                },
                &[SupportedModel {
                    model: "available".into(),
                    name: "Available".into(),
                    efforts: vec!["medium".into()],
                    default_effort: Some("medium".into()),
                }],
            )
            .unwrap();
        run.start("a", "r", "one");
        let mut monitor = InvocationMonitor {
            run: run.clone(),
            id: "a".into(),
            socket: Some(socket),
            source: None,
            capture: None,
            session: None,
            subscribed: false,
        };
        monitor.connect(true).unwrap();
        assert_eq!(
            run.snapshot().invocations[0].connection,
            ActivityConnection::Unsupported
        );
        assert!(monitor
            .deliver_configured("OMAR INVOCATION\ninvocation_id: a")
            .unwrap());
        assert!(run.snapshot().invocations[0]
            .settings_application
            .contains("accepted by turn/start"));
        assert_eq!(
            run.snapshot().invocations[0].confirmed,
            RoleSelection::default()
        );
        assert_eq!(
            server.join().unwrap(),
            vec![
                "initialize",
                "initialized",
                "thread/loaded/list",
                "thread/read",
                "thread/read",
                "thread/resume",
                "thread/read",
                "model/list",
                "thread/read",
                "turn/start"
            ]
        );
    }
    #[test]
    fn bounded_timeline_replay_does_not_turn_old_work_into_new_activity() {
        let dir = tempfile::tempdir().unwrap();
        let run = run(dir.path());
        run.start("a", "r", "one");
        let mut c = capture(&run, "a");
        for number in 0..250 {
            let mut item = command("completed", json!(0));
            item["id"] = json!(format!("item-{number}"));
            c.item("turn", &item, true, Some(now_ms()), false);
        }
        let before = run.snapshot();
        assert_eq!(before.invocations[0].events.len(), 200);
        for number in 0..250 {
            let mut item = command("completed", json!(0));
            item["id"] = json!(format!("item-{number}"));
            c.item("turn", &item, true, None, true);
        }
        let after = run.snapshot();
        assert_eq!(before.sequence, after.sequence);
        assert_eq!(
            before.invocations[0].last_activity_at,
            after.invocations[0].last_activity_at
        );
    }
}
