//! OMAR MCP server.
//!
//! This replaces the legacy REST surface with a typed MCP tool interface.

use anyhow::{anyhow, Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use crate::app::AgentInfo;
use crate::backend_probe;
use crate::computer;
use crate::config;
use crate::ea::{self, EaId};
use crate::manager::{self, McpLaunchContext};
use crate::memory;
use crate::metrics;
use crate::process::{pid_alive, pid_file_is_stale};
use crate::projects;
use crate::scheduler::{self, ScheduledEvent};
use crate::tmux::{DeliveryOptions, HealthChecker, TmuxClient};

const JSONRPC_VERSION: &str = "2.0";
const PROTOCOL_VERSION: &str = "2024-11-05";
const INITIAL_PROMPT_DELIVERY_STATUS_TIMEOUT: Duration = Duration::from_millis(1500);
const SERVER_INSTRUCTIONS: &str = concat!(
    "OMAR provides orchestration tools for executive assistant and worker sessions. ",
    "Use these tools for agent delegation, project tracking, scheduled wake-ups, ",
    "manager notes, action logs, Slack replies, and shared computer control. ",
    "Prefer tool descriptions for exact parameters, side effects, retry behavior, ",
    "and common failure modes."
);

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

#[derive(Debug, Deserialize)]
struct ToolCallRequest {
    name: String,
    #[serde(default)]
    arguments: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageFraming {
    ContentLength,
    JsonLine,
}

#[derive(Debug)]
enum McpRead {
    Request(JsonRpcRequest, MessageFraming),
    ParseError {
        message: String,
        framing: MessageFraming,
    },
}

/// Serde helpers that accept either a JSON integer or a JSON string for
/// numeric fields. Works around a Claude Code tool-call XML-to-JSON quirk
/// where integer parameters can arrive as strings ("1" instead of 1).
/// Agents calling via raw MCP JSON-RPC don't hit this — only the harness
/// XML path does. Applied to every integer-typed Args field reachable
/// from an MCP tool call.
mod flex_int {
    use serde::{Deserialize, Deserializer};
    use serde_json::Value;

    pub fn deserialize_usize<'de, D>(d: D) -> Result<usize, D::Error>
    where
        D: Deserializer<'de>,
    {
        match Value::deserialize(d)? {
            Value::Number(n) => n
                .as_u64()
                .map(|v| v as usize)
                .ok_or_else(|| serde::de::Error::custom("expected non-negative integer")),
            Value::String(s) => s.parse::<usize>().map_err(serde::de::Error::custom),
            other => Err(serde::de::Error::custom(format!(
                "expected integer or string, got {}",
                other
            ))),
        }
    }

    pub fn deserialize_u32<'de, D>(d: D) -> Result<u32, D::Error>
    where
        D: Deserializer<'de>,
    {
        match Value::deserialize(d)? {
            Value::Number(n) => n
                .as_u64()
                .and_then(|v| u32::try_from(v).ok())
                .ok_or_else(|| serde::de::Error::custom("expected integer in u32 range")),
            Value::String(s) => s.parse::<u32>().map_err(serde::de::Error::custom),
            other => Err(serde::de::Error::custom(format!(
                "expected integer or string, got {}",
                other
            ))),
        }
    }

    pub fn deserialize_opt_u64<'de, D>(d: D) -> Result<Option<u64>, D::Error>
    where
        D: Deserializer<'de>,
    {
        match Option::<Value>::deserialize(d)? {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(n)) => Ok(n.as_u64()),
            Some(Value::String(s)) => s.parse::<u64>().map(Some).map_err(serde::de::Error::custom),
            Some(other) => Err(serde::de::Error::custom(format!(
                "expected integer or string, got {}",
                other
            ))),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use serde::Deserialize;
        use serde_json::json;

        #[derive(Deserialize, Debug)]
        struct U {
            #[serde(deserialize_with = "deserialize_usize")]
            v: usize,
        }
        #[derive(Deserialize, Debug)]
        struct U32 {
            #[serde(deserialize_with = "deserialize_u32")]
            v: u32,
        }
        #[derive(Deserialize, Debug)]
        struct O {
            #[serde(default, deserialize_with = "deserialize_opt_u64")]
            v: Option<u64>,
        }

        #[test]
        fn usize_from_int_and_string() {
            let a: U = serde_json::from_value(json!({"v": 7})).unwrap();
            let b: U = serde_json::from_value(json!({"v": "7"})).unwrap();
            assert_eq!(a.v, 7);
            assert_eq!(b.v, 7);
        }

        #[test]
        fn u32_from_int_and_string() {
            let a: U32 = serde_json::from_value(json!({"v": 42})).unwrap();
            let b: U32 = serde_json::from_value(json!({"v": "42"})).unwrap();
            assert_eq!(a.v, 42);
            assert_eq!(b.v, 42);
        }

        #[test]
        fn opt_u64_handles_int_string_null_and_missing() {
            let a: O = serde_json::from_value(json!({"v": 99})).unwrap();
            let b: O = serde_json::from_value(json!({"v": "99"})).unwrap();
            let c: O = serde_json::from_value(json!({"v": null})).unwrap();
            let d: O = serde_json::from_value(json!({})).unwrap();
            assert_eq!(a.v, Some(99));
            assert_eq!(b.v, Some(99));
            assert_eq!(c.v, None);
            assert_eq!(d.v, None);
        }

        #[test]
        fn usize_rejects_garbage() {
            let e: Result<U, _> = serde_json::from_value(json!({"v": "abc"}));
            assert!(e.is_err());
        }
    }
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos() as u64
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn infer_backend_name(explicit_backend: Option<&str>, command: &str) -> String {
    fn normalize(s: &str) -> Option<&'static str> {
        match s.trim().to_ascii_lowercase().as_str() {
            "codex" => Some("codex"),
            "cursor" => Some("cursor"),
            "agy" => Some("agy"),
            "claude" | "claude-code" | "claude_code" => Some("claude"),
            "opencode" => Some("opencode"),
            _ => None,
        }
    }

    if let Some(name) = explicit_backend.and_then(normalize) {
        return name.to_string();
    }

    for token in command.split_whitespace() {
        let token = token.trim_matches(|c| matches!(c, '"' | '\'' | '(' | ')' | '[' | ']'));
        if token.is_empty() {
            continue;
        }
        let executable = Path::new(token)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(token);
        if let Some(name) = normalize(executable) {
            return name.to_string();
        }
    }

    "unknown".to_string()
}

fn looks_like_supervisor_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let tokens: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect();
    tokens
        .iter()
        .any(|token| matches!(*token, "pm" | "supervisor"))
        || tokens.windows(2).any(|pair| pair == ["project", "manager"])
}

fn supports_initial_prompt_delivery(backend_name: &str) -> bool {
    backend_name != "unknown"
}

fn validate_model_name(model: &str) -> Result<()> {
    if !model
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
    {
        return Err(anyhow!(
            "Invalid model name. Only alphanumeric, '-', '_', '.', '/' allowed."
        ));
    }
    Ok(())
}

fn validate_reasoning_effort(reasoning_effort: &str) -> Result<()> {
    if matches!(reasoning_effort, "low" | "medium" | "high" | "xhigh") {
        Ok(())
    } else {
        Err(anyhow!(
            "Invalid reasoning_effort. Supported values: low, medium, high, xhigh."
        ))
    }
}

fn apply_spawn_agent_command_overrides(
    mut base_command: String,
    backend: Option<&str>,
    command: Option<&str>,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
) -> Result<String> {
    if let Some(model) = model {
        validate_model_name(model)?;
        base_command = format!("{} --model {}", base_command, model);
    }

    if let Some(reasoning_effort) = reasoning_effort {
        validate_reasoning_effort(reasoning_effort)?;
        if command.is_some() || backend != Some("codex") {
            return Err(anyhow!(
                "reasoning_effort is only supported when spawn_agent uses backend='codex'."
            ));
        }
        base_command = format!(
            "{} -c model_reasoning_effort='\"{}\"'",
            base_command, reasoning_effort
        );
    }

    Ok(base_command)
}

fn append_debug_log(context: &McpLaunchContext, line: &str) {
    let state_dir = ea::ea_state_dir(context.ea_id, &context.omar_dir);
    if std::fs::create_dir_all(&state_dir).is_err() {
        return;
    }
    let path = state_dir.join("mcp_server.log");
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{} {}", now_rfc3339(), line);
    }
}

fn last_output_line(output: &str) -> String {
    output
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim()
        .chars()
        .take(120)
        .collect()
}

fn clean_human_output(output: &str) -> String {
    static ANSI_RE: OnceLock<Regex> = OnceLock::new();
    static ESCAPED_ANSI_RE: OnceLock<Regex> = OnceLock::new();
    static CONTROL_RE: OnceLock<Regex> = OnceLock::new();

    let ansi_re = ANSI_RE.get_or_init(|| {
        Regex::new(r"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07]*(?:\x07|\x1b\\))").unwrap()
    });
    let escaped_ansi_re =
        ESCAPED_ANSI_RE.get_or_init(|| Regex::new(r"\\u001b(?:\[[0-?]*[ -/]*[@-~])?").unwrap());
    let control_re =
        CONTROL_RE.get_or_init(|| Regex::new(r"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]").unwrap());

    let output = ansi_re.replace_all(output, "");
    let output = escaped_ansi_re.replace_all(&output, "");
    control_re.replace_all(&output, "").to_string()
}

fn backend_available_from_command(command: &str, fallback_executable: &str) -> bool {
    let executable = command
        .split_whitespace()
        .next()
        .unwrap_or(fallback_executable);
    backend_probe::backend_version_probe_succeeds(executable)
}

#[cfg(test)]
fn ready_to_complete_tail(output: &str, tail_lines: usize) -> bool {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .rev()
        .take(tail_lines)
        .any(|line| line.contains("[TASK COMPLETE]"))
}

fn health_from_activity(activity: i64, idle_warning: i64) -> &'static str {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs() as i64;
    if now.saturating_sub(activity) <= idle_warning {
        "running"
    } else {
        "idle"
    }
}

fn lock_path_for_state_dir(state_dir: &Path) -> PathBuf {
    state_dir.join(".mcp-state.lock")
}

#[derive(Debug, Serialize, Deserialize)]
struct LockFile {
    pid: u32,
    owner: String,
}

#[derive(Debug)]
struct LockInfo {
    pid: Option<u32>,
    owner: String,
}

struct FileLock {
    path: PathBuf,
}

impl FileLock {
    fn acquire(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create lock directory {:?}", parent))?;
        }
        for _ in 0..500 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    let _ = writeln!(file, "{}", std::process::id());
                    return Ok(Self { path });
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    if pid_file_is_stale(&path) {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(err) => {
                    return Err(err).with_context(|| format!("Failed to acquire {:?}", path))
                }
            }
        }
        Err(anyhow!("Timed out waiting for lock {:?}", path))
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn run_server_from_context_file(path: PathBuf) -> Result<()> {
    let context: McpLaunchContext = serde_json::from_str(
        &fs::read_to_string(&path)
            .with_context(|| format!("Failed to read MCP context file {}", path.display()))?,
    )
    .with_context(|| format!("Failed to parse MCP context file {}", path.display()))?;
    apply_context_environment(&context);
    OmarMcpServer::new(context).run()
}

fn apply_context_environment(context: &McpLaunchContext) {
    if let Some(server) = context.tmux_server.as_deref() {
        if !server.trim().is_empty() {
            std::env::set_var("OMAR_TMUX_SERVER", server);
        }
    }
}

/// Run an MCP server without a pre-built context file. Used by peer
/// processes (e.g. the Slack bridge) that aren't spawned by a specific
/// backend launch and need a default context derived from the current
/// config + active EA.
pub fn run_server_with_default_context() -> Result<()> {
    let omar_dir = std::env::var_os("OMAR_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".omar")
        });
    let config_path = omar_dir.join("config.toml").to_string_lossy().into_owned();
    let config = crate::config::Config::load(Some(&config_path))
        .with_context(|| format!("Failed to load omar config for {}", omar_dir.display()))?;
    let registered = ea::ensure_default_ea(&omar_dir)?;
    let ea_id = resolve_default_context_ea(&omar_dir, &registered)?;
    let context = McpLaunchContext {
        omar_dir,
        ea_id,
        session_prefix: config.dashboard.session_prefix,
        default_command: config.agent.default_command,
        default_workdir: config.agent.default_workdir,
        health_idle_warning: config.health.idle_warning,
        tmux_server: std::env::var("OMAR_TMUX_SERVER")
            .ok()
            .map(|server| server.trim().to_string())
            .filter(|server| !server.is_empty()),
        topology: None,
    };
    OmarMcpServer::new(context).run()
}

/// Pick the EA id for a default-context server. Honors `OMAR_EA_ID` so
/// peer processes (e.g. the Slack bridge) can pin a child to a specific
/// EA without mutating the global active-EA pointer the dashboard reads.
fn resolve_default_context_ea(omar_dir: &Path, registered: &[ea::EaInfo]) -> Result<ea::EaId> {
    if let Some(raw) = std::env::var_os("OMAR_EA_ID") {
        let raw = raw.to_string_lossy();
        if !raw.is_empty() {
            let id: ea::EaId = raw
                .parse()
                .with_context(|| format!("OMAR_EA_ID '{}' is not a valid EA id", raw))?;
            if !registered.iter().any(|ea| ea.id == id) {
                return Err(anyhow!(
                    "OMAR_EA_ID {} is not a registered EA on this dashboard",
                    id
                ));
            }
            return Ok(id);
        }
    }
    Ok(ea::resolve_active_ea(omar_dir, registered))
}

struct OmarMcpServer {
    context: McpLaunchContext,
    state_dir: PathBuf,
    session_prefix: String,
    manager_session: String,
    scheduler: scheduler::Scheduler,
}

impl OmarMcpServer {
    fn new(context: McpLaunchContext) -> Self {
        let state_dir = ea::ea_state_dir(context.ea_id, &context.omar_dir);
        let session_prefix = ea::ea_prefix(context.ea_id, &context.session_prefix);
        let manager_session = ea::ea_manager_session(context.ea_id, &context.session_prefix);
        let scheduler =
            scheduler::Scheduler::with_store(scheduler::events_store_path(&context.omar_dir));
        Self {
            context,
            state_dir,
            session_prefix,
            manager_session,
            scheduler,
        }
    }

    fn run(&self) -> Result<()> {
        let stdin = io::stdin();
        let stdout = io::stdout();
        let mut reader = BufReader::new(stdin.lock());
        let mut writer = stdout.lock();

        while let Some(message) = read_message(&mut reader)? {
            match message {
                McpRead::Request(request, framing) => {
                    append_debug_log(
                        &self.context,
                        &format!("request method={} id={:?}", request.method, request.id),
                    );
                    let maybe_response = self.handle_request(request);
                    if let Some(response) = maybe_response {
                        append_debug_log(
                            &self.context,
                            &format!(
                                "response id={} has_error={}",
                                response.id,
                                response.error.is_some()
                            ),
                        );
                        write_message(&mut writer, &response, framing)?;
                        writer.flush()?;
                    }
                }
                McpRead::ParseError { message, framing } => {
                    append_debug_log(&self.context, &format!("parse_error err={}", message));
                    let response = error_response(Value::Null, -32700, &message);
                    write_message(&mut writer, &response, framing)?;
                    writer.flush()?;
                }
            }
        }

        Ok(())
    }

    fn handle_request(&self, request: JsonRpcRequest) -> Option<JsonRpcResponse> {
        let id = request.id.clone()?;
        if request.jsonrpc != JSONRPC_VERSION {
            return Some(error_response(id, -32600, "Unsupported jsonrpc version"));
        }

        let response = match request.method.as_str() {
            "initialize" => ok_response(
                id,
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {
                        "tools": { "listChanged": false }
                    },
                    "serverInfo": {
                        "name": "omar",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "instructions": SERVER_INSTRUCTIONS,
                }),
            ),
            "tools/list" => {
                let tools = if self.context.topology.is_some() {
                    topology_tool_definitions()
                } else {
                    tool_definitions()
                };
                ok_response(id, json!({ "tools": tools }))
            }
            "tools/call" => match serde_json::from_value::<ToolCallRequest>(request.params) {
                Ok(call) => ok_response(id, self.call_tool(call)),
                Err(err) => {
                    error_response(id, -32602, &format!("Invalid tool call params: {}", err))
                }
            },
            "resources/list" => ok_response(id, json!({ "resources": [] })),
            "ping" => ok_response(id, json!({})),
            _ => error_response(id, -32601, &format!("Unknown method '{}'", request.method)),
        };

        Some(response)
    }

    fn call_tool(&self, call: ToolCallRequest) -> Value {
        append_debug_log(
            &self.context,
            &format!("tool_call name={} args={}", call.name, call.arguments),
        );
        let result = if let Some(topology) = &self.context.topology {
            match call.name.as_str() {
                "omar_set_port" => crate::topology::mcp_set_port(topology, call.arguments),
                "omar_complete" => crate::topology::mcp_complete(topology, call.arguments),
                other => Err(anyhow!(
                    "Tool '{}' is unavailable to topology agent '{}'",
                    other,
                    topology.agent
                )),
            }
        } else {
            match call.name.as_str() {
                "list_backends" => self.list_backends(),
                "list_eas" => self.list_eas(),
                "get_active_ea" => self.get_active_ea(),
                "switch_ea" => self.switch_ea(call.arguments),
                "create_ea" => self.create_ea(call.arguments),
                "delete_ea" => self.delete_ea(call.arguments),
                "list_agents" => self.list_agents(),
                "get_agent" => self.get_agent(call.arguments),
                "get_agent_summary" => self.get_agent_summary(call.arguments),
                "update_agent_status" => self.update_agent_status(call.arguments),
                "spawn_agent" => self.spawn_agent(call.arguments),
                "kill_agent" => self.kill_agent(call.arguments),
                "send_input" => self.send_input(call.arguments),
                "list_projects" => self.list_projects(),
                "add_project" => self.add_project(call.arguments),
                "complete_project" => self.complete_project(call.arguments),
                "schedule_omar_event" => self.schedule_omar_event(call.arguments),
                "list_events" => self.list_events(),
                "cancel_event" => self.cancel_event(call.arguments),
                "log_justification" => self.log_justification(call.arguments),
                "slack_reply" => self.slack_reply(call.arguments),
                "computer_status" => self.computer_status(),
                "computer_lock_acquire" => self.computer_lock_acquire(call.arguments),
                "computer_lock_release" => self.computer_lock_release(call.arguments),
                "computer_screenshot" => self.computer_screenshot(call.arguments),
                "computer_mouse" => self.computer_mouse(call.arguments),
                "computer_keyboard" => self.computer_keyboard(call.arguments),
                "computer_screen_size" => self.computer_screen_size(),
                "computer_mouse_position" => self.computer_mouse_position(),
                other => Err(anyhow!("Unknown tool '{}'", other)),
            }
        };

        match result {
            Ok(value) => {
                append_debug_log(&self.context, &format!("tool_ok name={}", call.name));
                tool_success_for(&call.name, value)
            }
            Err(err) => {
                append_debug_log(
                    &self.context,
                    &format!("tool_err name={} err={}", call.name, err),
                );
                tool_error(err)
            }
        }
    }

    fn ea_id(&self) -> EaId {
        self.context.ea_id
    }

    fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    fn session_prefix(&self) -> &str {
        &self.session_prefix
    }

    fn manager_session(&self) -> &str {
        &self.manager_session
    }

    fn qualified_session_name(&self, short_or_full: &str) -> Result<String> {
        let prefix = self.session_prefix();
        let manager = self.manager_session();
        if short_or_full == manager || short_or_full.starts_with(prefix) {
            Ok(short_or_full.to_string())
        } else {
            Ok(format!("{}{}", prefix, short_or_full))
        }
    }

    fn display_name<'a>(&self, session_name: &'a str) -> &'a str {
        session_name
            .strip_prefix(self.session_prefix())
            .unwrap_or(session_name)
    }

    fn client(&self) -> TmuxClient {
        TmuxClient::new(self.session_prefix())
    }

    fn scheduler(&self) -> &scheduler::Scheduler {
        &self.scheduler
    }

    fn refresh_memory_locked(&self) -> Result<()> {
        let state_dir = self.state_dir();
        let prefix = self.session_prefix();
        let manager_session = self.manager_session();
        let client = TmuxClient::new(prefix);
        let mut checker = HealthChecker::new(client.clone(), self.context.health_idle_warning);
        let sessions = client.list_sessions().unwrap_or_default();
        let mut manager = None;
        let mut agents = Vec::new();
        for session in sessions {
            let info = AgentInfo {
                session: session.clone(),
                health: checker.check(&session.name),
                is_unresolved: false,
            };
            if session.name == manager_session {
                manager = Some(info);
            } else {
                agents.push(info);
            }
        }
        memory::write_memory_to(
            state_dir,
            &agents,
            manager.as_ref(),
            manager_session,
            &client,
            &self.scheduler().list_by_ea(self.ea_id()),
        );
        Ok(())
    }

    fn list_backends(&self) -> Result<Value> {
        let backends = ["claude", "codex", "cursor", "opencode", "agy"];
        let infos: Vec<Value> = backends
            .iter()
            .filter_map(|name| {
                let command = config::resolve_backend(name).ok()?;
                let available = backend_available_from_command(&command, name);
                Some(json!({
                    "name": name,
                    "available": available,
                    "command": command,
                }))
            })
            .collect();
        Ok(json!({ "backends": infos }))
    }

    fn list_eas(&self) -> Result<Value> {
        let registry = ea::ensure_default_ea(&self.context.omar_dir)?;
        let active = ea::resolve_active_ea(&self.context.omar_dir, &registry);
        let eas: Vec<Value> = registry
            .into_iter()
            .map(|ea_info| {
                json!({
                    "id": ea_info.id,
                    "name": ea_info.name,
                    "description": ea_info.description,
                    "is_active": ea_info.id == active,
                })
            })
            .collect();
        Ok(json!({ "active": active, "eas": eas }))
    }

    fn get_active_ea(&self) -> Result<Value> {
        let registry = ea::ensure_default_ea(&self.context.omar_dir)?;
        let active = ea::resolve_active_ea(&self.context.omar_dir, &registry);
        let info = registry
            .iter()
            .find(|ea_info| ea_info.id == active)
            .ok_or_else(|| anyhow!("Active EA {} not found in registry", active))?;
        Ok(json!({
            "ea_id": info.id,
            "name": info.name,
        }))
    }

    fn switch_ea(&self, args: Value) -> Result<Value> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(deserialize_with = "flex_int::deserialize_u32")]
            ea_id: u32,
        }
        let args: Args = serde_json::from_value(args)?;
        let registry = ea::ensure_default_ea(&self.context.omar_dir)?;
        let info = registry
            .iter()
            .find(|ea_info| ea_info.id == args.ea_id)
            .ok_or_else(|| anyhow!("EA {} not found in registry", args.ea_id))?
            .clone();
        ea::save_active_ea(&self.context.omar_dir, args.ea_id)?;
        Ok(json!({
            "ea_id": info.id,
            "name": info.name,
        }))
    }

    fn create_ea(&self, args: Value) -> Result<Value> {
        #[derive(Deserialize)]
        struct Args {
            name: String,
            description: Option<String>,
        }
        let args: Args = serde_json::from_value(args)?;
        ea::validate_ea_name(&args.name).map_err(|e| anyhow!(e))?;
        let id = ea::register_ea(
            &self.context.omar_dir,
            &args.name,
            args.description.as_deref(),
        )?;
        Ok(json!({
            "id": id,
            "name": args.name,
            "description": args.description,
        }))
    }

    fn delete_ea(&self, args: Value) -> Result<Value> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(deserialize_with = "flex_int::deserialize_u32")]
            ea_id: u32,
        }
        let args: Args = serde_json::from_value(args)?;
        let registry = ea::ensure_default_ea(&self.context.omar_dir)?;
        if registry.len() <= 1 {
            return Err(anyhow!(
                "Cannot delete the only EA; at least one EA must remain"
            ));
        }
        if !registry.iter().any(|ea| ea.id == args.ea_id) {
            return Err(anyhow!("EA {} not found in registry", args.ea_id));
        }

        let prefix = ea::ea_prefix(args.ea_id, &self.context.session_prefix);
        let manager_session = ea::ea_manager_session(args.ea_id, &self.context.session_prefix);
        let client = TmuxClient::new(&prefix);
        let sessions = client.list_sessions().unwrap_or_default();
        let all_sessions = client.list_all_sessions().unwrap_or_default();
        let mut session_names = Vec::new();
        for session in &sessions {
            if session.name != manager_session {
                session_names.push(session.name.clone());
            }
        }
        let manager_present = all_sessions
            .iter()
            .any(|session| session.name == manager_session);
        if manager_present {
            session_names.push(manager_session.clone());
        }
        for session_name in &session_names {
            if all_sessions
                .iter()
                .any(|session| session.name == *session_name && session.attached)
            {
                return Err(anyhow!("Cannot delete attached session"));
            }
        }
        let mut killed = 0usize;
        for session_name in session_names {
            client
                .kill_session(&session_name)
                .map_err(|e| anyhow!("Failed to kill session '{}': {}", session_name, e))?;
            killed += 1;
        }
        let state_dir = ea::ea_state_dir(args.ea_id, &self.context.omar_dir);
        if state_dir.exists() {
            fs::remove_dir_all(&state_dir)
                .map_err(|e| anyhow!("Failed to remove state dir {:?}: {}", state_dir, e))?;
        }
        let notes_path = memory::manager_notes_path(&self.context.omar_dir, args.ea_id);
        if notes_path.exists() {
            fs::remove_file(&notes_path)
                .map_err(|e| anyhow!("Failed to remove notes {:?}: {}", notes_path, e))?;
        }
        // Per-EA MCP scratch dir (context.json, claude-mcp.json, backend
        // policy/config files). These were silently leaking on
        // EA delete before and could grow unbounded across re-creates.
        let mcp_dir = self
            .context
            .omar_dir
            .join("mcp")
            .join(format!("ea-{}", args.ea_id));
        if mcp_dir.exists() {
            fs::remove_dir_all(&mcp_dir)
                .map_err(|e| anyhow!("Failed to remove mcp dir {:?}: {}", mcp_dir, e))?;
        }
        manager::remove_omar_antigravity_mcp_config(args.ea_id)?;
        let events_cancelled = self.scheduler().cancel_by_ea(args.ea_id);
        ea::unregister_ea(&self.context.omar_dir, args.ea_id)?;
        Ok(json!({
            "deleted_ea": args.ea_id,
            "agents_killed": killed,
            "events_cancelled": events_cancelled,
        }))
    }

    fn list_agents(&self) -> Result<Value> {
        let client = self.client();
        let manager_session = self.manager_session();
        let sessions = client.list_sessions()?;
        let agents: Vec<Value> = sessions
            .iter()
            .filter(|s| s.name != manager_session)
            .map(|s| {
                let output =
                    clean_human_output(&client.capture_pane_plain(&s.name, 50).unwrap_or_default());
                json!({
                    "id": self.display_name(&s.name),
                    "health": health_from_activity(s.activity, self.context.health_idle_warning),
                    "last_output": last_output_line(&output),
                })
            })
            .collect();
        Ok(json!({ "agents": agents }))
    }

    fn get_agent(&self, args: Value) -> Result<Value> {
        #[derive(Deserialize)]
        struct Args {
            name: String,
        }
        let args: Args = serde_json::from_value(args)?;
        let client = self.client();
        let session_name = self.qualified_session_name(&args.name)?;
        let output_tail = client
            .capture_pane_plain(&session_name, 200)
            .map_err(|_| anyhow!("Agent '{}' not found", args.name))?;
        let output_tail = clean_human_output(&output_tail);
        let activity = client.get_pane_activity(&session_name).unwrap_or_default();
        Ok(json!({
            "id": self.display_name(&session_name),
            "health": health_from_activity(activity, self.context.health_idle_warning),
            "last_output": last_output_line(&output_tail),
            "output_tail": output_tail,
        }))
    }

    fn get_agent_summary(&self, args: Value) -> Result<Value> {
        #[derive(Deserialize)]
        struct Args {
            name: String,
        }
        let args: Args = serde_json::from_value(args)?;
        let state_dir = self.state_dir();
        let client = self.client();
        let session_name = self.qualified_session_name(&args.name)?;
        if !client.has_session(&session_name).unwrap_or(false) {
            return Err(anyhow!("Agent '{}' not found", args.name));
        }
        let short_name = self.display_name(&session_name).to_string();
        let task = memory::load_worker_tasks_from(state_dir)
            .remove(&session_name)
            .filter(|text| !text.trim().is_empty());
        let agent_parents = memory::load_agent_parents_from(state_dir);
        let children: Vec<String> = client
            .list_sessions()?
            .into_iter()
            .filter_map(|session| {
                if agent_parents.get(&session.name) == Some(&session_name) {
                    Some(self.display_name(&session.name).to_string())
                } else {
                    None
                }
            })
            .collect();
        let activity = client.get_pane_activity(&session_name).unwrap_or(0);
        let health = health_from_activity(activity, self.context.health_idle_warning);
        Ok(json!({
            "id": short_name,
            "health": health,
            "task": task,
            "status": memory::load_agent_status_in(state_dir, &session_name),
            "children": children,
        }))
    }

    fn update_agent_status(&self, args: Value) -> Result<Value> {
        #[derive(Deserialize)]
        struct Args {
            name: String,
            status: String,
        }
        let args: Args = serde_json::from_value(args)?;
        let state_dir = self.state_dir();
        let _lock = FileLock::acquire(lock_path_for_state_dir(state_dir))?;
        let session_name = self.qualified_session_name(&args.name)?;
        let client = self.client();
        if !client.has_session(&session_name).unwrap_or(false) {
            return Err(anyhow!("Agent '{}' not found", args.name));
        }
        memory::save_agent_status_in(state_dir, &session_name, &args.status);
        self.refresh_memory_locked()?;
        Ok(json!({ "status": "updated" }))
    }

    /// The single MCP spawn path. Requires an existing `project_id`.
    fn spawn_agent(&self, args: Value) -> Result<Value> {
        #[derive(Deserialize)]
        struct Args {
            name: String,
            #[serde(deserialize_with = "flex_int::deserialize_usize")]
            project_id: usize,
            task: Option<String>,
            workdir: Option<String>,
            command: Option<String>,
            backend: Option<String>,
            model: Option<String>,
            reasoning_effort: Option<String>,
            parent: Option<String>,
        }
        let args: Args = serde_json::from_value(args)?;
        let spawn_start = std::time::Instant::now();
        let state_dir = self.state_dir();
        let ea_id = self.ea_id();
        let prefix = self.session_prefix();
        let manager_session = self.manager_session();
        let client = self.client();

        let lock_wait_start = std::time::Instant::now();
        let _lock = FileLock::acquire(lock_path_for_state_dir(state_dir))?;
        let spawn_lock_wait_ms = lock_wait_start.elapsed().as_millis() as u64;

        // Project must already exist. add_project owns creation; this path
        // never auto-creates.
        let project = projects::find_project_in(state_dir, args.project_id).ok_or_else(|| {
            anyhow!(
                "Project '{}' not found. Call add_project first to register a project.",
                args.project_id
            )
        })?;
        let project_id = project.id;
        let project_name = project.name.clone();

        let task = args
            .task
            .as_deref()
            .map(str::trim)
            .filter(|task| !task.is_empty())
            .ok_or_else(|| anyhow!("spawn_agent requires a non-empty 'task'"))?
            .to_string();

        let parent = match args.parent.as_deref().map(str::trim) {
            Some("") => return Err(anyhow!("spawn_agent parent must not be empty")),
            Some(parent) => Some(parent.to_string()),
            None => None,
        };
        self.validate_spawn_parent(project_id, parent.as_deref())?;

        let session_name = match args.name.trim() {
            n if !n.is_empty() => {
                let stripped = n.strip_prefix(prefix).unwrap_or(n);
                format!("{}{}", prefix, stripped)
            }
            _ => generate_agent_name_in_ea(prefix),
        };
        let short_name = self.display_name(&session_name).to_string();
        let parent_session = match parent.as_deref() {
            Some("ea") | None => manager_session.to_string(),
            Some(p) => self.qualified_session_name(p)?,
        };
        let prompt_parent = if parent_session == manager_session {
            "ea".to_string()
        } else {
            self.display_name(&parent_session).to_string()
        };

        if args.backend.is_some() && args.command.is_some() {
            return Err(anyhow!("Cannot specify both 'backend' and 'command'"));
        }
        let mut base_command = if let Some(backend) = args.backend.as_deref() {
            config::resolve_backend(backend).map_err(|err| anyhow!(err))?
        } else {
            args.command
                .clone()
                .unwrap_or_else(|| self.context.default_command.clone())
        };
        base_command = apply_spawn_agent_command_overrides(
            base_command,
            args.backend.as_deref(),
            args.command.as_deref(),
            args.model.as_deref(),
            args.reasoning_effort.as_deref(),
        )?;

        let backend_name = infer_backend_name(args.backend.as_deref(), &base_command);
        let supports_prompt_delivery = supports_initial_prompt_delivery(&backend_name);

        let workdir = args
            .workdir
            .unwrap_or_else(|| self.context.default_workdir.clone());
        let command = if supports_prompt_delivery {
            let prompt_file = manager::prompts_dir(&self.context.omar_dir).join("agent.md");
            manager::build_agent_command(
                &base_command,
                &prompt_file,
                &[
                    ("{{PARENT_NAME}}", &prompt_parent),
                    ("{{TASK}}", &task),
                    ("{{EA_ID}}", &ea_id.to_string()),
                ],
                &self.context,
            )
        } else {
            base_command.clone()
        };

        if client.has_session(&session_name).unwrap_or(false) {
            return Err(anyhow!("Agent '{}' already exists", short_name));
        }
        let tmux_spawn_start = std::time::Instant::now();
        client.new_session(&session_name, &command, Some(&workdir))?;
        let tmux_spawn_ms = tmux_spawn_start.elapsed().as_millis() as u64;
        metrics::record_backend_bootstrap(&backend_name);

        memory::save_agent_parent_in(state_dir, &session_name, &parent_session);
        memory::save_worker_task_in(state_dir, &session_name, &task);
        memory::save_agent_project_in(state_dir, &session_name, project_id);

        let initial_prompt_delivery = if !supports_prompt_delivery {
            "metadata_only".to_string()
        } else {
            let client2 = client.clone();
            let session2 = session_name.clone();
            let header = format!(
                "YOUR NAME: {}\nYOUR PARENT: {}\nYOUR TASK: {}",
                short_name, prompt_parent, task
            );
            // opencode has no system-prompt flag, so build_agent_command
            // spawns it bare. Inline the rendered agent.md content here so
            // the worker receives instructions plus the YOUR NAME header
            // in a single user message. Other backends already received
            // agent.md via their respective system-prompt flags.
            let first_message = if backend_name == "opencode" {
                let prompt_file = manager::prompts_dir(&self.context.omar_dir).join("agent.md");
                let content = std::fs::read_to_string(&prompt_file)
                    .unwrap_or_default()
                    .replace("{{PARENT_NAME}}", &prompt_parent)
                    .replace("{{TASK}}", &task)
                    .replace("{{EA_ID}}", &ea_id.to_string());
                format!("{}\n\n---\n\n{}", content, header)
            } else {
                header
            };
            let backend_name2 = backend_name.clone();
            let readiness_markers = crate::tmux::backend_readiness_markers(&backend_name).to_vec();
            let (delivery_tx, delivery_rx) = std::sync::mpsc::channel();
            thread::spawn(move || {
                let delivery_start = std::time::Instant::now();
                let readiness = if !readiness_markers.is_empty() {
                    let ready = client2.wait_for_markers(
                        &session2,
                        &readiness_markers,
                        Duration::from_secs(45),
                        Duration::from_millis(250),
                    );
                    if ready {
                        Ok(())
                    } else {
                        Err(anyhow!("backend readiness markers timed out"))
                    }
                } else {
                    client2.wait_for_stable(
                        &session2,
                        Duration::from_millis(500),
                        Duration::from_secs(8),
                        Duration::from_millis(120),
                        false,
                    )
                };
                let opts = DeliveryOptions::default();
                let delivery = client2.deliver_prompt(&session2, &first_message, &opts);
                let delivery_ok = delivery.is_ok();
                metrics::record_prompt_delivery(
                    ea_id,
                    &session2,
                    &backend_name2,
                    delivery_start.elapsed().as_millis() as u64,
                    delivery_ok,
                );
                let status = match (readiness, delivery) {
                    (Ok(()), Ok(())) => "delivered".to_string(),
                    (Err(readiness_err), Ok(())) => {
                        format!("delivered_after_readiness_warning: {}", readiness_err)
                    }
                    (_, Err(delivery_err)) => format!("failed: {}", delivery_err),
                };
                let _ = delivery_tx.send(status);
            });
            delivery_rx
                .recv_timeout(INITIAL_PROMPT_DELIVERY_STATUS_TIMEOUT)
                .unwrap_or_else(|_| "pending_background_delivery".to_string())
        };

        metrics::record_agent_spawn(metrics::AgentSpawnMetric {
            ea_id,
            session: &session_name,
            short_name: &short_name,
            backend: &backend_name,
            has_task: supports_prompt_delivery,
            spawn_lock_wait_ms,
            tmux_spawn_ms,
            total_spawn_ms: spawn_start.elapsed().as_millis() as u64,
        });

        self.refresh_memory_locked()?;
        Ok(json!({
            "project_id": project_id,
            "project_name": project_name,
            "agent_name": short_name,
            "status": "running",
            "initial_prompt_delivery": initial_prompt_delivery,
        }))
    }

    fn validate_spawn_parent(&self, project_id: usize, parent: Option<&str>) -> Result<()> {
        let client = self.client();
        let agent_projects = memory::load_agent_projects_from(self.state_dir());
        if let Some(parent) = parent {
            if parent == "ea" {
                return Ok(());
            }
            let parent_session = self.qualified_session_name(parent)?;
            if !client.has_session(&parent_session).unwrap_or(false) {
                return Err(anyhow!(
                    "Parent agent '{}' is not running. Use an active parent in project '{}' or pass parent='ea' for an intentional EA-owned worker.",
                    parent,
                    project_id
                ));
            }
            match agent_projects.get(&parent_session) {
                Some(parent_project_id) if *parent_project_id == project_id => Ok(()),
                Some(parent_project_id) => Err(anyhow!(
                    "Parent agent '{}' belongs to project '{}', but spawn_agent was called for project '{}'. Use a parent from the same project or pass parent='ea' for an intentional EA-owned worker.",
                    parent,
                    parent_project_id,
                    project_id
                )),
                None => Err(anyhow!(
                    "Parent agent '{}' is not attached to a project. Use a tracked parent in project '{}' or pass parent='ea' for an intentional EA-owned worker.",
                    parent,
                    project_id
                )),
            }
        } else {
            let supervisors = self.active_project_supervisors(project_id, &agent_projects);
            if supervisors.is_empty() {
                Ok(())
            } else {
                Err(anyhow!(
                    "Project '{}' already has active supervisor agent(s): {}. Set parent to the supervisor, ask the supervisor to spawn/manage the worker, or pass parent='ea' for an intentional EA-owned worker.",
                    project_id,
                    supervisors.join(", ")
                ))
            }
        }
    }

    fn active_project_supervisors(
        &self,
        project_id: usize,
        agent_projects: &std::collections::HashMap<String, usize>,
    ) -> Vec<String> {
        let client = self.client();
        let mut supervisors = Vec::new();
        for (session_name, session_project_id) in agent_projects {
            if *session_project_id != project_id {
                continue;
            }
            let short_name = self.display_name(session_name);
            if !looks_like_supervisor_name(short_name) {
                continue;
            }
            if client.has_session(session_name).unwrap_or(false) {
                supervisors.push(short_name.to_string());
            }
        }
        supervisors.sort();
        supervisors
    }

    fn kill_agent(&self, args: Value) -> Result<Value> {
        #[derive(Deserialize)]
        struct Args {
            name: String,
        }
        let args: Args = serde_json::from_value(args)?;
        let state_dir = self.state_dir();
        let _lock = FileLock::acquire(lock_path_for_state_dir(state_dir))?;
        let client = self.client();
        let session_name = self.qualified_session_name(&args.name)?;
        let manager_session = self.manager_session();
        if session_name == manager_session {
            return Err(anyhow!("Cannot kill manager via MCP"));
        }
        let _session = client.ensure_session_not_attached(&session_name)?;
        client.kill_session(&session_name)?;
        memory::remove_agent_parent_in(state_dir, &session_name);
        memory::remove_agent_project_in(state_dir, &session_name);
        let short_name = self.display_name(&session_name).to_string();
        let events_cancelled = self
            .scheduler()
            .cancel_by_receiver_and_ea(&short_name, self.ea_id());

        self.refresh_memory_locked()?;
        Ok(json!({
            "status": "killed",
            "events_cancelled": events_cancelled,
        }))
    }

    fn send_input(&self, args: Value) -> Result<Value> {
        #[derive(Deserialize)]
        struct Args {
            name: String,
            text: String,
            #[serde(default)]
            enter: bool,
        }
        let args: Args = serde_json::from_value(args)?;
        let client = self.client();
        let session_name = self.qualified_session_name(&args.name)?;
        if !client.has_session(&session_name).unwrap_or(false) {
            return Err(anyhow!("Agent '{}' not found", args.name));
        }
        client.send_keys_literal(&session_name, &args.text)?;
        if args.enter {
            thread::sleep(Duration::from_millis(100));
            client.send_keys(&session_name, "Enter")?;
        }
        Ok(json!({ "status": "sent" }))
    }

    fn schedule_omar_event(&self, args: Value) -> Result<Value> {
        #[derive(Deserialize)]
        struct Args {
            receiver: String,
            payload: String,
            sender: Option<String>,
            #[serde(default, deserialize_with = "flex_int::deserialize_opt_u64")]
            timestamp_ns: Option<u64>,
            #[serde(default, deserialize_with = "flex_int::deserialize_opt_u64")]
            delay_seconds: Option<u64>,
            #[serde(default, deserialize_with = "flex_int::deserialize_opt_u64")]
            delay_ns: Option<u64>,
            #[serde(default, deserialize_with = "flex_int::deserialize_opt_u64")]
            recurring_seconds: Option<u64>,
            #[serde(default, deserialize_with = "flex_int::deserialize_opt_u64")]
            recurring_ns: Option<u64>,
        }
        let args: Args = serde_json::from_value(args)?;
        let base = now_ns();
        let delay_ns =
            scheduler::combine_seconds_and_ns(args.delay_seconds, args.delay_ns).unwrap_or(0);
        let recurring_ns =
            scheduler::combine_seconds_and_ns(args.recurring_seconds, args.recurring_ns);
        let event = ScheduledEvent {
            id: Uuid::new_v4().to_string(),
            sender: args.sender.unwrap_or_else(|| "ea".to_string()),
            receiver: args.receiver,
            timestamp: args
                .timestamp_ns
                .unwrap_or_else(|| base.saturating_add(delay_ns)),
            payload: args.payload,
            created_at: base,
            recurring_ns,
            ea_id: self.ea_id(),
        };
        self.scheduler().insert(event.clone());
        let state_dir = self.state_dir();
        let _lock = FileLock::acquire(lock_path_for_state_dir(state_dir))?;
        self.refresh_memory_locked()?;
        Ok(json!({
            "id": event.id,
            "sender": event.sender,
            "receiver": event.receiver,
            "timestamp_ns": event.timestamp,
            "recurring_ns": event.recurring_ns,
        }))
    }

    fn list_events(&self) -> Result<Value> {
        let mut events = self.scheduler().list_by_ea(self.ea_id());
        events.sort_by_key(|event| (event.timestamp, event.created_at));
        Ok(json!({
            "events": events.into_iter().map(|event| {
                json!({
                    "id": event.id,
                    "sender": event.sender,
                    "receiver": event.receiver,
                    "timestamp_ns": event.timestamp,
                    "payload": event.payload,
                    "created_at": event.created_at,
                    "recurring_ns": event.recurring_ns,
                    "ea_id": event.ea_id,
                })
            }).collect::<Vec<_>>()
        }))
    }

    fn cancel_event(&self, args: Value) -> Result<Value> {
        #[derive(Deserialize)]
        struct Args {
            event_id: String,
        }
        let args: Args = serde_json::from_value(args)?;
        let scheduler = self.scheduler();
        match scheduler.cancel_if_ea(&args.event_id, self.ea_id()) {
            Ok(event) => {
                let state_dir = self.state_dir();
                let _lock = FileLock::acquire(lock_path_for_state_dir(state_dir))?;
                self.refresh_memory_locked()?;
                Ok(json!({
                    "id": event.id,
                    "status": "cancelled",
                }))
            }
            Err(true) => Err(anyhow!(
                "Event '{}' belongs to a different EA",
                args.event_id
            )),
            Err(false) => Err(anyhow!("Event '{}' not found", args.event_id)),
        }
    }

    fn list_projects(&self) -> Result<Value> {
        let state_dir = self.state_dir();
        let projects: Vec<Value> = projects::load_projects_from(state_dir)
            .into_iter()
            .map(|project| json!({ "id": project.id, "name": project.name }))
            .collect();
        Ok(json!({ "projects": projects }))
    }

    fn add_project(&self, args: Value) -> Result<Value> {
        #[derive(Deserialize)]
        struct Args {
            name: String,
        }
        let args: Args = serde_json::from_value(args)?;
        let name = args.name.trim();
        if name.is_empty() {
            return Err(anyhow!("Project name must not be empty"));
        }
        let state_dir = self.state_dir();
        let _lock = FileLock::acquire(lock_path_for_state_dir(state_dir))?;
        let project_id = projects::add_project_in(state_dir, name)?;
        self.refresh_memory_locked()?;
        Ok(json!({
            "project_id": project_id,
            "name": name,
        }))
    }

    fn complete_project(&self, args: Value) -> Result<Value> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(deserialize_with = "flex_int::deserialize_usize")]
            project_id: usize,
        }
        let args: Args = serde_json::from_value(args)?;
        let state_dir = self.state_dir();
        let _lock = FileLock::acquire(lock_path_for_state_dir(state_dir))?;
        let project = projects::find_project_in(state_dir, args.project_id)
            .ok_or_else(|| anyhow!("Project '{}' not found", args.project_id))?;
        let client = self.client();
        let active_sessions: Vec<String> = memory::load_agent_projects_from(state_dir)
            .into_iter()
            .filter_map(|(session_name, project_id)| {
                if project_id == args.project_id
                    && client.has_session(&session_name).unwrap_or(false)
                {
                    Some(self.display_name(&session_name).to_string())
                } else {
                    None
                }
            })
            .collect();
        if !active_sessions.is_empty() {
            return Err(anyhow!(
                "Project '{}' still has active agent sessions: {}. Kill or finish those agents before completing the project.",
                args.project_id,
                active_sessions.join(", ")
            ));
        }
        let removed = projects::remove_project_in(state_dir, args.project_id)?;
        if !removed {
            return Err(anyhow!(
                "Project '{}' could not be removed",
                args.project_id
            ));
        }
        self.refresh_memory_locked()?;
        Ok(json!({
            "project_id": project.id,
            "name": project.name,
            "status": "completed",
        }))
    }

    /// Queue a Slack reply for the `omar-slack-bridge` peer to pick up and
    /// post to Slack. File-based rendezvous via `{omar_dir}/slack_outbox/`
    /// keeps the MCP surface free of loopback HTTP and survives a bridge
    /// that's momentarily down — the bridge drains the outbox on restart.
    fn slack_reply(&self, args: Value) -> Result<Value> {
        #[derive(Deserialize)]
        struct Args {
            channel: String,
            #[serde(default)]
            thread_ts: Option<String>,
            text: String,
        }
        let args: Args = serde_json::from_value(args)?;
        if args.channel.trim().is_empty() {
            return Err(anyhow!("slack_reply requires a non-empty channel"));
        }
        if args.text.is_empty() {
            return Err(anyhow!("slack_reply requires a non-empty text"));
        }
        let dir = self.context.omar_dir.join("slack_outbox");
        fs::create_dir_all(&dir).context("Failed to create slack outbox directory")?;
        let id = Uuid::new_v4();
        let tmp = dir.join(format!("{}.json.tmp", id));
        // Prefix filename with timestamp so the bridge delivers messages in
        // the order they were queued even under heavy fan-out.
        let final_path = dir.join(format!("{}-{}.json", now_ns(), id));
        let payload = serde_json::to_vec(&json!({
            "channel": args.channel,
            "thread_ts": args.thread_ts,
            "text": args.text,
            "queued_at": now_rfc3339(),
        }))?;
        fs::write(&tmp, &payload).context("Failed to stage slack reply")?;
        fs::rename(&tmp, &final_path).context("Failed to commit slack reply")?;
        Ok(json!({
            "status": "queued",
            "path": final_path,
        }))
    }

    fn log_justification(&self, args: Value) -> Result<Value> {
        #[derive(Deserialize)]
        struct Args {
            agent_name: String,
            action: String,
            justification: String,
        }
        let args: Args = serde_json::from_value(args)?;
        let state_dir = self.state_dir();
        let _lock = FileLock::acquire(lock_path_for_state_dir(state_dir))?;
        let path = state_dir.join("action_log.jsonl");
        fs::create_dir_all(state_dir).ok();
        let line = serde_json::to_string(&json!({
            "timestamp": now_rfc3339(),
            "ea_id": self.ea_id(),
            "agent_name": args.agent_name,
            "action": args.action,
            "justification": args.justification,
        }))?;
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        writeln!(file, "{}", line)?;
        Ok(json!({ "status": "logged", "path": path }))
    }

    fn computer_lock_path(&self) -> PathBuf {
        self.context.omar_dir.join("computer.lock")
    }

    /// Read the current lock file. Returns the owner string and — when the
    /// file is in the new JSON format — the PID that claimed it, so the
    /// caller can detect a stale lock left behind by a crashed process.
    fn read_computer_lock(&self) -> Option<LockInfo> {
        let raw = fs::read_to_string(self.computer_lock_path()).ok()?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        if let Ok(parsed) = serde_json::from_str::<LockFile>(trimmed) {
            // A well-formed JSON payload with an empty `owner` is not a valid
            // lock — treat it as unreadable rather than falling through to
            // the legacy path, which would otherwise return the raw JSON
            // string as the owner.
            if parsed.owner.is_empty() {
                return None;
            }
            return Some(LockInfo {
                pid: Some(parsed.pid),
                owner: parsed.owner,
            });
        }
        // Legacy plain-text lock (older omar build). No PID → never
        // reclaimable as stale; remove the lock file by hand to clear.
        Some(LockInfo {
            pid: None,
            owner: trimmed.to_string(),
        })
    }

    fn computer_status(&self) -> Result<Value> {
        let xdotool = computer::is_available();
        let screenshot = computer::is_screenshot_available();
        let screen_size = computer::get_screen_size().ok().map(|size| {
            json!({
                "width": size.width,
                "height": size.height,
            })
        });
        Ok(json!({
            "available": xdotool && screenshot && screen_size.is_some(),
            "xdotool": xdotool,
            "screenshot": screenshot,
            "display": screen_size.is_some(),
            "screenshot_ready": screenshot && screen_size.is_some(),
            "screen_size": screen_size,
            "held_by": self.read_computer_lock().map(|info| info.owner),
        }))
    }

    fn serialize_lock_payload(&self, owner: &str) -> Result<Vec<u8>> {
        serde_json::to_vec(&LockFile {
            pid: std::process::id(),
            owner: owner.to_string(),
        })
        .context("Failed to serialize computer lock payload")
    }

    fn computer_lock_acquire(&self, args: Value) -> Result<Value> {
        #[derive(Deserialize)]
        struct Args {
            agent: String,
        }
        let args: Args = serde_json::from_value(args)?;
        let owner = format!("{}:{}", self.ea_id(), args.agent);
        let path = self.computer_lock_path();
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            // Write the payload on the handle returned by `create_new` so
            // another process racing on `read_computer_lock` can never see
            // an empty-but-present file and mistake it for a reclaim target.
            Ok(mut file) => {
                let payload = self.serialize_lock_payload(&owner)?;
                file.write_all(&payload)
                    .context("Failed to write computer lock payload")?;
                Ok(json!({ "status": "acquired", "held_by": owner }))
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                let held = self.read_computer_lock();
                match held {
                    Some(info) if info.owner == owner => {
                        Ok(json!({ "status": "already_held", "held_by": owner }))
                    }
                    Some(info) if info.pid.is_some_and(|pid| !pid_alive(pid)) => {
                        // Prior holder's process is gone. Remove the stale
                        // file and re-race via create_new so two concurrent
                        // reclaimers can't both believe they won.
                        let _ = fs::remove_file(&path);
                        match OpenOptions::new().write(true).create_new(true).open(&path) {
                            Ok(mut file) => {
                                let payload = self.serialize_lock_payload(&owner)?;
                                file.write_all(&payload)
                                    .context("Failed to write computer lock payload")?;
                                Ok(json!({
                                    "status": "reclaimed",
                                    "held_by": owner,
                                    "previous_holder": info.owner,
                                    "previous_pid": info.pid,
                                }))
                            }
                            Err(_) => {
                                let new_holder = self
                                    .read_computer_lock()
                                    .map(|i| i.owner)
                                    .unwrap_or_else(|| "unknown".to_string());
                                Err(anyhow!("Computer is locked by '{}'", new_holder))
                            }
                        }
                    }
                    Some(info) => Err(anyhow!("Computer is locked by '{}'", info.owner)),
                    None => {
                        // File exists but we can't parse it. Could be mid-write
                        // by another acquirer, or a corrupted/legacy leftover.
                        // Don't reclaim blindly — the caller can retry, or an
                        // operator can remove the lock file by hand if stale.
                        Err(anyhow!(
                            "Computer lock file exists but is unreadable; \
                             retry, or remove the lock file by hand if stale"
                        ))
                    }
                }
            }
            Err(err) => Err(err).context("Failed to acquire computer lock"),
        }
    }

    fn computer_lock_release(&self, args: Value) -> Result<Value> {
        #[derive(Deserialize)]
        struct Args {
            agent: String,
        }
        let args: Args = serde_json::from_value(args)?;
        let owner = format!("{}:{}", self.ea_id(), args.agent);
        let held = self.read_computer_lock();
        match held {
            Some(ref info) if info.owner == owner => {
                let _ = fs::remove_file(self.computer_lock_path());
                Ok(json!({ "status": "released" }))
            }
            Some(info) => Err(anyhow!("Lock held by '{}', not '{}'", info.owner, owner)),
            None => Ok(json!({ "status": "not_held" })),
        }
    }

    fn verify_computer_lock(&self, agent: &str) -> Result<()> {
        let expected = format!("{}:{}", self.ea_id(), agent);
        match self.read_computer_lock() {
            Some(info) if info.owner == expected => Ok(()),
            Some(info) => Err(anyhow!("Computer is locked by '{}'", info.owner)),
            None => Err(anyhow!("You must acquire the computer lock first")),
        }
    }

    fn computer_screenshot(&self, args: Value) -> Result<Value> {
        #[derive(Deserialize)]
        struct Args {
            agent: String,
            max_width: Option<u32>,
            max_height: Option<u32>,
        }
        let args: Args = serde_json::from_value(args)?;
        self.verify_computer_lock(&args.agent)?;
        let image_base64 = if let (Some(w), Some(h)) = (args.max_width, args.max_height) {
            computer::take_screenshot_resized(w, h)?
        } else {
            computer::take_screenshot()?
        };
        let size = computer::get_screen_size().unwrap_or(computer::ScreenSize {
            width: 0,
            height: 0,
        });
        Ok(json!({
            "image_base64": image_base64,
            "width": size.width,
            "height": size.height,
            "format": "png",
        }))
    }

    fn computer_mouse(&self, args: Value) -> Result<Value> {
        #[derive(Deserialize)]
        struct Args {
            agent: String,
            action: String,
            x: i32,
            y: i32,
            button: Option<u8>,
            to_x: Option<i32>,
            to_y: Option<i32>,
            scroll_direction: Option<String>,
            scroll_amount: Option<u32>,
        }
        let args: Args = serde_json::from_value(args)?;
        self.verify_computer_lock(&args.agent)?;
        let button = args.button.unwrap_or(1);
        match args.action.as_str() {
            "move" => computer::mouse_move(args.x, args.y)?,
            "click" => computer::mouse_click(args.x, args.y, button)?,
            "double_click" => computer::mouse_double_click(args.x, args.y, button)?,
            "drag" => computer::mouse_drag(
                args.x,
                args.y,
                args.to_x.ok_or_else(|| anyhow!("drag requires to_x"))?,
                args.to_y.ok_or_else(|| anyhow!("drag requires to_y"))?,
                button,
            )?,
            "scroll" => computer::mouse_scroll(
                args.x,
                args.y,
                args.scroll_direction
                    .as_deref()
                    .ok_or_else(|| anyhow!("scroll requires scroll_direction"))?,
                args.scroll_amount.unwrap_or(3),
            )?,
            other => {
                return Err(anyhow!(
                    "Unknown mouse action '{}'. Use move/click/double_click/drag/scroll",
                    other
                ));
            }
        }
        Ok(json!({ "status": "ok" }))
    }

    fn computer_keyboard(&self, args: Value) -> Result<Value> {
        #[derive(Deserialize)]
        struct Args {
            agent: String,
            action: String,
            text: String,
        }
        let args: Args = serde_json::from_value(args)?;
        self.verify_computer_lock(&args.agent)?;
        match args.action.as_str() {
            "type" => computer::type_text(&args.text)?,
            "key" => computer::key_press(&args.text)?,
            other => return Err(anyhow!("Unknown keyboard action '{}'. Use type/key", other)),
        }
        Ok(json!({ "status": "ok" }))
    }

    fn computer_screen_size(&self) -> Result<Value> {
        let size = computer::get_screen_size()?;
        Ok(json!({ "width": size.width, "height": size.height }))
    }

    fn computer_mouse_position(&self) -> Result<Value> {
        let (x, y) = computer::get_mouse_position()?;
        Ok(json!({ "x": x, "y": y }))
    }
}

fn generate_agent_name_in_ea(prefix: &str) -> String {
    for i in 1..1000 {
        let name = format!("{}{}", prefix, i);
        let result = crate::tmux::tmux_command()
            .args(["has-session", "-t", &name])
            .output();
        match result {
            Ok(output) if !output.status.success() => return name,
            _ => continue,
        }
    }
    format!("{}{}", prefix, &Uuid::new_v4().to_string()[..8])
}

fn read_message(reader: &mut impl BufRead) -> Result<Option<McpRead>> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            return Ok(None);
        }
        let trimmed_line = line.trim();
        // Some clients (including current Claude CLI) send line-delimited JSON-RPC
        // over stdio instead of Content-Length framing.
        if trimmed_line.starts_with('{') {
            return Ok(Some(
                match serde_json::from_str::<JsonRpcRequest>(trimmed_line) {
                    Ok(request) => McpRead::Request(request, MessageFraming::JsonLine),
                    Err(err) => McpRead::ParseError {
                        message: format!("Invalid JSON line MCP request: {}", err),
                        framing: MessageFraming::JsonLine,
                    },
                },
            ));
        }
        if line.trim().is_empty() {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                match value.trim().parse::<usize>() {
                    Ok(length) => content_length = Some(length),
                    Err(err) => {
                        return Ok(Some(McpRead::ParseError {
                            message: format!("Invalid Content-Length header: {}", err),
                            framing: MessageFraming::ContentLength,
                        }));
                    }
                }
            }
        }
    }

    let Some(length) = content_length else {
        return Ok(Some(McpRead::ParseError {
            message: "Missing Content-Length header".to_string(),
            framing: MessageFraming::ContentLength,
        }));
    };
    let mut payload = vec![0u8; length];
    reader.read_exact(&mut payload)?;
    Ok(Some(match serde_json::from_slice(&payload) {
        Ok(request) => McpRead::Request(request, MessageFraming::ContentLength),
        Err(err) => McpRead::ParseError {
            message: format!("Invalid JSON MCP request body: {}", err),
            framing: MessageFraming::ContentLength,
        },
    }))
}

fn write_message(
    writer: &mut impl Write,
    response: &JsonRpcResponse,
    framing: MessageFraming,
) -> Result<()> {
    let payload = serde_json::to_vec(response)?;
    match framing {
        MessageFraming::ContentLength => {
            write!(writer, "Content-Length: {}\r\n\r\n", payload.len())?;
            writer.write_all(&payload)?;
        }
        MessageFraming::JsonLine => {
            writer.write_all(&payload)?;
            writer.write_all(b"\n")?;
        }
    }
    Ok(())
}

fn ok_response(id: Value, result: Value) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: JSONRPC_VERSION,
        id,
        result: Some(result),
        error: None,
    }
}

fn error_response(id: Value, code: i64, message: &str) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: JSONRPC_VERSION,
        id,
        result: None,
        error: Some(JsonRpcError {
            code,
            message: message.to_string(),
        }),
    }
}

fn tool_success(value: Value) -> Value {
    tool_success_with_text(
        value.clone(),
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
    )
}

fn tool_success_for(tool_name: &str, value: Value) -> Value {
    let text = match tool_name {
        "get_agent" => format_get_agent_text(&value),
        "list_agents" => format_list_agents_text(&value),
        _ => return tool_success(value),
    };
    tool_success_with_text(value, text)
}

fn tool_success_with_text(value: Value, text: String) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": text,
        }],
        "structuredContent": value,
        "isError": false,
    })
}

fn format_get_agent_text(value: &Value) -> String {
    let id = value.get("id").and_then(Value::as_str).unwrap_or("unknown");
    let health = value
        .get("health")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let last_output = value
        .get("last_output")
        .and_then(Value::as_str)
        .unwrap_or("");
    let output_tail = value
        .get("output_tail")
        .and_then(Value::as_str)
        .unwrap_or("");

    format!(
        "Agent: {id}\nHealth: {health}\nLast output: {last_output}\n\nOutput tail:\n{output_tail}"
    )
}

fn format_list_agents_text(value: &Value) -> String {
    let Some(agents) = value.get("agents").and_then(Value::as_array) else {
        return serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    };
    if agents.is_empty() {
        return "No agents found.".to_string();
    }

    let mut lines = Vec::with_capacity(agents.len() + 1);
    lines.push("Agents:".to_string());
    for agent in agents {
        let id = agent.get("id").and_then(Value::as_str).unwrap_or("unknown");
        let health = agent
            .get("health")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let last_output = agent
            .get("last_output")
            .and_then(Value::as_str)
            .unwrap_or("");
        lines.push(format!("- {id} [{health}] {last_output}"));
    }
    lines.join("\n")
}

fn tool_error(err: anyhow::Error) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": err.to_string(),
        }],
        "isError": true,
    })
}

fn tool_definitions() -> Vec<Value> {
    static TOOLS: OnceLock<Vec<Value>> = OnceLock::new();
    TOOLS
        .get_or_init(|| vec![
        tool(
            "list_backends",
            "List installed OMAR agent backends and their commands. Use before choosing a backend/model override when availability is unclear. Read-only and safe to retry.",
            json!({"type":"object","properties":{},"additionalProperties":false}),
        ),
        tool(
            "list_eas",
            "List registered Executive Assistants (EAs) visible to this OMAR state directory. Read-only and safe to retry.",
            json!({"type":"object","properties":{},"additionalProperties":false}),
        ),
        tool(
            "get_active_ea",
            "Get the EA currently selected on disk for dashboard/CLI defaults. This MCP server remains pinned to its launch EA, so this is informational for this session. Read-only and safe to retry.",
            json!({"type":"object","properties":{},"additionalProperties":false}),
        ),
        tool(
            "switch_ea",
            "Switch the persisted active-EA pointer read by the dashboard and future default CLI operations. Side effect: writes the active EA selection on disk. Does not change this MCP server's pinned EA context. Safe to retry with the same ea_id; fails if the EA does not exist.",
            json!({
                "type":"object",
                "properties":{"ea_id":{"type":"integer"}},
                "required":["ea_id"],
                "additionalProperties":false
            }),
        ),
        tool(
            "create_ea",
            "Create a new EA registry entry and state directory. Use when a distinct team/context should own its own agents, projects, notes, and scheduled events. Side effect: persists a new EA. Not idempotent; retrying can create duplicates with the same name. Fails on invalid or empty names.",
            json!({
                "type":"object",
                "properties":{
                    "name":{"type":"string"},
                    "description":{"type":"string"}
                },
                "required":["name"],
                "additionalProperties":false
            }),
        ),
        tool(
            "delete_ea",
            "Delete an EA, kill its sessions, remove its state, and cancel its scheduled events. Use only for intentional cleanup. Destructive and not retry-safe after success. Fails if this is the only remaining EA or the EA id is unknown.",
            json!({
                "type":"object",
                "properties":{"ea_id":{"type":"integer"}},
                "required":["ea_id"],
                "additionalProperties":false
            }),
        ),
        tool(
            "list_agents",
            "List running agents in this MCP server's EA with health and last-output summary. Use for monitoring and straggler discovery. Read-only and safe to retry.",
            json!({"type":"object","properties":{},"additionalProperties":false}),
        ),
        tool(
            "get_agent",
            "Get detailed output tail for one running agent. Use to inspect a stuck or completed-looking worker before deciding whether to send input, wait, or kill it. Read-only and safe to retry. Fails if the agent is not running in this EA.",
            json!({
                "type":"object",
                "properties":{"name":{"type":"string","description":"Short agent name without the session prefix."}},
                "required":["name"],
                "additionalProperties":false
            }),
        ),
        tool(
            "get_agent_summary",
            "Get one agent's tracked task, self-reported status, health, and child-agent summary without the full output tail. Use for lightweight monitoring. Read-only and safe to retry. Fails if the agent is not running in this EA.",
            json!({
                "type":"object",
                "properties":{"name":{"type":"string","description":"Short agent name without the session prefix."}},
                "required":["name"],
                "additionalProperties":false
            }),
        ),
        tool(
            "update_agent_status",
            "Update a running agent's one-line dashboard status. Use after meaningful milestones or when blocked. Side effect: writes status metadata for display/recovery. Safe to retry with the same status; later calls replace the displayed status. Fails if the agent name is invalid or not tracked.",
            json!({
                "type":"object",
                "properties":{
                    "name":{"type":"string","description":"Your own agent name."},
                    "status":{"type":"string","description":"One-line status shown in the dashboard, e.g. 'Writing auth module - 60% done'."}
                },
                "required":["name","status"],
                "additionalProperties":false
            }),
        ),
        tool(
            "spawn_agent",
            "Spawn one tracked agent session in the current EA. Use for delegated work, PM/worker decomposition, or raw demo/bash windows. Requires an existing project_id; call list_projects/add_project first because spawn_agent never auto-creates projects. Side effects: creates a tmux session, records task/project/parent metadata, and delivers the initial task prompt unless command starts a raw session. Not retry-safe with the same name after success; retry only after checking list_agents/get_agent. Common failures: project not found, duplicate agent name, invalid parent/project relationship, backend unavailable, or both backend and command set.",
            json!({
                "type":"object",
                "properties":{
                    "name":{"type":"string","description":"Short agent name (session prefix is added automatically)."},
                    "project_id":{"type":"integer","description":"Existing project id from add_project or list_projects. Required — spawn_agent does not auto-create projects."},
                    "task":{"type":"string","description":"Delivered to the agent as their initial task and shown in the dashboard. What to build or do — no [TASK COMPLETE] or parent-wakeup instructions; those are already in every agent's system prompt."},
                    "command":{"type":"string","description":"Raw command to run instead of a backend agent (e.g. 'bash' for a demo window). Mutually exclusive with backend."},
                    "backend":{"type":"string","enum":["claude","codex","cursor","opencode","agy"],"description":"Backend agent command to launch. Mutually exclusive with command."},
                    "model":{"type":"string","description":"Optional backend model override. Allowed characters are alphanumeric plus '-', '_', '.', '/'."},
                    "reasoning_effort":{"type":"string","enum":["low","medium","high","xhigh"],"description":"Optional Codex reasoning effort override. Supported only with backend='codex'; appends a Codex config override such as -c model_reasoning_effort='\"high\"'."},
                    "workdir":{"type":"string","description":"Working directory for the new session. Defaults to this MCP server's launch workdir."},
                    "parent":{"type":"string","description":"Parent agent name for hierarchy tracking. Omit only for new EA-owned top-level work; use your own name for child tasks. Pass 'ea' only for intentional EA-owned work."}
                },
                "required":["name","project_id","task"],
                "additionalProperties":false
            }),
        ),
        tool(
            "kill_agent",
            "Kill a running worker/demo agent in this EA. Use for intentional cleanup, abandoned work, or replacement after inspection. Side effects: kills the tmux session, removes parent/project metadata, and cancels scheduled events for that agent. Not retry-safe after success; a second call fails because the agent no longer exists. Cannot kill the EA manager or attached sessions.",
            json!({
                "type":"object",
                "properties":{"name":{"type":"string"}},
                "required":["name"],
                "additionalProperties":false
            }),
        ),
        tool(
            "send_input",
            "Send text to a running agent or raw demo session. Use for follow-up instructions, concrete unblocking messages, or demo commands. Side effect: injects text into the target tmux pane and optionally presses Enter. Not generally retry-safe because duplicate input may execute twice. Fails if the target agent is not running.",
            json!({
                "type":"object",
                "properties":{
                    "name":{"type":"string","description":"Short target agent/session name."},
                    "text":{"type":"string","description":"Literal text to send."},
                    "enter":{"type":"boolean","description":"Whether to press Enter after text. Defaults to false."}
                },
                "required":["name","text"],
                "additionalProperties":false
            }),
        ),
        tool(
            "list_projects",
            "List tracked projects in this EA. Use before spawning agents to reuse an existing project when the work belongs to the same initiative. Read-only and safe to retry.",
            json!({"type":"object","properties":{},"additionalProperties":false}),
        ),
        tool(
            "add_project",
            "Register a new project bucket in the current EA and return its project_id for spawn_agent. Use once per user initiative or reusable workstream. Side effect: persists project metadata. Not idempotent; retrying can create duplicate project names, so call list_projects after uncertain results. Projects are not auto-created or auto-removed by agent lifecycle.",
            json!({
                "type":"object",
                "properties":{
                    "name":{"type":"string","description":"Short project label."}
                },
                "required":["name"],
                "additionalProperties":false
            }),
        ),
        tool(
            "complete_project",
            "Remove a project from the current EA registry after its tracked agents are no longer running. Use when a user initiative is complete or intentionally abandoned. Side effect: deletes project metadata only; it does not kill agents. Not retry-safe after success because the project is gone. Fails if tracked agents are still running or project_id is unknown.",
            json!({
                "type":"object",
                "properties":{
                    "project_id":{"type":"integer","description":"Project id from add_project or list_projects. Fails while tracked agents for this project are still running."}
                },
                "required":["project_id"],
                "additionalProperties":false
            }),
        ),
        tool(
            "schedule_omar_event",
            "Enqueue an event in OMAR's persistent event queue to wake an agent or the EA at a chosen time. The event queue is OMAR's central coordination primitive: every timed check-in, parent completion notification, future nudge, and recurring cron-style task flows through it. Use this instead of sleep loops or any backend-native timer/reminder tool. Side effect: appends a scheduled event visible in the dashboard via list_events and durable across restarts. Not retry-safe unless duplicate delivery is acceptable; use list_events/cancel_event after uncertain results. For immediate parent notification after completion, set receiver to the parent name, payload to '[CHILD COMPLETE] {your_name}: {summary}', and delay_seconds to 0.",
            json!({
                "type":"object",
                "properties":{
                    "receiver":{"type":"string","description":"Target agent short name, or 'ea' for the Executive Assistant."},
                    "payload":{"type":"string","description":"Message text injected into the receiver's session."},
                    "sender":{"type":"string","description":"Optional sender label. Defaults to 'ea'."},
                    "delay_seconds":{"type":"integer","description":"Deliver this many seconds from now. Preferred over timestamp_ns for simplicity."},
                    "timestamp_ns":{"type":"integer","description":"Absolute logical timestamp in nanoseconds. Use delay_seconds instead unless you need precise coordination."},
                    "delay_ns":{"type":"integer","description":"Deliver this many nanoseconds from now. Use for sub-second precision."},
                    "recurring_seconds":{"type":"integer","description":"Auto-reschedule every N seconds after each delivery (cron job)."},
                    "recurring_ns":{"type":"integer","description":"Auto-reschedule every N nanoseconds after each delivery."}
                },
                "required":["receiver","payload"],
                "additionalProperties":false
            }),
        ),
        tool(
            "list_events",
            "List scheduled events in the current EA. Use to audit pending wake-ups, avoid duplicate timers, or find an event to cancel. Read-only and safe to retry.",
            json!({"type":"object","properties":{},"additionalProperties":false}),
        ),
        tool(
            "cancel_event",
            "Cancel a scheduled event in this EA by id. Use to remove stale wake-ups or duplicate timers. Side effect: deletes the event if it belongs to this EA. Not retry-safe after success; a second call reports not found. Fails if the event id is unknown or belongs to another EA.",
            json!({
                "type":"object",
                "properties":{"event_id":{"type":"string"}},
                "required":["event_id"],
                "additionalProperties":false
            }),
        ),
        tool(
            "log_justification",
            "Write a structured action log entry before significant state-changing OMAR actions. Use to record why spawning, killing, sending input, scheduling, or project changes support the user goal. Side effect: appends JSONL audit data; retrying duplicates the entry.",
            json!({
                "type":"object",
                "properties":{
                    "agent_name":{"type":"string","description":"Your own agent name."},
                    "action":{"type":"string","description":"Short label for the action being justified."},
                    "justification":{"type":"string","description":"Why this action serves the user's goal."}
                },
                "required":["agent_name","action","justification"],
                "additionalProperties":false
            }),
        ),
        tool(
            "slack_reply",
            "Queue a reply for the omar-slack-bridge to post to Slack. Use only in response to a Slack-originated request and pass channel/thread_ts from the inbound [SLACK MESSAGE] event. Side effect: writes an outbox file that the bridge may post. Not retry-safe because duplicate files can post duplicate messages. Fails if channel or text is empty.",
            json!({
                "type":"object",
                "properties":{
                    "channel":{"type":"string","description":"From the inbound [SLACK MESSAGE] event."},
                    "thread_ts":{"type":"string","description":"From the inbound [SLACK MESSAGE] event. Omit to start a new thread."},
                    "text":{"type":"string"}
                },
                "required":["channel","text"],
                "additionalProperties":false
            }),
        ),
        tool(
            "computer_status",
            "Check shared desktop-control availability and dependency status. Use before acquiring the computer lock or diagnosing GUI automation failures. Read-only and safe to retry.",
            json!({"type":"object","properties":{},"additionalProperties":false}),
        ),
        tool(
            "computer_lock_acquire",
            "Acquire the shared computer-control lock for one agent. Use before screenshots, mouse, or keyboard actions. Side effect: records lock ownership. Safe to retry only by the current holder; fails if another agent holds the lock.",
            json!({
                "type":"object",
                "properties":{"agent":{"type":"string","description":"Your own agent name. Only one agent may hold the lock at a time."}},
                "required":["agent"],
                "additionalProperties":false
            }),
        ),
        tool(
            "computer_lock_release",
            "Release the shared computer-control lock. Use when done with GUI automation or when blocked. Side effect: clears lock ownership. Safe to retry only after checking state; fails if the named agent does not hold the lock.",
            json!({
                "type":"object",
                "properties":{"agent":{"type":"string","description":"Your own agent name. Must match the name that acquired the lock."}},
                "required":["agent"],
                "additionalProperties":false
            }),
        ),
        tool(
            "computer_screenshot",
            "Take a screenshot while holding the computer-control lock. Use to inspect GUI state before mouse/keyboard actions. Read-only but requires the lock; safe to retry. Fails if the agent does not hold the lock or screenshot dependencies are unavailable.",
            json!({
                "type":"object",
                "properties":{
                    "agent":{"type":"string","description":"Your own agent name — proves you hold the lock."},
                    "max_width":{"type":"integer","description":"Resize screenshot to at most this width in pixels."},
                    "max_height":{"type":"integer","description":"Resize screenshot to at most this height in pixels."}
                },
                "required":["agent"],
                "additionalProperties":false
            }),
        ),
        tool(
            "computer_mouse",
            "Perform a mouse action while holding the computer-control lock. Use only after a recent screenshot/coordinate check. Side effect: moves/clicks/drags/scrolls the real desktop and can affect user state. Not generally retry-safe. Fails if the agent does not hold the lock or required coordinates for the action are missing.",
            json!({
                "type":"object",
                "properties":{
                    "agent":{"type":"string","description":"Your own agent name — proves you hold the lock."},
                    "action":{"type":"string","enum":["move","click","double_click","drag","scroll"],"description":"Mouse action to perform."},
                    "x":{"type":"integer","description":"X coordinate in pixels."},
                    "y":{"type":"integer","description":"Y coordinate in pixels."},
                    "button":{"type":"integer","description":"Mouse button: 1=left, 2=middle, 3=right. Defaults to 1."},
                    "to_x":{"type":"integer","description":"Drag destination X (drag action only)."},
                    "to_y":{"type":"integer","description":"Drag destination Y (drag action only)."},
                    "scroll_direction":{"type":"string","enum":["up","down"],"description":"Scroll direction (scroll action only)."},
                    "scroll_amount":{"type":"integer","description":"Number of scroll steps (scroll action only)."}
                },
                "required":["agent","action","x","y"],
                "additionalProperties":false
            }),
        ),
        tool(
            "computer_keyboard",
            "Perform a keyboard action while holding the computer-control lock. Use to type text or press key combinations in the active GUI target. Side effect: sends real keyboard input and can submit forms/commands. Not generally retry-safe. Fails if the agent does not hold the lock.",
            json!({
                "type":"object",
                "properties":{
                    "agent":{"type":"string","description":"Your own agent name — proves you hold the lock."},
                    "action":{"type":"string","enum":["type","key"],"description":"'type' to type a string, 'key' to press a key combination."},
                    "text":{"type":"string","description":"Text to type, or key combination to press."}
                },
                "required":["agent","action","text"],
                "additionalProperties":false
            }),
        ),
        tool(
            "computer_screen_size",
            "Read the current desktop screen size. Use to interpret screenshot coordinates. Read-only and safe to retry.",
            json!({"type":"object","properties":{},"additionalProperties":false}),
        ),
        tool(
            "computer_mouse_position",
            "Read the current mouse cursor position. Use for coordinate debugging during GUI automation. Read-only and safe to retry.",
            json!({"type":"object","properties":{},"additionalProperties":false}),
        ),
        ])
        .clone()
}

fn topology_tool_definitions() -> Vec<Value> {
    vec![
        tool(
            "omar_set_port",
            "Set one effect port for the active OMAR invocation. Calls are buffered until omar_complete; repeated writes to the same port use last-writer-wins semantics.",
            json!({
                "type":"object",
                "properties":{
                    "invocation_id":{"type":"string"},
                    "port":{"type":"string"},
                    "value":{}
                },
                "required":["invocation_id","port","value"],
                "additionalProperties":false
            }),
        ),
        tool(
            "omar_complete",
            "Complete the active OMAR invocation after setting its effects. The runtime validates the final effect snapshot and releases the tag barrier.",
            json!({
                "type":"object",
                "properties":{"invocation_id":{"type":"string"}},
                "required":["invocation_id"],
                "additionalProperties":false
            }),
        ),
    ]
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_env_lock()
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn unset(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.previous.as_ref() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    fn test_context() -> McpLaunchContext {
        McpLaunchContext {
            omar_dir: std::env::temp_dir().join(format!("omar-mcp-test-{}", Uuid::new_v4())),
            ea_id: 0,
            session_prefix: "omar-test-".to_string(),
            default_command: "claude".to_string(),
            default_workdir: ".".to_string(),
            health_idle_warning: 15,
            tmux_server: None,
            topology: None,
        }
    }

    #[test]
    fn topology_scope_exposes_only_port_protocol_tools() {
        let names: Vec<_> = topology_tool_definitions()
            .into_iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["omar_set_port", "omar_complete"]);
    }

    #[test]
    fn spawn_agent_command_overrides_preserve_model_and_add_codex_reasoning() {
        let command = apply_spawn_agent_command_overrides(
            "codex".to_string(),
            Some("codex"),
            None,
            Some("gpt-5.5"),
            Some("high"),
        )
        .unwrap();

        assert_eq!(
            command,
            "codex --model gpt-5.5 -c model_reasoning_effort='\"high\"'"
        );
    }

    #[test]
    fn spawn_agent_command_overrides_validate_reasoning_effort() {
        let err = apply_spawn_agent_command_overrides(
            "codex".to_string(),
            Some("codex"),
            None,
            None,
            Some("max"),
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("low, medium, high, xhigh"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn spawn_agent_command_overrides_reject_non_codex_reasoning_effort() {
        let err = apply_spawn_agent_command_overrides(
            "claude".to_string(),
            Some("claude"),
            None,
            None,
            Some("high"),
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("backend='codex'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn spawn_agent_command_overrides_reject_raw_command_reasoning_effort() {
        let err = apply_spawn_agent_command_overrides(
            "codex".to_string(),
            None,
            Some("codex"),
            None,
            Some("high"),
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("backend='codex'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn infer_backend_name_recognizes_agy() {
        assert_eq!(infer_backend_name(Some("agy"), "ignored"), "agy");
        assert_eq!(
            infer_backend_name(None, "env FOO=bar agy --dangerously-skip-permissions"),
            "agy"
        );
    }

    #[test]
    fn list_backends_includes_agy() {
        let server = OmarMcpServer::new(test_context());
        let response = server.list_backends().unwrap();
        let backends = response["backends"].as_array().unwrap();
        let agy = backends
            .iter()
            .find(|backend| backend["name"].as_str() == Some("agy"))
            .expect("agy backend entry");

        assert_eq!(agy["command"], "agy --dangerously-skip-permissions");
        assert!(agy["available"].is_boolean());
    }

    #[test]
    fn resolve_default_context_ea_uses_active_when_env_unset() {
        let _lock = env_lock();
        let _guard = EnvVarGuard::unset("OMAR_EA_ID");
        let dir = tempfile::tempdir().unwrap();
        let registered = ea::ensure_default_ea(dir.path()).unwrap();
        let id = ea::register_ea(dir.path(), "Research", None).unwrap();
        ea::save_active_ea(dir.path(), id).unwrap();
        let registered = {
            let mut all = registered;
            all.extend(
                ea::load_registry(dir.path())
                    .into_iter()
                    .filter(|ea| ea.id == id),
            );
            all
        };

        let resolved = resolve_default_context_ea(dir.path(), &registered).unwrap();

        assert_eq!(resolved, id);
    }

    #[test]
    fn resolve_default_context_ea_honors_env_override() {
        let _lock = env_lock();
        let _guard = EnvVarGuard::unset("OMAR_EA_ID");
        let dir = tempfile::tempdir().unwrap();
        let _ = ea::ensure_default_ea(dir.path()).unwrap();
        let target = ea::register_ea(dir.path(), "Research", None).unwrap();
        ea::save_active_ea(dir.path(), 0).unwrap();
        let registered = ea::load_registry(dir.path());

        std::env::set_var("OMAR_EA_ID", target.to_string());
        let resolved = resolve_default_context_ea(dir.path(), &registered).unwrap();

        assert_eq!(resolved, target);
    }

    #[test]
    fn resolve_default_context_ea_rejects_unknown_env_id() {
        let _lock = env_lock();
        let _guard = EnvVarGuard::unset("OMAR_EA_ID");
        let dir = tempfile::tempdir().unwrap();
        let registered = ea::ensure_default_ea(dir.path()).unwrap();

        std::env::set_var("OMAR_EA_ID", "99");
        let err = resolve_default_context_ea(dir.path(), &registered).unwrap_err();

        assert!(
            err.to_string().contains("99"),
            "error must name the rejected id: {err}"
        );
    }

    #[test]
    fn resolve_default_context_ea_rejects_nonnumeric_env() {
        let _lock = env_lock();
        let _guard = EnvVarGuard::unset("OMAR_EA_ID");
        let dir = tempfile::tempdir().unwrap();
        let registered = ea::ensure_default_ea(dir.path()).unwrap();

        std::env::set_var("OMAR_EA_ID", "not-a-number");
        let err = resolve_default_context_ea(dir.path(), &registered).unwrap_err();

        assert!(
            err.to_string().to_lowercase().contains("valid ea id"),
            "error should mention invalid id: {err}"
        );
    }

    #[test]
    fn context_environment_restores_tmux_server() {
        let _lock = env_lock();
        let _guard = EnvVarGuard::unset("OMAR_TMUX_SERVER");
        let mut context = test_context();
        context.tmux_server = Some("isolated-test-server".to_string());

        apply_context_environment(&context);

        assert_eq!(
            std::env::var("OMAR_TMUX_SERVER").as_deref(),
            Ok("isolated-test-server")
        );
    }

    #[test]
    fn slack_reply_queues_file_in_outbox() {
        let server = OmarMcpServer::new(test_context());
        let outbox = server.context.omar_dir.join("slack_outbox");

        let result = server
            .slack_reply(json!({
                "channel": "C123",
                "thread_ts": "1700000000.001",
                "text": "hello from test",
            }))
            .expect("slack_reply should succeed");

        assert_eq!(result["status"], json!("queued"));
        assert!(outbox.is_dir(), "outbox dir must exist");

        let entries: Vec<_> = std::fs::read_dir(&outbox)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
            .collect();
        assert_eq!(entries.len(), 1, "exactly one outbox file");

        let body = std::fs::read_to_string(entries[0].path()).unwrap();
        let parsed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["channel"], json!("C123"));
        assert_eq!(parsed["thread_ts"], json!("1700000000.001"));
        assert_eq!(parsed["text"], json!("hello from test"));
    }

    #[test]
    fn slack_reply_rejects_empty_channel_or_text() {
        let server = OmarMcpServer::new(test_context());
        assert!(server
            .slack_reply(json!({"channel": "", "text": "x"}))
            .is_err());
        assert!(server
            .slack_reply(json!({"channel": "C1", "text": ""}))
            .is_err());
    }

    #[test]
    fn delete_ea_rejects_only_ea_before_destructive_cleanup() {
        let context = test_context();
        let server = OmarMcpServer::new(context.clone());
        ea::ensure_default_ea(&context.omar_dir).unwrap();
        let state_dir = ea::ea_state_dir(0, &context.omar_dir);
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("sentinel.txt"), "keep").unwrap();

        let result = server.delete_ea(json!({ "ea_id": 0 }));

        assert!(result.is_err());
        assert!(
            state_dir.join("sentinel.txt").exists(),
            "delete_ea must not remove state before rejecting only EA"
        );
        assert_eq!(ea::load_registry(&context.omar_dir).len(), 1);
    }

    #[test]
    fn delete_ea_purges_per_ea_mcp_dir() {
        let context = test_context();
        let server = OmarMcpServer::new(context.clone());
        ea::ensure_default_ea(&context.omar_dir).unwrap();
        let target_id = ea::register_ea(&context.omar_dir, "Doomed", None).unwrap();

        // Simulate the per-EA MCP scratch files that build_ea_command and
        // friends materialize on every manager spawn. Pre-fix these were
        // leaked across delete_ea calls, growing without bound.
        let mcp_dir = context
            .omar_dir
            .join("mcp")
            .join(format!("ea-{}", target_id));
        std::fs::create_dir_all(&mcp_dir).unwrap();
        std::fs::write(mcp_dir.join("context.json"), "{}").unwrap();
        std::fs::write(mcp_dir.join("claude-mcp.json"), "{}").unwrap();

        // Also seed an EA state dir so the existing cleanup path runs.
        let state_dir = ea::ea_state_dir(target_id, &context.omar_dir);
        std::fs::create_dir_all(&state_dir).unwrap();

        let result = server.delete_ea(json!({ "ea_id": target_id }));
        assert!(result.is_ok(), "delete_ea must succeed: {:?}", result);
        assert!(
            !mcp_dir.exists(),
            "mcp/ea-{} dir leaked after delete_ea",
            target_id
        );
        assert!(!state_dir.exists(), "state dir leaked after delete_ea");
    }

    #[test]
    fn delete_ea_rejects_unknown_ea_before_destructive_cleanup() {
        let context = test_context();
        let server = OmarMcpServer::new(context.clone());
        ea::ensure_default_ea(&context.omar_dir).unwrap();
        ea::register_ea(&context.omar_dir, "Second", None).unwrap();
        let bogus_state_dir = ea::ea_state_dir(99, &context.omar_dir);
        std::fs::create_dir_all(&bogus_state_dir).unwrap();
        std::fs::write(bogus_state_dir.join("sentinel.txt"), "keep").unwrap();

        let result = server.delete_ea(json!({ "ea_id": 99 }));

        assert!(result.is_err());
        assert!(
            bogus_state_dir.join("sentinel.txt").exists(),
            "delete_ea must not remove state for an unknown EA"
        );
        assert_eq!(ea::load_registry(&context.omar_dir).len(), 2);
    }

    #[test]
    fn delete_ea_rejects_attached_manager_before_worker_cleanup() {
        if !std::process::Command::new("tmux")
            .arg("-V")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
        {
            eprintln!("Skipping test: tmux not available");
            return;
        }
        if !std::process::Command::new("script")
            .arg("--help")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
        {
            eprintln!("Skipping test: script not available");
            return;
        }

        let _lock = env_lock();
        let _guard = EnvVarGuard::unset("OMAR_TMUX_SERVER");
        let tmux_server = format!("omar-mcp-delete-manager-{}", Uuid::new_v4());
        let mut context = test_context();
        context.tmux_server = Some(tmux_server.clone());
        apply_context_environment(&context);

        let server = OmarMcpServer::new(context.clone());
        ea::ensure_default_ea(&context.omar_dir).unwrap();
        let attached_ea = ea::register_ea(&context.omar_dir, "attached-mcp-manager", None).unwrap();
        let state_dir = ea::ea_state_dir(attached_ea, &context.omar_dir);
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("sentinel.txt"), b"keep").unwrap();

        let worker_session = format!(
            "{}mcp-attached",
            ea::ea_prefix(attached_ea, &context.session_prefix)
        );
        let manager_session = ea::ea_manager_session(attached_ea, &context.session_prefix);

        let _ = std::process::Command::new("tmux")
            .args([
                "-L",
                &tmux_server,
                "new-session",
                "-d",
                "-s",
                &worker_session,
                "sleep",
                "600",
            ])
            .status()
            .expect("create worker session");
        let _ = std::process::Command::new("tmux")
            .args([
                "-L",
                &tmux_server,
                "new-session",
                "-d",
                "-s",
                &manager_session,
                "sleep",
                "600",
            ])
            .status()
            .expect("create manager session");

        let mut attachment = std::process::Command::new("script")
            .args([
                "-qec",
                &format!(
                    "tmux -L {} attach-session -t {}",
                    tmux_server, manager_session
                ),
                "/dev/null",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("attach manager session");

        let tmux_session_attached = |name: &str| -> bool {
            let output = std::process::Command::new("tmux")
                .args([
                    "-L",
                    &tmux_server,
                    "list-sessions",
                    "-F",
                    "#{session_name}|#{session_attached}",
                ])
                .output()
                .ok();
            let output = match output {
                Some(output) => output,
                None => return false,
            };
            if !output.status.success() {
                return false;
            }

            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.split_once('|'))
                .find_map(|(session, attached)| {
                    if session == name {
                        Some(attached == "1")
                    } else {
                        None
                    }
                })
                .unwrap_or(false)
        };

        for _ in 0..20 {
            if tmux_session_attached(&manager_session) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if !tmux_session_attached(&manager_session) {
            attachment.kill().expect("failed to stop script harness");
            attachment.wait().expect("failed to wait script harness");
            let _ = std::process::Command::new("tmux")
                .args(["-L", &tmux_server, "kill-session", "-t", &worker_session])
                .status();
            let _ = std::process::Command::new("tmux")
                .args(["-L", &tmux_server, "kill-session", "-t", &manager_session])
                .status();
            eprintln!("Skipping test: failed to attach manager session");
            return;
        }

        let has_session = |name: &str| -> bool {
            std::process::Command::new("tmux")
                .args(["-L", &tmux_server, "has-session", "-t", name])
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
        };

        let result = server.delete_ea(json!({ "ea_id": attached_ea }));

        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            "Cannot delete attached session",
            "expected attached guard error"
        );
        assert!(
            has_session(&worker_session),
            "attached manager should not delete worker sessions"
        );
        assert!(
            has_session(&manager_session),
            "attached manager should remain after refusal"
        );
        assert!(
            state_dir.join("sentinel.txt").exists(),
            "delete_ea should preserve state on attached refusal"
        );

        attachment.kill().expect("failed to stop script harness");
        attachment.wait().expect("failed to wait script harness");

        let result = server.delete_ea(json!({ "ea_id": attached_ea }));
        assert!(result.is_ok(), "delete should succeed after detach");
        assert!(
            !has_session(&worker_session),
            "worker session should be removed after successful delete"
        );
        assert!(
            !has_session(&manager_session),
            "manager session should be removed after successful delete"
        );
        assert!(
            !state_dir.exists(),
            "EA state should be removed after successful delete"
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_lock_reclaims_stale_pid_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".mcp-state.lock");
        std::fs::write(&path, u32::MAX.to_string()).unwrap();

        let lock = FileLock::acquire(path.clone()).expect("stale lock should be reclaimed");

        assert!(path.exists());
        drop(lock);
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn list_backends_probe_treats_hanging_command_as_unavailable() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::time::Instant;

        let temp = tempfile::tempdir().unwrap();
        let slow = temp.path().join("slow-backend");
        fs::write(&slow, "#!/bin/sh\nsleep 5\n").unwrap();
        let mut perms = fs::metadata(&slow).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&slow, perms).unwrap();

        let start = Instant::now();
        let available = backend_available_from_command(slow.to_str().unwrap(), "slow-backend");

        assert!(!available);
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "list_backends should not block indefinitely on backend --version"
        );
    }

    #[test]
    fn initialize_response_includes_server_instructions() {
        let server = OmarMcpServer::new(test_context());
        let response = server
            .handle_request(JsonRpcRequest {
                jsonrpc: JSONRPC_VERSION.to_string(),
                id: Some(json!(1)),
                method: "initialize".to_string(),
                params: json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "test", "version": "1.0"},
                }),
            })
            .expect("initialize should produce a response");

        let result = response.result.expect("initialize should return a result");
        assert_eq!(result["instructions"], json!(SERVER_INSTRUCTIONS));
        assert_eq!(result["protocolVersion"], json!(PROTOCOL_VERSION));
    }

    #[test]
    fn read_message_accepts_lf_terminated_lowercase_content_length() {
        let payload = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "ping",
            "params": {}
        }))
        .unwrap();
        let input = format!(
            "content-length: {}\ncontent-type: application/json\n\n{}",
            payload.len(),
            String::from_utf8(payload).unwrap()
        );
        let mut reader = BufReader::new(Cursor::new(input.into_bytes()));
        let request = read_message(&mut reader).expect("message should parse");
        let McpRead::Request(request, framing) = request.expect("message should be present") else {
            panic!("expected request");
        };
        assert_eq!(request.method, "ping");
        assert_eq!(request.id, Some(json!(1)));
        assert_eq!(framing, MessageFraming::ContentLength);
    }

    #[test]
    fn read_message_accepts_single_line_json_request() {
        let input = b"{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"initialize\",\"params\":{}}\n";
        let mut reader = BufReader::new(Cursor::new(input.as_slice()));
        let request = read_message(&mut reader).expect("message should parse");
        let McpRead::Request(request, framing) = request.expect("message should be present") else {
            panic!("expected request");
        };
        assert_eq!(request.method, "initialize");
        assert_eq!(request.id, Some(json!(0)));
        assert_eq!(framing, MessageFraming::JsonLine);
    }

    #[test]
    fn read_message_reports_bad_json_without_io_error() {
        let input = b"{bad json}\n";
        let mut reader = BufReader::new(Cursor::new(input.as_slice()));
        let message = read_message(&mut reader).expect("bad json should be a protocol error");
        let McpRead::ParseError { message, framing } = message.expect("message should be present")
        else {
            panic!("expected parse error");
        };
        assert_eq!(framing, MessageFraming::JsonLine);
        assert!(message.contains("Invalid JSON line MCP request"));
    }

    #[test]
    fn read_message_reports_missing_content_length_without_io_error() {
        let input = b"X-Test: nope\r\n\r\n";
        let mut reader = BufReader::new(Cursor::new(input.as_slice()));
        let message = read_message(&mut reader).expect("bad framing should be a protocol error");
        let McpRead::ParseError { message, framing } = message.expect("message should be present")
        else {
            panic!("expected parse error");
        };
        assert_eq!(framing, MessageFraming::ContentLength);
        assert!(message.contains("Missing Content-Length"));
    }

    #[test]
    fn ready_to_complete_tail_matches_recent_sentinel() {
        let output =
            "doing work\nmore work\n[TASK COMPLETE]\n\nSummary:\n- done\n- all tests pass\n";
        assert!(ready_to_complete_tail(output, 10));
    }

    #[test]
    fn ready_to_complete_tail_ignores_historical_reasoning() {
        // Sentinel appears early in a long pane capture but is buried far
        // past the tail window. Pre-fix a raw `contains` returned true here
        // and flagged the worker as ready to complete.
        let mut output = String::from("plan:\n- mention the `[TASK COMPLETE]` token\n");
        for i in 0..50 {
            output.push_str(&format!("line {}\n", i));
        }
        assert!(!ready_to_complete_tail(&output, 10));
    }

    #[test]
    fn ready_to_complete_tail_ignores_blank_lines() {
        // Blank lines between the sentinel and the tail must not push the
        // sentinel out of the tail window — we look at the last N non-empty
        // lines.
        let output = "[TASK COMPLETE]\n\n\n\nSummary:\n- done\n\n\n\n";
        assert!(ready_to_complete_tail(output, 10));
    }
}
