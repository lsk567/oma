//! Manager agent — prompt embedding, command building, and orchestration

pub mod protocol;

use anyhow::Result;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::ea::{self, EaId};
use crate::memory;
use crate::metrics;
use crate::tmux::{DeliveryOptions, TmuxClient};
use protocol::{parse_manager_message, ManagerMessage, ProposedAgent};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct McpLaunchContext {
    pub omar_dir: PathBuf,
    pub ea_id: EaId,
    pub session_prefix: String,
    pub default_command: String,
    pub default_workdir: String,
    pub health_idle_warning: i64,
    #[serde(default)]
    pub tmux_server: Option<String>,
    #[serde(default)]
    pub topology: Option<TopologyMcpContext>,
    #[serde(default)]
    pub serve: Option<ServeMcpContext>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TopologyMcpContext {
    pub team: String,
    pub agent: String,
    pub endpoint: String,
    pub token: String,
}

/// Lets the EA talk back to whoever is driving `omar serve` — replies and
/// proposed programs. Present only when serve launched the manager session.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ServeMcpContext {
    pub endpoint: String,
    pub token: String,
}

#[derive(Debug, Clone)]
pub struct ManagerRuntimeOptions {
    pub default_workdir: String,
    pub health_idle_warning: i64,
    /// Set by `omar serve` so the EA can reply and propose designs.
    pub serve: Option<ServeMcpContext>,
}

// Embed prompt files at compile time so they work regardless of CWD.
const PROMPT_EA: &str = include_str!("../../prompts/executive-assistant.md");
const PROMPT_AGENT: &str = include_str!("../../prompts/agent.md");

// Backend-native wake/reminder tools bypass OMAR's durable, EA-scoped scheduler.
// Deny these names where a backend exposes per-session tool controls.
// (Names are a superset across backends; unrecognized names are no-ops.)
const BACKEND_NATIVE_WAKE_TOOLS: &[&str] = &[
    "ScheduleWakeup",
    "TaskReminder",
    "task_reminder",
    "scheduled_tasks",
];

// Backend-native subagent/dispatcher tools overlap with OMAR's `spawn_agent`
// and would let the EA delegate work outside OMAR's bookkeeping (no tmux
// session, no project tracking, no dashboard visibility, no durable scheduler
// hooks). Deny them so all delegation flows through OMAR's MCP `spawn_agent`.
// (Names are a superset across backends; unrecognized names are no-ops.)
const BACKEND_NATIVE_AGENT_TOOLS: &[&str] = &[
    "Task", // Claude Code subagent dispatcher
    "task", // lowercase variant used by some opencode/codex builds
    "Agent",
    "agent",
    "subagent",
    "dispatch_agent",
];

/// Embedded prompt files, keyed by filename.
const EMBEDDED_PROMPTS: &[(&str, &str)] = &[
    ("executive-assistant.md", PROMPT_EA),
    ("agent.md", PROMPT_AGENT),
];

/// Return the `{omar_dir}/prompts/` directory, writing embedded prompts into it.
///
/// Prompts are shared templates containing `{{EA_ID}}` placeholders.
/// Substitution happens at spawn time in `build_ea_command` / `build_agent_command`.
pub fn prompts_dir(omar_dir: &Path) -> PathBuf {
    let dir = omar_dir.join("prompts");
    std::fs::create_dir_all(&dir).ok();

    for (name, content) in EMBEDDED_PROMPTS {
        let path = dir.join(name);
        // Always overwrite so prompts stay in sync with the binary
        std::fs::write(&path, content).ok();
    }

    dir
}

/// Escape a string for use in a sed replacement (with `|` as delimiter).
///
/// The sed expression is wrapped in single quotes in the generated shell command
/// (e.g. `sed 's|PAT|REPL|g' file`), so any single quote in the replacement
/// must be closed, escaped, and reopened: `'` → `'\''`.
fn sed_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace('&', "\\&")
        .replace('\n', "\\n")
        .replace('\'', "'\\''")
}

pub(crate) fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// CSV of every backend-native tool name OMAR wants denied (wake + subagent
/// dispatchers). Used by `--disallowedTools` style flags that take a flat list.
fn backend_native_disallowed_tools_csv() -> String {
    BACKEND_NATIVE_WAKE_TOOLS
        .iter()
        .chain(BACKEND_NATIVE_AGENT_TOOLS.iter())
        .copied()
        .collect::<Vec<_>>()
        .join(",")
}

fn current_tmux_server() -> Option<String> {
    std::env::var("OMAR_TMUX_SERVER")
        .ok()
        .map(|server| server.trim().to_string())
        .filter(|server| !server.is_empty())
}

#[cfg(test)]
fn global_home_env_lock() -> std::sync::MutexGuard<'static, ()> {
    crate::test_env_lock()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendKind {
    Agy,
    Claude,
    Codex,
    Cursor,
    Opencode,
    Stub,
}

impl BackendKind {
    fn canonical_name(self) -> &'static str {
        match self {
            BackendKind::Agy => "agy",
            BackendKind::Claude => "claude",
            BackendKind::Codex => "codex",
            BackendKind::Cursor => "cursor",
            BackendKind::Opencode => "opencode",
            BackendKind::Stub => "stub",
        }
    }
}

fn detect_backend_token(token: &str) -> Option<BackendKind> {
    let token = token.trim_matches(|c| matches!(c, '"' | '\'' | '(' | ')'));
    let executable = Path::new(token)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(token);

    match executable {
        "agy" => Some(BackendKind::Agy),
        // Homebrew ships Claude Code as a single-file executable named
        // `claude.exe`, which is what the pane reports it is running.
        "claude" | "claude.exe" => Some(BackendKind::Claude),
        "codex" => Some(BackendKind::Codex),
        "cursor" => Some(BackendKind::Cursor),
        "opencode" => Some(BackendKind::Opencode),
        // Matched on the subcommand: the executable is `omar` itself.
        "stub-agent" => Some(BackendKind::Stub),
        _ => None,
    }
}

fn detect_backend(base_command: &str) -> Option<BackendKind> {
    base_command
        .split_whitespace()
        .find_map(detect_backend_token)
}

pub fn command_backend_name(command: &str) -> Option<&'static str> {
    detect_backend(command).map(BackendKind::canonical_name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagerEnsureResult {
    AlreadyRunning,
    Started,
    ReplacedBackend,
}

/// Give opencode a port so OMAR can reach it without the input box.
///
/// opencode only listens when it is told a port: with none it talks to an
/// in-process worker over a fake hostname, and there is nothing to connect to.
/// Nothing is broken if the port cannot be claimed — the pane simply launches
/// without a side channel and events go through the composer.
fn with_opencode_port(base_command: &str) -> String {
    if base_command
        .split_whitespace()
        .any(|token| token == "--port" || token.starts_with("--port=") || token == "--hostname")
    {
        return base_command.to_string();
    }
    match crate::channel::free_port() {
        Some(port) => format!("{} --port {}", base_command, port),
        None => base_command.to_string(),
    }
}

/// Let OMAR deliver events over Claude Code's cross-session peer socket.
///
/// Without this the session holds an inbound message behind an approval
/// dialog — OMAR does not attest a permission mode, and an agent launched with
/// `--dangerously-skip-permissions` distrusts a sender that has not. The dialog
/// covers the composer, so the held message is worse than no channel at all.
///
/// `--settings` loads *additional* settings, so the operator's own settings
/// files still apply.
const CLAUDE_INBOUND_SETTINGS: &str = "--settings '{\"crossSessionInbound\":\"accept\"}'";

fn ensure_codex_runtime_flags(base_command: &str) -> String {
    if detect_backend(base_command) != Some(BackendKind::Codex) {
        return base_command.to_string();
    }

    let mut command = base_command.to_string();

    if !base_command
        .split_whitespace()
        .any(|token| token == "--dangerously-bypass-approvals-and-sandbox")
    {
        command.push_str(" --dangerously-bypass-approvals-and-sandbox");
    }

    if !base_command
        .split_whitespace()
        .any(|token| token == "--no-alt-screen")
    {
        command.push_str(" --no-alt-screen");
    }

    command
}

fn materialize_prompt_file(prompt_file: &Path, substitutions: &[(&str, &str)]) -> PathBuf {
    if substitutions.is_empty() {
        return prompt_file.to_path_buf();
    }

    let mut content = std::fs::read_to_string(prompt_file).unwrap_or_default();
    for (pattern, replacement) in substitutions {
        content = content.replace(pattern, replacement);
    }

    let stem = prompt_file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("prompt");
    let ext = prompt_file
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("md");

    // Backend reads this later, so no self-deleting guard; 0600 in the private dir.
    let rendered = match crate::paths::private_temp_dir() {
        Ok(dir) => dir.join(format!("{}-{}.{}", stem, Uuid::new_v4(), ext)),
        Err(_) => return prompt_file.to_path_buf(),
    };

    match crate::paths::create_private_file(&rendered) {
        Ok(mut file) => {
            if file.write_all(content.as_bytes()).is_ok() {
                rendered
            } else {
                let _ = std::fs::remove_file(&rendered);
                prompt_file.to_path_buf()
            }
        }
        Err(_) => prompt_file.to_path_buf(),
    }
}

/// MCP state directory for a given EA. Stable per-EA path — avoids leaking
/// files into world-readable `/tmp` and prevents unbounded growth from
/// per-spawn UUID filenames.
/// Path of the MCP context an EA's sidecar reads, for callers that need to
/// check what the launched agent actually received.
pub fn ea_mcp_context_path(omar_dir: &Path, ea_id: EaId) -> PathBuf {
    omar_dir
        .join("mcp")
        .join(format!("ea-{ea_id}"))
        .join("context.json")
}

fn mcp_ea_dir(context: &McpLaunchContext) -> Option<PathBuf> {
    let dir = context
        .omar_dir
        .join("mcp")
        .join(format!("ea-{}", context.ea_id));
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Atomically write `bytes` to `path` with mode 0600 on Unix.
///
/// Caller-readable only, because these files embed workdirs and the omar
/// binary path which leak detail about the user's environment to other
/// accounts on shared hosts. These paths are shared by every worker under an
/// EA, so publishing through a temp file avoids launch-time MCP readers seeing
/// a truncated JSON file during rapid multi-agent spawns.
fn write_private_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if std::fs::read(path).is_ok_and(|current| current == bytes) {
        return Ok(());
    }

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let tmp = parent.join(format!(".{}.{}.tmp", file_name, Uuid::new_v4()));

    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }

    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }

    Ok(())
}

pub(crate) fn materialize_mcp_context_file(context: &McpLaunchContext) -> Option<PathBuf> {
    let dir = mcp_ea_dir(context)?;
    let path = match &context.topology {
        Some(topology) => dir.join(format!(
            "context-topology-{}-{}.json",
            topology.team, topology.agent
        )),
        None => dir.join("context.json"),
    };
    let json = serde_json::to_vec(context).ok()?;
    write_private_file(&path, &json).ok()?;
    Some(path)
}

/// Strip a trailing " (deleted)" marker from an executable path.
///
/// On Linux, when a running binary's file is replaced or unlinked (e.g. a
/// rebuild/reinstall while omar keeps running), `/proc/self/exe` — and thus
/// `std::env::current_exe()` — resolves to the original path with a literal
/// " (deleted)" suffix appended. Only a trailing marker is removed; the same
/// substring elsewhere in the path is preserved.
fn strip_deleted_suffix(path: &Path) -> PathBuf {
    const MARKER: &str = " (deleted)";
    match path.to_str() {
        Some(s) => match s.strip_suffix(MARKER) {
            Some(stripped) => PathBuf::from(stripped),
            None => path.to_path_buf(),
        },
        None => path.to_path_buf(),
    }
}

/// Resolve the path to the running omar binary for use as a backend MCP server
/// command.
///
/// Uses `std::env::current_exe()`, but guards against the Linux "(deleted)"
/// case: if the binary was replaced after the process started, the raw path is
/// not runnable. We strip the marker and, if the result no longer exists on
/// disk, fall back to locating the same binary name on `PATH`. Writing an
/// unrunnable path into a backend MCP config makes the OMAR server silently
/// fail to launch, so every backend config builder must go through here.
fn omar_server_exe() -> Option<PathBuf> {
    let raw = std::env::current_exe().ok()?;
    let cleaned = strip_deleted_suffix(&raw);
    if cleaned.exists() {
        return Some(cleaned);
    }
    // The current binary was replaced/unlinked; find it again on PATH.
    let file_name = cleaned.file_name()?;
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join(file_name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    // Last resort: return the cleaned path even if we could not confirm it.
    Some(cleaned)
}

fn materialize_claude_mcp_config(context: &McpLaunchContext) -> Option<PathBuf> {
    let server_exe = omar_server_exe()?;
    let context_file = materialize_mcp_context_file(context)?;
    let json = serde_json::json!({
        "mcpServers": {
            "omar": {
                "type": "stdio",
                "command": server_exe,
                "args": ["mcp-server", "--context-file", context_file],
            }
        }
    });

    let dir = mcp_ea_dir(context)?;
    let path = match &context.topology {
        Some(topology) => dir.join(format!(
            "claude-mcp-topology-{}-{}.json",
            topology.team, topology.agent
        )),
        None => dir.join("claude-mcp.json"),
    };
    write_private_file(&path, &serde_json::to_vec(&json).ok()?).ok()?;
    Some(path)
}

fn codex_mcp_overrides(context: &McpLaunchContext) -> Option<String> {
    let server_exe = omar_server_exe()?;
    let context_file = materialize_mcp_context_file(context)?;
    let command = serde_json::to_string(&server_exe.display().to_string()).ok()?;
    let args = serde_json::to_string(&vec![
        "mcp-server".to_string(),
        "--context-file".to_string(),
        context_file.display().to_string(),
    ])
    .ok()?;
    let command_arg = format!("mcp_servers.omar.command={}", command);
    let args_arg = format!("mcp_servers.omar.args={}", args);
    Some(format!(
        "-c features.scheduled_tasks=false -c {} -c {}",
        shell_single_quote(&command_arg),
        shell_single_quote(&args_arg)
    ))
}

/// The longest socket path codex will bind.
///
/// Not the operating system's limit — macOS itself accepts 103 bytes — but
/// codex's own, measured against the shipped 0.147.0 binary: 95 binds and 96
/// fails with "path must be shorter than SUN_LEN". A path in between passes
/// the OS and is refused by codex, which loses the channel silently and leaves
/// a home behind for nothing, so the stricter bound is the useful one. It may
/// move with the codex version.
const SUN_PATH_MAX: usize = 96;

/// Flags that make codex refuse to attach to a running app-server.
///
/// The refusal is silent, so a pane carrying one of these would start a server
/// nobody joins, sit out the socket wait, and then have provisioning poll it
/// for ninety seconds — all to arrive where it started. Better to see the flag
/// and not build the home at all.
const CODEX_NO_ATTACH_FLAGS: &[&str] = &[
    "-c",
    "--config",
    "--profile",
    "--strict-config",
    "--search",
    "--approve-for-me",
    "--enable",
    "--disable",
    "--dangerously-bypass-hook-trust",
];

/// Would this command's codex refuse the app-server?
///
/// `spawn_agent` adds `-c model_reasoning_effort=…` for any agent given a
/// reasoning effort, and an operator may have configured flags of their own,
/// so this is a real case rather than a defensive one.
/// Answer codex's directory-trust prompt before it is asked.
///
/// codex blocks on "Do you trust the contents of this directory?" the first
/// time it runs anywhere new, and nothing in an agent pane ever answers it —
/// the agent simply hangs. Saying yes is exactly this record appended to the
/// config, which is what codex itself writes when a human clicks through, so
/// OMAR writes it for the directory it chose for the agent.
///
/// `CODEX_HOME` may or may not be set: the side-channel launch points it at a
/// per-pane home, and the fallback uses the operator's own.
fn codex_trust_cwd() -> String {
    "omar_home=\"${CODEX_HOME:-$HOME/.codex}\"; \
     mkdir -p \"$omar_home\"; \
     omar_cwd=\"$(pwd -P | sed 's/[\\\\\"]/\\\\&/g')\"; \
     touch \"$omar_home/config.toml\"; \
     grep -qF \"[projects.\\\"$omar_cwd\\\"]\" \"$omar_home/config.toml\" || \
     printf '\\n[projects.\"%s\"]\\ntrust_level = \"trusted\"\\n' \"$omar_cwd\" \
     >> \"$omar_home/config.toml\"; "
        .to_string()
}

fn codex_refuses_to_attach(base_command: &str) -> bool {
    base_command.split_whitespace().any(|token| {
        let flag = token.split_once('=').map_or(token, |(flag, _)| flag);
        CODEX_NO_ATTACH_FLAGS.contains(&flag)
    })
}

/// A codex home of OMAR's own, one per launched pane.
///
/// codex's TUI only attaches to an app-server when it is started with no
/// config overrides at all, so everything OMAR needs to configure — the
/// developer instructions, the OMAR MCP server, the directory trust record —
/// has to be a `config.toml` rather than the `-c` flags it used to be. That
/// file is per-pane because the MCP context file it names is per-agent, so the
/// home holding it is per-pane too.
///
/// The name is a fresh id rather than anything derived from the pane: sibling
/// workers under one EA share an MCP context, and two panes sharing one home
/// would share one app-server and one ambiguous thread list.
fn codex_home_dir(context: &McpLaunchContext) -> Option<PathBuf> {
    // Short by design: `<home>/app-server-control/app-server-control.sock` has
    // to fit in `sun_path`, and a session-shaped name would not.
    let id = Uuid::new_v4().simple().to_string();
    let dir = context.omar_dir.join("codex").join(&id[..12]);
    let socket = crate::channel::codex_socket_path(&dir);
    if socket.as_os_str().len() >= SUN_PATH_MAX {
        return None;
    }
    prune_stale_codex_homes(&context.omar_dir.join("codex"));
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Drop the codex homes of panes that are gone.
///
/// Disk only. The app-server needs no reaping: it is a background job of the
/// pane's own shell, so it shares the pane's process group and dies with it —
/// on `tmux kill-session`, and on the TUI exiting and taking the session with
/// it. But nothing removes the directory, and codex fills each one with
/// several megabytes of sqlite, so without this they accumulate for the life
/// of the machine.
///
/// A home is finished when the pane named in it is no longer a tmux session.
/// Named, rather than probed for a live socket, because the home holds the
/// running agent's thread history: deleting one whose app-server happens to be
/// down would take a working pane's state with it.
///
/// One that names no pane is either mid-launch — `codex_home_dir` made it
/// moments ago and `claim_codex_home` is about to write the name — or the
/// leftovers of a launch that never got that far. Age separates the two, and a
/// live app-server vetoes either way: whatever else is true of a home, if
/// something is still answering on its socket it is in use.
fn prune_stale_codex_homes(root: &Path) {
    const GRACE: Duration = Duration::from_secs(300);
    let client = TmuxClient::new("");
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let finished = match std::fs::read_to_string(path.join(CODEX_HOME_SESSION_FILE)) {
            Ok(session) => !client.has_session(session.trim()).unwrap_or(true),
            Err(_) => entry
                .metadata()
                .and_then(|meta| meta.modified())
                .map(|modified| modified.elapsed().unwrap_or_default() >= GRACE)
                .unwrap_or(false),
        };
        if finished && !codex_home_is_serving(&path) {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// Is a pane still using this home?
///
/// The home holds the agent's thread history and the config its TUI is
/// running on, so this is the last check before deleting one. A connection
/// that is accepted means an app-server is up, which means a pane is up.
fn codex_home_is_serving(home: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(crate::channel::codex_socket_path(home)).is_ok()
}

/// Record which pane a home belongs to, and drop any earlier home that claimed
/// the same one.
///
/// A session name is reused: `ensure_manager_session` and the topology runner
/// both kill a pane and immediately make another with the same name, so the
/// home the old pane was using would otherwise keep resolving to a live
/// session and never be pruned.
pub(crate) fn claim_codex_home(home: &Path, session: &str) {
    if let Some(root) = home.parent() {
        for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
            let sibling = entry.path();
            if sibling == home {
                continue;
            }
            let claimed = std::fs::read_to_string(sibling.join(CODEX_HOME_SESSION_FILE))
                .is_ok_and(|name| name.trim() == session);
            if claimed && !codex_home_is_serving(&sibling) {
                let _ = std::fs::remove_dir_all(&sibling);
            }
        }
    }
    let _ = std::fs::write(home.join(CODEX_HOME_SESSION_FILE), session);
}

/// Where provisioning records which tmux session a home belongs to.
pub(crate) const CODEX_HOME_SESSION_FILE: &str = "omar-session";

/// The `config.toml` for a per-pane codex home.
///
/// The operator's own `~/.codex/config.toml` is the base, so their model,
/// plugins and the rest still apply — minus `projects`, whose trust records
/// the pane appends for its own working directory and which must not appear
/// twice in one file.
fn codex_config_toml(
    user_config: &str,
    developer_instructions: &str,
    server_exe: &Path,
    context_file: &Path,
) -> String {
    let mut config: toml::Table = toml::from_str(user_config).unwrap_or_default();
    config.remove("projects");
    config.insert(
        "developer_instructions".to_string(),
        developer_instructions.into(),
    );
    if let Some(features) = config
        .entry("features")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
    {
        features.insert("scheduled_tasks".to_string(), false.into());
    }
    let mut omar = toml::Table::new();
    omar.insert(
        "command".to_string(),
        server_exe.display().to_string().into(),
    );
    omar.insert(
        "args".to_string(),
        toml::Value::Array(vec![
            "mcp-server".into(),
            "--context-file".into(),
            context_file.display().to_string().into(),
        ]),
    );
    if let Some(servers) = config
        .entry("mcp_servers")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
    {
        servers.insert("omar".to_string(), toml::Value::Table(omar));
    }
    config.to_string()
}

/// Fill a per-pane codex home and return where it is.
///
/// Returns `None` on any IO failure, and the caller then launches codex the
/// old way — with `-c` overrides and no side channel. A pane that comes up
/// without a channel still works; one that does not come up at all does not.
fn codex_home_launch(context: &McpLaunchContext, developer_instructions: &str) -> Option<PathBuf> {
    let server_exe = omar_server_exe()?;
    let context_file = materialize_mcp_context_file(context)?;
    let user_codex = dirs::home_dir()?.join(".codex");
    let config = codex_config_toml(
        &std::fs::read_to_string(user_codex.join("config.toml")).unwrap_or_default(),
        developer_instructions,
        &server_exe,
        &context_file,
    );

    // Made last, once everything that can fail has: a home abandoned between
    // being created and being filled would have no session name to prune it by
    // and no server to answer for it, so nothing would ever remove it.
    let home = codex_home_dir(context)?;
    if write_private_file(&home.join("config.toml"), config.as_bytes()).is_err() {
        let _ = std::fs::remove_dir_all(&home);
        return None;
    }

    // Symlinked, not copied: the token is refreshed in place, and a pane
    // holding a stale copy would be logged out mid-run.
    let _ = std::os::unix::fs::symlink(user_codex.join("auth.json"), home.join("auth.json"));
    // A home that has never seen an update check opens the release prompt over
    // the TUI and waits for a keypress that never comes. Carrying the
    // operator's own answer across means the pane starts where they left off.
    let _ = std::fs::copy(user_codex.join("version.json"), home.join("version.json"));

    Some(home)
}

/// Launch codex in a per-pane home, with an app-server the pane owns.
///
/// Three things happen before the TUI starts, all in the pane's own shell:
///
/// 1. The pane's working directory is recorded as trusted. Only the shell
///    knows it — `pwd -P` resolves it the same way codex does — and without
///    the record a fresh home opens a "do you trust this directory?" prompt
///    the agent never answers. Appended only if it is not already there: the
///    same table twice is a parse error, and codex refuses to start on one.
///    `sed` escapes it first, because it goes in as a TOML quoted key and a
///    directory named with a `"` or a `\` would be the same parse error.
/// 2. An app-server is started, in the background of that same shell. The TUI
///    attaches to it on its way up, which is what gives OMAR somewhere to
///    inject events.
/// 3. The TUI waits for the socket. A brand-new home has no state database
///    yet, and a server and a TUI creating it at the same moment collide on
///    the migration — codex then refuses to start with a "local database
///    appears to be damaged". Waiting for the socket means the server has
///    finished. The wait ends early if the server dies — a codex too old for
///    `app-server --listen`, or one that is not on `PATH`, would otherwise
///    spend twenty seconds of the caller's readiness budget going nowhere —
///    and is bounded in any case. Past either, the TUI starts with no
///    channel, which is the ordinary fallback.
///
/// Deliberately no `trap` to kill the server. A plain background job is in the
/// pane's process group and dies with the pane; a `trap ... HUP` makes the
/// shell *catch* the hangup instead of dying of it, and a shell blocked in the
/// TUI never reaches the handler — so it survives, holding the server open.
/// The trap does not clean up after the pane, it is what stops the pane
/// cleaning up after itself.
///
/// The TUI itself is launched with no `-c` and no `--profile`: any of those
/// makes codex refuse to attach, which is the whole reason the configuration
/// moved into the home's `config.toml`.
fn codex_home_command(home: &Path, tui_command: &str) -> String {
    let home = shell_single_quote(&home.display().to_string());
    let socket = "\"$CODEX_HOME/app-server-control/app-server-control.sock\"";
    format!(
        "export CODEX_HOME={home}; {trust}\
         codex app-server --listen unix:// >/dev/null 2>&1 & \
         omar_srv=$!; omar_waited=0; \
         while [ ! -S {socket} ] && [ \"$omar_waited\" -lt 100 ] \
         && kill -0 \"$omar_srv\" 2>/dev/null; \
         do sleep 0.2; omar_waited=$((omar_waited+1)); done; \
         {tui_command}",
        trust = codex_trust_cwd()
    )
}

fn opencode_config_env(context: &McpLaunchContext) -> Option<String> {
    let server_exe = omar_server_exe()?;
    let context_file = materialize_mcp_context_file(context)?;
    // Disable every backend-native tool that overlaps an OMAR MCP tool so
    // delegation/scheduling can only flow through OMAR and stays visible in
    // the dashboard. Names that opencode does not expose are no-ops.
    let mut tools = serde_json::Map::new();
    for name in BACKEND_NATIVE_WAKE_TOOLS
        .iter()
        .chain(BACKEND_NATIVE_AGENT_TOOLS.iter())
    {
        tools.insert((*name).to_string(), serde_json::Value::Bool(false));
    }
    let config = serde_json::json!({
        "mcp": {
            "omar": {
                "type": "local",
                "enabled": true,
                "command": [
                    server_exe.display().to_string(),
                    "mcp-server",
                    "--context-file",
                    context_file.display().to_string()
                ]
            }
        },
        "tools": tools,
        "permission": {
            "doom_loop": "deny"
        }
    });
    Some(config.to_string())
}

fn ensure_cursor_mcp_config(context: &McpLaunchContext) -> Option<()> {
    // Cursor only reads MCP servers from `~/.cursor/mcp.json`, so we have to
    // write there. Scope the key per-EA (`omar-ea-<id>`) so concurrent spawns
    // across EAs don't clobber each other, preserve every non-omar key the
    // user already has, and write via tmp+rename so partial writes under
    // concurrency can't corrupt the file.
    let server_exe = omar_server_exe()?;
    let context_file = materialize_mcp_context_file(context)?;
    let home = std::env::var("HOME").ok()?;
    let cursor_dir = PathBuf::from(home).join(".cursor");
    std::fs::create_dir_all(&cursor_dir).ok()?;
    let path = cursor_dir.join("mcp.json");

    let mut root = match std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
    {
        Some(v) if v.is_object() => v,
        _ => serde_json::json!({}),
    };

    if !root
        .get("mcpServers")
        .map(|v| v.is_object())
        .unwrap_or(false)
    {
        root["mcpServers"] = serde_json::json!({});
    }

    // Remove the legacy plain `omar` key and any stale `omar-ea-*` entries
    // whose context files no longer exist. Stale entries cause cursor to
    // fail MCP server startup, which can block loading of the fresh entry.
    if let Some(servers) = root["mcpServers"].as_object_mut() {
        let stale_keys: Vec<String> = servers
            .iter()
            .filter_map(|(k, v)| {
                if k == "omar" {
                    return Some(k.clone());
                }
                if k.starts_with("omar-ea-") {
                    let ctx_path = v
                        .get("args")
                        .and_then(|a| a.as_array())
                        .and_then(|a| a.last())
                        .and_then(|p| p.as_str())
                        .map(PathBuf::from);
                    if let Some(p) = ctx_path {
                        if !p.exists() {
                            return Some(k.clone());
                        }
                    }
                }
                None
            })
            .collect();
        for k in stale_keys {
            servers.remove(&k);
        }
    }

    let key = format!("omar-ea-{}", context.ea_id);
    root["mcpServers"][&key] = serde_json::json!({
        "command": server_exe.display().to_string(),
        "args": ["mcp-server", "--context-file", context_file.display().to_string()],
        "enabled": true,
    });

    // Best-effort cleanup of any `mcp.json.omar-*.tmp` leftovers from a
    // prior crash between write and rename. We own this naming scheme, so
    // it's safe to sweep on every successful call.
    if let Ok(entries) = std::fs::read_dir(&cursor_dir) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if name.starts_with("mcp.json.omar-") && name.ends_with(".tmp") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }

    let payload = serde_json::to_vec_pretty(&root).ok()?;
    let tmp = cursor_dir.join(format!("mcp.json.omar-{}.tmp", Uuid::new_v4()));
    std::fs::write(&tmp, &payload).ok()?;
    if std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    Some(())
}

fn ensure_antigravity_mcp_config(context: &McpLaunchContext) -> Option<()> {
    // Antigravity CLI loads MCP servers from native plugin bundles. Keep OMAR's
    // plugin EA-scoped so lifecycle and cleanup do not touch user plugins.
    // `agy plugin install` stages active plugins under ~/.gemini/config/plugins
    // and records them in ~/.gemini/config/import_manifest.json.
    let server_exe = omar_server_exe()?;
    let context_file = materialize_mcp_context_file(context)?;
    let home = std::env::var("HOME").ok()?;
    let config_dir = PathBuf::from(home).join(".gemini").join("config");
    let plugins_dir = config_dir.join("plugins");
    std::fs::create_dir_all(&plugins_dir).ok()?;
    let key = format!("omar-ea-{}", context.ea_id);
    let plugin_dir = plugins_dir.join(&key);
    std::fs::create_dir_all(&plugin_dir).ok()?;

    if let Ok(entries) = std::fs::read_dir(&plugins_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path == plugin_dir {
                continue;
            }
            if let Some(name) = entry.file_name().to_str() {
                if name.starts_with("omar-ea-") {
                    let ctx_path = path
                        .join("mcp_config.json")
                        .canonicalize()
                        .ok()
                        .and_then(|config| std::fs::read_to_string(config).ok())
                        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                        .and_then(|v| {
                            v.get("mcpServers")
                                .and_then(|servers| servers.get(name))
                                .and_then(|server| server.get("args"))
                                .and_then(|args| args.as_array())
                                .and_then(|args| args.last())
                                .and_then(|arg| arg.as_str())
                                .map(PathBuf::from)
                        });
                    if ctx_path.map(|p| !p.exists()).unwrap_or(false) {
                        let _ = std::fs::remove_dir_all(path);
                    }
                }
            }
        }
    }

    let plugin = serde_json::json!({
        "name": key.clone(),
        "version": "0.0.0",
        "description": "OMAR MCP server registration for this Executive Assistant"
    });
    let mut servers = serde_json::Map::new();
    servers.insert(
        key.clone(),
        serde_json::json!({
        "command": server_exe.display().to_string(),
        "args": ["mcp-server", "--context-file", context_file.display().to_string()],
        }),
    );
    let config = serde_json::json!({
        "mcpServers": servers
    });

    let plugin_path = plugin_dir.join("plugin.json");
    let config_path = plugin_dir.join("mcp_config.json");
    let manifest_path = config_dir.join("import_manifest.json");
    let plugin_payload = serde_json::to_vec_pretty(&plugin).ok()?;
    let config_payload = serde_json::to_vec_pretty(&config).ok()?;
    write_private_file(&plugin_path, &plugin_payload).ok()?;
    write_private_file(&config_path, &config_payload).ok()?;

    let mut manifest = match std::fs::read_to_string(&manifest_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
    {
        Some(v) if v.is_object() => v,
        _ => serde_json::json!({}),
    };
    let mut imports = manifest
        .get("imports")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    imports.retain(|entry| {
        let Some(name) = entry.get("name").and_then(|name| name.as_str()) else {
            return true;
        };
        if name == key {
            return false;
        }
        if let Some(ea) = name.strip_prefix("omar-ea-") {
            return plugins_dir.join(format!("omar-ea-{ea}")).exists();
        }
        true
    });
    imports.push(serde_json::json!({
        "name": key,
        "source": "local-install",
        "importedAt": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "components": ["installed"],
    }));
    manifest["imports"] = serde_json::Value::Array(imports);
    let manifest_payload = serde_json::to_vec_pretty(&manifest).ok()?;
    write_private_file(&manifest_path, &manifest_payload).ok()?;
    Some(())
}

fn antigravity_config_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".gemini").join("config"))
}

fn rewrite_antigravity_manifest_without<F>(keep_import: F) -> Result<()>
where
    F: Fn(&str) -> bool,
{
    let Some(config_dir) = antigravity_config_dir() else {
        return Ok(());
    };
    let manifest_path = config_dir.join("import_manifest.json");
    let Some(mut manifest) = std::fs::read_to_string(&manifest_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .filter(|v| v.is_object())
    else {
        return Ok(());
    };
    let Some(imports) = manifest.get("imports").and_then(|v| v.as_array()) else {
        return Ok(());
    };
    let retained: Vec<_> = imports
        .iter()
        .filter(|entry| {
            entry
                .get("name")
                .and_then(|name| name.as_str())
                .map(&keep_import)
                .unwrap_or(true)
        })
        .cloned()
        .collect();
    manifest["imports"] = serde_json::Value::Array(retained);
    write_private_file(&manifest_path, &serde_json::to_vec_pretty(&manifest)?)?;
    Ok(())
}

pub(crate) fn remove_omar_antigravity_mcp_config(ea_id: EaId) -> Result<()> {
    let Some(config_dir) = antigravity_config_dir() else {
        return Ok(());
    };
    let key = format!("omar-ea-{ea_id}");
    let plugin_dir = config_dir.join("plugins").join(&key);
    if plugin_dir.exists() {
        std::fs::remove_dir_all(&plugin_dir)?;
    }
    rewrite_antigravity_manifest_without(|name| name != key)
}

pub(crate) fn remove_all_omar_antigravity_mcp_configs() -> Result<()> {
    let Some(config_dir) = antigravity_config_dir() else {
        return Ok(());
    };
    let plugins_dir = config_dir.join("plugins");
    if let Ok(entries) = std::fs::read_dir(&plugins_dir) {
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_str()
                .map(|name| name.starts_with("omar-ea-"))
                .unwrap_or(false)
            {
                std::fs::remove_dir_all(entry.path())?;
            }
        }
    }
    rewrite_antigravity_manifest_without(|name| !name.starts_with("omar-ea-"))
}

/// Build a CLI command with system prompt loaded from a file via native flag.
///
/// - `prompt_file`: absolute path to the prompt .md file
/// - `substitutions`: `(pattern, replacement)` pairs for sed; empty = use `cat`
///
/// Detects backend from `base_command`:
///   - claude  → `--system-prompt "$(cat '<path>')"` plus native wake-tool denylist
///   - codex   → `-c "developer_instructions='''$(cat '<path>')'''"` plus scheduled-task disable
///   - cursor  → positional arg `"Load the <path> file and follow the instructions."`
///   - agy → `-i "$(cat '<path>')"` with an EA-scoped MCP entry in
///     `~/.gemini/config/plugins/omar-ea-<id>/mcp_config.json`
///   - opencode → MCP env only (no `--prompt`); the agent prompt is delivered
///     after spawn via tmux because opencode's `--prompt` is treated as the
///     first **user** message (not system role) and the LLM responds by
///     asking the user to fill in the fields described in the prompt
///   - unknown → returns `base_command` unchanged
pub fn build_agent_command(
    base_command: &str,
    prompt_file: &Path,
    substitutions: &[(&str, &str)],
    mcp_context: &McpLaunchContext,
) -> String {
    let base_command = ensure_codex_runtime_flags(base_command);
    let path_str = prompt_file.display().to_string();
    let prompt_path = shell_single_quote(&path_str);
    let shell_expr = if substitutions.is_empty() {
        format!("$(cat {})", prompt_path)
    } else {
        let sed_script: String = substitutions
            .iter()
            .map(|(pat, repl)| format!("s|{}|{}|g", pat, sed_escape(repl)))
            .collect::<Vec<_>>()
            .join("; ");
        format!("$(sed '{}' {})", sed_script, prompt_path)
    };

    // Per-backend MCP wiring. Each helper returns None only on an IO-level
    // failure (omar_dir unwritable, current_exe missing, serde error) — in
    // that case we fall back to launching the agent without MCP so the
    // session can still come up and the human operator sees the problem
    // via a degraded but visible agent, rather than a launch failure.
    match detect_backend(&base_command) {
        Some(BackendKind::Agy) => {
            let _ = ensure_antigravity_mcp_config(mcp_context);
            format!("TERM=xterm-256color {} -i \"{}\"", base_command, shell_expr)
        }
        Some(BackendKind::Claude) => match materialize_claude_mcp_config(mcp_context) {
            Some(mcp_config) => format!(
                "{} --system-prompt \"{}\" --mcp-config {} --disallowedTools {} {}",
                base_command,
                shell_expr,
                shell_single_quote(&mcp_config.display().to_string()),
                shell_single_quote(&backend_native_disallowed_tools_csv()),
                CLAUDE_INBOUND_SETTINGS
            ),
            None => format!(
                "{} --system-prompt \"{}\" {}",
                base_command, shell_expr, CLAUDE_INBOUND_SETTINGS
            ),
        },
        Some(BackendKind::Codex) => {
            // Preferred: everything in a per-pane home's `config.toml`, so the
            // TUI carries no `-c` and will attach to an app-server OMAR can
            // reach. Falls back to the flags if the home cannot be written, or
            // if the command already carries something codex will not attach
            // with — a home built for a TUI that refuses to join it is worse
            // than none, because the pane pays for the server twice over and
            // still ends up on the input box.
            let instructions = std::fs::read_to_string(prompt_file).map(|body| {
                substitutions
                    .iter()
                    .fold(body, |body, (pattern, replacement)| {
                        body.replace(pattern, replacement)
                    })
            });
            if !codex_refuses_to_attach(&base_command) {
                if let Some(home) = instructions
                    .ok()
                    .and_then(|instructions| codex_home_launch(mcp_context, &instructions))
                {
                    return codex_home_command(&home, &base_command);
                }
            }
            // No channel here, but codex still refuses to start in a directory
            // it has not been told to trust, and no agent answers that prompt.
            let mut cmd = format!(
                "{}{} -c \"developer_instructions='''{}'''\"",
                codex_trust_cwd(),
                base_command,
                shell_expr
            );
            if let Some(overrides) = codex_mcp_overrides(mcp_context) {
                cmd.push(' ');
                cmd.push_str(&overrides);
            }
            cmd
        }
        Some(BackendKind::Cursor) => {
            let rendered = materialize_prompt_file(prompt_file, substitutions);
            let _ = ensure_cursor_mcp_config(mcp_context);
            // Cursor Agent currently exposes no per-session tool deny flag in
            // interactive mode; the prompt-level wake policy is the enforcement
            // mechanism for this backend.
            format!(
                "{} --approve-mcps \"Load the '{}' file and follow the instructions.\"",
                base_command,
                rendered.display()
            )
        }
        Some(BackendKind::Stub) => {
            // The stub reads the endpoint and token straight from the topology
            // context, so it needs no MCP server of its own.
            let exe = omar_server_exe()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "omar".to_string());
            match materialize_mcp_context_file(mcp_context) {
                Some(context_file) => format!(
                    "{} stub-agent --context-file {}",
                    shell_single_quote(&exe),
                    shell_single_quote(&context_file.display().to_string())
                ),
                None => format!("{} stub-agent", shell_single_quote(&exe)),
            }
        }
        Some(BackendKind::Opencode) => {
            // opencode has no `--system-prompt`; `--prompt` is treated as the
            // first user message, which makes the LLM read agent.md
            // descriptively and ask back "What is your agent name?" etc.
            // Spawn opencode bare and let `spawn_worker` deliver the prompt
            // via tmux as a single combined first user message.
            let base_command = with_opencode_port(&base_command);
            match opencode_config_env(mcp_context) {
                Some(config) => format!(
                    "OPENCODE_CONFIG_CONTENT={} {}",
                    shell_single_quote(&config),
                    base_command
                ),
                None => base_command,
            }
        }
        None => base_command.to_string(),
    }
}

/// Build the manager (EA) command with memory + notes baked in, and return
/// the CLI command together with an optional cwd override the caller must
/// honor when launching the tmux session.
///
/// Manager prompts can be huge (template + memory + notes). On Linux any
/// single argv element above ~128 KB (`MAX_ARG_STRLEN`) makes `execve`
/// return `E2BIG` (`Argument list too long`), which manifests as a tmux
/// session dying inside `omar manager start` with an opaque "can't find
/// session" error. Two complementary defenses:
///
/// 1. **claude** uses the native `--system-prompt-file <path>` flag, so
///    the prompt never touches argv at all and is unbounded.
/// 2. **codex / agy / opencode** keep the legacy inline shell-expansion
///    path because their auto-loaded prompt/config files are anchored at the
///    agent's *working root*, not at the process cwd. Setting cwd = a per-EA
///    workspace dir would either silently load the wrong project context (the
///    one at the user's working root) or force the manager to operate in a dir
///    that isn't the user's project. A bounded truncation cap in `memory.rs`
///    (see `truncate_for_prompt`) keeps the inlined prompt comfortably
///    under `MAX_ARG_STRLEN`.
///
/// Cursor was already file-based via `materialize_prompt_file` and is
/// unaffected. Unknown backends fall through to the inline path so the
/// manager still launches in a degraded but visible state.
///
/// The `Option<PathBuf>` in the return type is reserved for future
/// backends that gain a real workspace mode; currently only claude could
/// in principle benefit, and it doesn't need a cwd override either.
pub fn build_ea_command(
    base_command: &str,
    ea_id: EaId,
    ea_name: &str,
    omar_dir: &Path,
    mcp_context: &McpLaunchContext,
) -> (String, Option<PathBuf>) {
    let prompt_file = prompts_dir(omar_dir).join("executive-assistant.md");
    let state_dir = ea::ea_state_dir(ea_id, omar_dir);
    let mem = memory::load_memory_from(&state_dir);

    let notes = memory::load_manager_notes(omar_dir, ea_id);
    let prompt_content = std::fs::read_to_string(&prompt_file).unwrap_or_default();

    // Write combined prompt (template + memory) to EA-scoped directory
    let combined_path = state_dir.join("ea_prompt_combined.md");
    std::fs::create_dir_all(&state_dir).ok();

    let combined = match (mem.is_empty(), notes.is_empty()) {
        (true, true) => prompt_content,
        (false, true) => format!(
            "{}\n\n---\n\n## Current OMAR State (from previous session)\n\n{}",
            prompt_content, mem
        ),
        (true, false) => format!(
            "{}\n\n---\n\n## Manager Notes (from previous session)\n\n{}",
            prompt_content, notes
        ),
        (false, false) => format!(
            "{}\n\n---\n\n## Current OMAR State (from previous session)\n\n{}\n\n## Manager Notes (from previous session)\n\n{}",
            prompt_content, mem, notes
        ),
    };
    // For claude (file flag) we resolve `{{EA_ID}}` / `{{EA_NAME}}` on
    // disk. The other backends still go through `build_agent_command`,
    // which pipes the file through sed at launch time, so we leave the
    // placeholders intact for them and write a separate, pre-resolved
    // file for claude.
    std::fs::write(&combined_path, &combined).ok();
    let backend = detect_backend(base_command);

    match backend {
        Some(BackendKind::Claude) => {
            let resolved = combined
                .replace("{{EA_ID}}", &ea_id.to_string())
                .replace("{{EA_NAME}}", ea_name);
            std::fs::write(&combined_path, &resolved).ok();
            let base_command = ensure_codex_runtime_flags(base_command);
            let cmd = match materialize_claude_mcp_config(mcp_context) {
                Some(mcp_config) => format!(
                    "{} --system-prompt-file {} --mcp-config {} --disallowedTools {} {}",
                    base_command,
                    shell_single_quote(&combined_path.display().to_string()),
                    shell_single_quote(&mcp_config.display().to_string()),
                    shell_single_quote(&backend_native_disallowed_tools_csv()),
                    CLAUDE_INBOUND_SETTINGS
                ),
                None => format!(
                    "{} --system-prompt-file {} {}",
                    base_command,
                    shell_single_quote(&combined_path.display().to_string()),
                    CLAUDE_INBOUND_SETTINGS
                ),
            };
            (cmd, None)
        }
        _ => {
            // Inline path for codex/agy/opencode/cursor/unknown. The
            // truncation cap in memory.rs keeps the rendered prompt under
            // `MAX_ARG_STRLEN` even when notes/memory grow large.
            let cmd = build_agent_command(
                base_command,
                &combined_path,
                &[("{{EA_ID}}", &ea_id.to_string()), ("{{EA_NAME}}", ea_name)],
                mcp_context,
            );
            (cmd, None)
        }
    }
}

/// Start the manager agent session for a specific EA.
pub fn start_manager(
    client: &TmuxClient,
    command: &str,
    ea_id: EaId,
    ea_name: &str,
    omar_dir: &Path,
    base_prefix: &str,
    options: &ManagerRuntimeOptions,
) -> Result<()> {
    let start = Instant::now();
    let (session, result) = ensure_manager_session(
        client,
        command,
        ea_id,
        ea_name,
        omar_dir,
        base_prefix,
        options,
    )?;

    if result == ManagerEnsureResult::AlreadyRunning {
        println!("Manager session already exists. Attaching...");
    } else {
        metrics::record_manager_start(ea_id, &session, true, start.elapsed().as_millis() as u64);
        println!("Attaching to manager session...");
    }
    client.attach_session(&session)?;

    Ok(())
}

/// Ensure the manager agent session for a specific EA exists, without
/// attaching to it. If the existing manager is live but running a different
/// known backend than requested, replace only that manager session.
pub fn ensure_manager_session(
    client: &TmuxClient,
    command: &str,
    ea_id: EaId,
    ea_name: &str,
    omar_dir: &Path,
    base_prefix: &str,
    options: &ManagerRuntimeOptions,
) -> Result<(String, ManagerEnsureResult)> {
    let session = ea::ea_manager_session(ea_id, base_prefix);
    let mut result = ManagerEnsureResult::Started;

    if client.has_session(&session)? {
        if client.session_has_live_pane(&session)? {
            let requested_backend = command_backend_name(command);
            let existing_backend = client
                .get_pane_command(&session)
                .ok()
                .and_then(|pane_command| command_backend_name(&pane_command))
                .or_else(|| {
                    client
                        .get_pane_process_command(&session)
                        .ok()
                        .and_then(|process_command| command_backend_name(&process_command))
                });

            if requested_backend.is_some()
                && existing_backend.is_some()
                && requested_backend != existing_backend
            {
                client.kill_session(&session)?;
                result = ManagerEnsureResult::ReplacedBackend;
            } else {
                return Ok((session, ManagerEnsureResult::AlreadyRunning));
            }
        } else {
            client.kill_session(&session)?;
        }
    }

    // Build command with EA system prompt + memory baked in. For backends
    // whose prompt is now loaded from a workspace file (codex/agy/opencode)
    // the build also returns the cwd that backend must be launched in for
    // auto-discovery to work.
    let (cmd, workspace_cwd) = build_ea_command(
        command,
        ea_id,
        ea_name,
        omar_dir,
        &McpLaunchContext {
            omar_dir: omar_dir.to_path_buf(),
            ea_id,
            session_prefix: base_prefix.to_string(),
            default_command: command.to_string(),
            default_workdir: options.default_workdir.clone(),
            health_idle_warning: options.health_idle_warning,
            tmux_server: current_tmux_server(),
            topology: None,
            serve: options.serve.clone(),
        },
    );

    // Create manager session — system prompt set at process start
    println!("Starting manager agent (EA {})...", ea_id);
    let cwd = match workspace_cwd {
        Some(p) => p.to_string_lossy().into_owned(),
        None => std::env::current_dir()?.to_string_lossy().into_owned(),
    };
    client.new_session(&session, &cmd, Some(&cwd))?;

    // Give it time to start
    thread::sleep(Duration::from_secs(2));
    Ok((session, result))
}

/// Run the manager in orchestration mode (interactive)
pub fn run_manager_orchestration(
    client: &TmuxClient,
    command: &str,
    ea_id: EaId,
    ea_name: &str,
    omar_dir: &Path,
    base_prefix: &str,
    options: &ManagerRuntimeOptions,
) -> Result<()> {
    let session = ea::ea_manager_session(ea_id, base_prefix);

    println!("=== OMAR Manager Orchestration Mode (EA {}) ===\n", ea_id);

    // Check if manager exists
    if !client.has_session(&session)? {
        println!("No manager session found. Starting one...");
        start_manager(
            client,
            command,
            ea_id,
            ea_name,
            omar_dir,
            base_prefix,
            options,
        )?;
        return Ok(());
    }

    loop {
        // Get user input
        print!("\n[OMAR] Enter command (or 'help'): ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim();

        match input {
            "help" | "h" => {
                print_help();
            }
            "status" | "s" => {
                show_status(client, &session)?;
            }
            "attach" | "a" => {
                client.attach_session(&session)?;
            }
            "check" | "c" => {
                check_manager_output(client, &session)?;
            }
            "approve" | "y" => {
                approve_plan(client, command, &session, ea_id, omar_dir, base_prefix)?;
            }
            "reject" | "n" => {
                reject_plan(client, &session)?;
            }
            "quit" | "q" => {
                println!("Exiting orchestration mode.");
                break;
            }
            _ if input.starts_with("send ") => {
                let rest = &input[5..];
                if let Some((target, msg)) = rest.split_once(' ') {
                    send_to_agent(client, target, msg)?;
                } else {
                    println!("Usage: send <agent-name> <message>");
                }
            }
            _ if !input.is_empty() => {
                // Send to manager as a request
                send_to_manager(client, &session, input)?;
            }
            _ => {}
        }
    }

    Ok(())
}

fn print_help() {
    println!(
        r#"
OMAR Manager Commands:
  <text>        Send request to manager agent
  status (s)    Show all agent status
  attach (a)    Attach to manager session
  check (c)     Check manager's latest output for plans
  approve (y)   Approve the proposed plan
  reject (n)    Reject the proposed plan
  send <agent> <msg>  Send message to specific agent
  quit (q)      Exit orchestration mode
"#
    );
}

fn show_status(client: &TmuxClient, session: &str) -> Result<()> {
    println!("\n=== Agent Status ===");

    // Show manager
    if client.has_session(session)? {
        let output = client.capture_pane(session, 3)?;
        println!("Manager: Active");
        println!(
            "  Last output: {}",
            output.lines().last().unwrap_or("(none)")
        );
    } else {
        println!("Manager: Not running");
    }

    // Show workers
    let sessions = client.list_sessions()?;
    if sessions.is_empty() {
        println!("\nNo worker agents running.");
    } else {
        println!("\nWorkers:");
        for s in sessions {
            let output = client.capture_pane(&s.name, 1).unwrap_or_default();
            let short_name = s.name.strip_prefix(client.prefix()).unwrap_or(&s.name);
            println!("  {}: {}", short_name, output.trim());
        }
    }

    Ok(())
}

fn check_manager_output(client: &TmuxClient, session: &str) -> Result<()> {
    let output = client.capture_pane(session, 50)?;

    if let Some(msg) = parse_manager_message(&output) {
        match msg {
            ManagerMessage::Plan {
                description,
                agents,
            } => {
                println!("\n=== Proposed Plan ===");
                println!("Goal: {}\n", description);
                println!("Agents:");
                for (i, agent) in agents.iter().enumerate() {
                    println!("  {}. {} ({})", i + 1, agent.name, agent.role);
                    println!("     Task: {}", agent.task);
                    if !agent.depends_on.is_empty() {
                        println!("     Depends on: {}", agent.depends_on.join(", "));
                    }
                }
                println!("\nApprove this plan? Use 'approve' or 'reject'");
            }
            ManagerMessage::Send { target, message } => {
                println!("Manager wants to send to '{}': {}", target, message);
            }
            ManagerMessage::Query { target } => {
                println!("Manager querying status of: {}", target);
            }
            ManagerMessage::Complete { summary } => {
                println!("Manager reports completion: {}", summary);
            }
        }
    } else {
        println!("No structured plan found in manager output.");
        println!("Recent output:");
        for line in output.lines().rev().take(10) {
            println!("  {}", line);
        }
    }

    Ok(())
}

fn approve_plan(
    client: &TmuxClient,
    command: &str,
    session: &str,
    ea_id: EaId,
    omar_dir: &Path,
    base_prefix: &str,
) -> Result<()> {
    let output = client.capture_pane(session, 50)?;

    if let Some(ManagerMessage::Plan {
        description,
        agents,
    }) = parse_manager_message(&output)
    {
        println!("\nApproving plan: {}", description);
        println!("Spawning {} worker agents...\n", agents.len());

        for agent in &agents {
            spawn_worker(client, agent, command, ea_id, omar_dir, base_prefix)?;
        }

        // Notify manager that plan was approved
        let approval_msg = format!(
            "Plan approved. {} agents spawned: {}",
            agents.len(),
            agents
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        send_to_manager(client, session, &approval_msg)?;

        println!("\nAll agents spawned. Use 'status' to monitor progress.");
    } else {
        println!("No plan found to approve. Use 'check' to see manager output.");
    }

    Ok(())
}

fn reject_plan(client: &TmuxClient, session: &str) -> Result<()> {
    print!("Reason for rejection: ");
    io::stdout().flush()?;

    let mut reason = String::new();
    io::stdin().read_line(&mut reason)?;

    send_to_manager(
        client,
        session,
        &format!("Plan rejected. Reason: {}", reason.trim()),
    )?;
    println!("Rejection sent to manager.");

    Ok(())
}

fn send_to_manager(client: &TmuxClient, session: &str, message: &str) -> Result<()> {
    client.deliver_prompt(session, message, &DeliveryOptions::default())?;
    println!("Sent to manager: {}", message);
    Ok(())
}

fn send_to_agent(client: &TmuxClient, agent: &str, message: &str) -> Result<()> {
    let session_name = client.session_for(agent);

    if !client.has_session(&session_name)? {
        println!("Agent '{}' not found.", agent);
        return Ok(());
    }

    client.deliver_prompt(&session_name, message, &DeliveryOptions::default())?;
    println!("Sent to {}: {}", agent, message);
    Ok(())
}

fn spawn_worker(
    client: &TmuxClient,
    agent: &ProposedAgent,
    command: &str,
    ea_id: EaId,
    omar_dir: &Path,
    base_prefix: &str,
) -> Result<()> {
    let session_name = client.session_for(&agent.name);

    if client.has_session(&session_name)? {
        println!("  {} - already exists, skipping", agent.name);
        return Ok(());
    }

    // Build command with worker system prompt (template vars substituted via sed)
    let parent_name = "ea";
    let prompt_file = prompts_dir(omar_dir).join("agent.md");
    let cmd = build_agent_command(
        command,
        &prompt_file,
        &[("{{TASK}}", &agent.task), ("{{EA_ID}}", &ea_id.to_string())],
        &McpLaunchContext {
            omar_dir: omar_dir.to_path_buf(),
            ea_id,
            session_prefix: base_prefix.to_string(),
            default_command: command.to_string(),
            default_workdir: ".".to_string(),
            health_idle_warning: 15,
            tmux_server: current_tmux_server(),
            topology: None,
            serve: None,
        },
    );

    // Create worker session — system prompt set at process start
    client.new_session(
        &session_name,
        &cmd,
        Some(&std::env::current_dir()?.to_string_lossy()),
    )?;

    // Wait for backend readiness when possible, then deliver an explicit
    // first task message so workers begin execution deterministically.
    // If markers succeed, the TUI is proven ready; skip require_initial_change
    // (a fresh Claude Code banner stays pixel-stable after drawing, so any
    // extra "wait for a change" would time out).
    let markers_proved_ready = if let Some(kind) = detect_backend(command) {
        let markers = crate::tmux::backend_readiness_markers(kind.canonical_name());
        if markers.is_empty() {
            false
        } else {
            let detected = client.wait_for_markers(
                &session_name,
                markers,
                Duration::from_secs(60),
                Duration::from_millis(250),
            );
            if !detected {
                println!(
                    "  {} - readiness markers timed out; attempting delivery anyway",
                    agent.name
                );
            }
            detected
        }
    } else {
        false
    };

    // opencode has no system-prompt flag, so build_agent_command spawns it
    // bare. Inline the rendered agent.md content here so the worker receives
    // its instructions plus the YOUR NAME header in a single user message.
    let header = format!(
        "YOUR NAME: {}\nYOUR PARENT: {}\nYOUR TASK: {}",
        agent.name, parent_name, agent.task
    );
    let initial_msg = if detect_backend(command) == Some(BackendKind::Opencode) {
        let rendered = materialize_prompt_file(
            &prompt_file,
            &[("{{TASK}}", &agent.task), ("{{EA_ID}}", &ea_id.to_string())],
        );
        let body = std::fs::read_to_string(&rendered).unwrap_or_default();
        format!("{}\n\n---\n\n{}", body, header)
    } else {
        header
    };
    let opts = DeliveryOptions {
        startup_timeout: Duration::from_secs(45),
        stable_quiet: Duration::from_millis(800),
        verify_timeout: Duration::from_secs(6),
        max_retries: 4,
        poll_interval: Duration::from_millis(120),
        retry_delay: Duration::from_millis(250),
        require_initial_change: !markers_proved_ready,
    };
    client
        .deliver_prompt(&session_name, &initial_msg, &opts)
        .map_err(|e| anyhow::anyhow!("failed to deliver initial task to {}: {}", agent.name, e))?;

    // Persist worker task description to EA-scoped state dir
    let state_dir = ea::ea_state_dir(ea_id, omar_dir);
    memory::save_worker_task_in(&state_dir, &session_name, &agent.task);
    memory::save_agent_parent_in(
        &state_dir,
        &session_name,
        &ea::ea_manager_session(ea_id, base_prefix),
    );

    println!("  {} - spawned ({})", agent.name, agent.role);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_deleted_suffix_removes_trailing_marker() {
        // The Linux "(deleted)" marker on a replaced binary is stripped.
        assert_eq!(
            strip_deleted_suffix(Path::new("/home/u/.cargo/bin/omar (deleted)")),
            PathBuf::from("/home/u/.cargo/bin/omar")
        );
        // A clean path is returned unchanged.
        assert_eq!(
            strip_deleted_suffix(Path::new("/home/u/.cargo/bin/omar")),
            PathBuf::from("/home/u/.cargo/bin/omar")
        );
        // The marker is only stripped when it is a true suffix, not when the
        // same substring appears earlier in the path.
        assert_eq!(
            strip_deleted_suffix(Path::new("/home/u/omar (deleted)/bin/omar")),
            PathBuf::from("/home/u/omar (deleted)/bin/omar")
        );
    }

    #[test]
    fn omar_server_exe_returns_existing_binary() {
        // The running test binary exists, so resolution returns a real path
        // (never one carrying the "(deleted)" marker).
        let exe = omar_server_exe().expect("current exe should resolve");
        assert!(exe.exists(), "resolved exe should exist on disk: {exe:?}");
        assert!(
            !exe.to_string_lossy().ends_with(" (deleted)"),
            "resolved exe must not retain the (deleted) marker: {exe:?}"
        );
    }

    /// A minimal MCP context scoped to a caller-supplied temp dir. Tests that
    /// exercise only command-string shape (no filesystem assertions) can pass
    /// any path — the per-backend materializers return `None` silently on IO
    /// failure, which is part of what we're asserting on. Tests that also
    /// need the context files on disk must use a real `tempfile::tempdir()`.
    #[test]
    fn serve_context_survives_the_context_file_round_trip() {
        // The EA's MCP sidecar is a separate process that reads this file back,
        // so the serve endpoint and token have to serialise or the operator
        // tools silently never work.
        let omar_dir =
            std::env::temp_dir().join(format!("omar-serve-ctx-{}", uuid::Uuid::new_v4()));
        let context = McpLaunchContext {
            serve: Some(ServeMcpContext {
                endpoint: "127.0.0.1:7340".to_string(),
                token: "secret-token".to_string(),
            }),
            ..test_mcp_context(&omar_dir)
        };
        let encoded = serde_json::to_string(&context).expect("context serialises");
        let decoded: McpLaunchContext =
            serde_json::from_str(&encoded).expect("context deserialises");
        let serve = decoded.serve.expect("serve context survives");
        assert_eq!(serve.endpoint, "127.0.0.1:7340");
        assert_eq!(serve.token, "secret-token");

        // Contexts written before this field existed still load.
        let legacy = serde_json::to_value(&context)
            .map(|mut value| {
                value.as_object_mut().unwrap().remove("serve");
                value
            })
            .expect("legacy context");
        let decoded: McpLaunchContext =
            serde_json::from_value(legacy).expect("legacy context deserialises");
        assert!(decoded.serve.is_none());
    }

    fn test_mcp_context(omar_dir: &Path) -> McpLaunchContext {
        McpLaunchContext {
            omar_dir: omar_dir.to_path_buf(),
            ea_id: 0,
            session_prefix: "omar-agent-".to_string(),
            default_command: "claude".to_string(),
            default_workdir: ".".to_string(),
            health_idle_warning: 15,
            tmux_server: None,
            topology: None,
            serve: None,
        }
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &Path) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
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

    #[test]
    fn embedded_prompts_forbid_backend_native_wake_tools() {
        for prompt in [PROMPT_EA, PROMPT_AGENT] {
            assert!(prompt.contains("MUST use the OMAR MCP tool `schedule_omar_event`"));
            assert!(prompt.contains("ScheduleWakeup"));
            assert!(prompt.contains("scheduled tasks"));
            assert!(prompt.contains("If a non-OMAR wake/reminder tool is visible, ignore it"));
        }
    }

    #[test]
    fn test_build_agent_command_claude() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = build_agent_command(
            "claude --some-flag",
            Path::new("/tmp/prompts/ea.md"),
            &[],
            &test_mcp_context(dir.path()),
        );
        assert!(
            cmd.starts_with("claude --some-flag --system-prompt \"$(cat '/tmp/prompts/ea.md')\""),
            "unexpected claude command: {cmd}"
        );
        assert!(cmd.contains("--mcp-config"));
        assert!(cmd.contains("--disallowedTools"));
        // Wake-tool denylist (overlap with schedule_omar_event).
        assert!(cmd.contains("ScheduleWakeup"));
        assert!(cmd.contains("scheduled_tasks"));
        // Subagent-dispatcher denylist (overlap with spawn_agent). The Claude
        // Code built-in `Task` tool is the canonical example.
        assert!(
            cmd.contains(",Task,") || cmd.contains("Task,task,"),
            "claude --disallowedTools must include the built-in Task tool: {cmd}"
        );
        assert!(cmd.contains("dispatch_agent"));
    }

    #[test]
    fn a_claude_session_accepts_events_from_omar_over_its_peer_socket() {
        // Without this the session holds OMAR's messages behind an approval
        // dialog that covers the composer, and no event is ever delivered.
        let dir = tempfile::tempdir().unwrap();
        for base in ["claude --dangerously-skip-permissions", "claude"] {
            let cmd = build_agent_command(
                base,
                Path::new("/tmp/prompts/ea.md"),
                &[],
                &test_mcp_context(dir.path()),
            );
            assert!(
                cmd.contains(r#"--settings '{"crossSessionInbound":"accept"}'"#),
                "claude must opt in to inbound peer messages: {cmd}"
            );
        }
    }

    #[test]
    fn the_ea_pane_accepts_events_from_omar_too() {
        // The EA manager is the most common receiver of scheduled events, and
        // it is built by a different function than the worker panes.
        let dir = tempfile::tempdir().unwrap();
        let (cmd, _) = build_ea_command(
            "claude --dangerously-skip-permissions",
            1,
            "ea-one",
            Path::new("/tmp/prompts/ea.md"),
            &test_mcp_context(dir.path()),
        );
        assert!(
            cmd.contains(r#"--settings '{"crossSessionInbound":"accept"}'"#),
            "the EA pane must opt in to inbound peer messages: {cmd}"
        );
    }

    #[test]
    fn only_claude_is_told_about_inbound_peer_messages() {
        // The flag is Claude Code's; handing it to another backend would be an
        // unrecognised argument at launch.
        let dir = tempfile::tempdir().unwrap();
        for base in [
            "codex --no-alt-screen",
            "opencode",
            "cursor agent --yolo",
            "agy --dangerously-skip-permissions",
        ] {
            let cmd = build_agent_command(
                base,
                Path::new("/tmp/prompts/ea.md"),
                &[],
                &test_mcp_context(dir.path()),
            );
            assert!(
                !cmd.contains("crossSessionInbound"),
                "{base} must not receive a Claude-only flag: {cmd}"
            );
        }
    }

    /// A tempdir short enough that a codex home under it can still bind a
    /// Unix socket. The default one lives under a long `/var/folders/...`
    /// path on macOS, which is over the `sun_path` limit before codex has
    /// added a single character of its own.
    fn short_tempdir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("omar-test-")
            .tempdir_in("/tmp")
            .unwrap()
    }

    #[test]
    fn test_build_agent_command_codex() {
        let dir = short_tempdir();
        let prompt = dir.path().join("ea.md");
        std::fs::write(&prompt, "be helpful, {{EA_NAME}}").unwrap();
        let cmd = build_agent_command(
            "codex --no-alt-screen",
            &prompt,
            &[("{{EA_NAME}}", "CapX")],
            &test_mcp_context(dir.path()),
        );

        // Nothing on codex's disqualifier list may reach the TUI, or it
        // refuses to attach to the app-server and there is no side channel.
        assert!(
            cmd.ends_with("codex --no-alt-screen --dangerously-bypass-approvals-and-sandbox"),
            "the TUI must be launched bare: {cmd}"
        );
        assert!(
            !cmd.contains(" -c "),
            "no config override may survive: {cmd}"
        );
        assert!(cmd.contains("export CODEX_HOME="), "{cmd}");
        assert!(
            cmd.contains("codex app-server --listen unix://"),
            "the pane must own an app-server: {cmd}"
        );
        // A background job dies with the pane's process group. A `trap` would
        // make the shell catch the hangup instead of dying of it, and a shell
        // sitting in the TUI never reaches the handler — so the pane would
        // survive its own kill and hold the server open.
        assert!(
            !cmd.contains("trap "),
            "trapping the hangup is what keeps the server alive: {cmd}"
        );
        assert!(
            cmd.contains(
                "while [ ! -S \"$CODEX_HOME/app-server-control/app-server-control.sock\" ]"
            ),
            "the TUI must wait for the server, or they race on a fresh home's \
             state database and codex refuses to start: {cmd}"
        );
        assert!(
            cmd.contains("trust_level = \"trusted\"") && cmd.contains("$(pwd -P"),
            "the launch cwd must be trusted or codex blocks on a prompt: {cmd}"
        );
        assert!(
            cmd.contains(r#"sed 's/[\\"]/\\&/g'"#),
            "the cwd goes in as a TOML quoted key and must be escaped: {cmd}"
        );
        assert!(
            cmd.contains("kill -0 \"$omar_srv\""),
            "the wait must end when the server dies: {cmd}"
        );

        // The configuration the flags used to carry now lives in the home.
        let home = crate::channel::codex_home(&cmd).expect("command names a codex home");
        let config = std::fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(config.contains("be helpful, CapX"), "{config}");
        assert!(config.contains("scheduled_tasks = false"), "{config}");
        assert!(config.contains("[mcp_servers.omar]"), "{config}");
        assert!(config.contains("mcp-server"), "{config}");
    }

    #[test]
    fn a_launch_without_a_side_channel_still_trusts_its_own_directory() {
        // codex blocks on "Do you trust the contents of this directory?" the
        // first time it runs anywhere new, and no agent ever answers it. The
        // side-channel launch writes the record into its own home; this path
        // uses the operator's, and used to leave the pane hanging forever.
        let dir = tempfile::tempdir().unwrap();
        let prompt = dir.path().join("ea.md");
        std::fs::write(&prompt, "be helpful").unwrap();
        let cmd = build_agent_command(
            // a disqualifying flag forces the fallback
            "codex --no-alt-screen -c model_reasoning_effort='\"high\"'",
            &prompt,
            &[],
            &test_mcp_context(dir.path()),
        );

        assert!(
            !cmd.contains("export CODEX_HOME="),
            "this must be the fallback, not the side-channel launch: {cmd}"
        );
        assert!(
            cmd.contains("trust_level = \"trusted\"") && cmd.contains("$(pwd -P"),
            "the launch cwd must be trusted or the pane hangs on a prompt: {cmd}"
        );
        assert!(
            cmd.contains("${CODEX_HOME:-$HOME/.codex}"),
            "with no per-pane home it must write the operator's own: {cmd}"
        );
    }

    #[test]
    fn the_socket_bound_is_the_one_codex_enforces_not_the_kernel() {
        // Measured against codex 0.147.0: a 95-byte socket path binds, 96
        // fails with "path must be shorter than SUN_LEN". macOS itself would
        // take 103, and a path in that gap is the bad case — it passes the
        // kernel, codex refuses it, and the channel is lost silently while a
        // home is left behind for nothing.
        assert_eq!(
            SUN_PATH_MAX, 96,
            "the guard must reject the paths codex rejects, not the ones the OS does"
        );
    }

    #[test]
    fn a_codex_home_too_deep_for_a_socket_falls_back_to_the_flags() {
        // A pane whose home cannot hold a socket codex will bind gets no side
        // channel — but it must still launch, with the configuration it always
        // had.
        let dir = short_tempdir();
        let deep = dir.path().join("d".repeat(90));
        std::fs::create_dir_all(&deep).unwrap();
        let prompt = deep.join("ea.md");
        std::fs::write(&prompt, "be helpful").unwrap();
        let cmd = build_agent_command(
            "codex --no-alt-screen",
            &prompt,
            &[],
            &test_mcp_context(&deep),
        );
        assert!(
            cmd.contains(
                "codex --no-alt-screen --dangerously-bypass-approvals-and-sandbox \
                 -c \"developer_instructions='''$(cat '"
            ),
            "unexpected fallback command: {cmd}"
        );
        // The trust record is written first, so the pane does not stop on
        // a prompt before it reaches codex at all.
        assert!(
            cmd.starts_with("omar_home=") && cmd.contains("trust_level = \"trusted\""),
            "the fallback must trust its cwd before launching: {cmd}"
        );
        assert!(cmd.contains("mcp_servers.omar.command"));
        assert!(cmd.contains("mcp_servers.omar.args"));
        assert!(cmd.contains("-c features.scheduled_tasks=false"));
    }

    #[test]
    fn a_codex_that_will_not_attach_is_launched_the_old_way() {
        // `spawn_agent` adds `-c model_reasoning_effort=…` for any agent given
        // one, and codex then refuses the app-server. Building a home anyway
        // would start a second codex nobody joins, spend the pane's readiness
        // budget waiting for a socket, and poll it for ninety seconds — to end
        // up on the input box regardless.
        let dir = short_tempdir();
        let prompt = dir.path().join("agent.md");
        std::fs::write(&prompt, "be helpful").unwrap();

        for base in [
            "codex --no-alt-screen -c model_reasoning_effort='\"high\"'",
            "codex --no-alt-screen --profile work",
            "codex --no-alt-screen --search",
            "codex --no-alt-screen --enable web_search",
            "codex --no-alt-screen --config model=\"o3\"",
        ] {
            let cmd = build_agent_command(base, &prompt, &[], &test_mcp_context(dir.path()));
            assert!(
                cmd.contains("developer_instructions='''"),
                "{base} must take the flag path: {cmd}"
            );
            assert!(
                crate::channel::codex_home(&cmd).is_none(),
                "{base} must not be given a home it cannot attach to: {cmd}"
            );
        }

        // The flags OMAR always adds are not on that list.
        for base in [
            "codex --no-alt-screen",
            "codex --no-alt-screen --dangerously-bypass-approvals-and-sandbox --model gpt-5.6-sol",
        ] {
            let cmd = build_agent_command(base, &prompt, &[], &test_mcp_context(dir.path()));
            assert!(
                crate::channel::codex_home(&cmd).is_some(),
                "{base} should still get a home: {cmd}"
            );
        }
    }

    #[test]
    fn a_home_whose_server_is_answering_is_never_deleted() {
        // The pane's TUI is a client of that server, and the home holds the
        // thread history it is running on. Deleting either out from under a
        // live pane takes the agent with it, so a live socket vetoes every
        // path that removes a home — however the session bookkeeping reads.
        let dir = short_tempdir();
        let root = dir.path();

        let live = root.join("live");
        std::fs::create_dir_all(crate::channel::codex_socket_path(&live).parent().unwrap())
            .unwrap();
        let _listener =
            std::os::unix::net::UnixListener::bind(crate::channel::codex_socket_path(&live))
                .unwrap();
        // Everything the bookkeeping could get wrong, at once: it claims a
        // session that does not exist, and it is older than the grace.
        std::fs::write(live.join(CODEX_HOME_SESSION_FILE), "omar-agent-gone-9z8y7x").unwrap();

        prune_stale_codex_homes(root);
        assert!(live.exists(), "a live app-server must veto the prune");

        // And the same veto covers the sweep of an earlier home for a reused
        // session name.
        let newer = root.join("newer");
        std::fs::create_dir_all(&newer).unwrap();
        claim_codex_home(&newer, "omar-agent-gone-9z8y7x");
        assert!(live.exists(), "a live app-server must veto the sweep too");
    }

    #[test]
    fn an_earlier_home_for_the_same_pane_is_reclaimed() {
        // `ensure_manager_session` kills a pane and makes another with the
        // same name, so the old home's recorded session resolves again and it
        // would never be pruned.
        let dir = short_tempdir();
        let root = dir.path();
        let old = root.join("old");
        let new = root.join("new");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(old.join(CODEX_HOME_SESSION_FILE), "omar-agent-1-work").unwrap();

        claim_codex_home(&new, "omar-agent-1-work");

        assert!(!old.exists(), "the previous home for this pane must go");
        assert_eq!(
            std::fs::read_to_string(new.join(CODEX_HOME_SESSION_FILE)).unwrap(),
            "omar-agent-1-work"
        );
    }

    #[test]
    fn a_codex_home_outlives_its_pane_only_until_the_next_launch() {
        // The app-server dies with the pane on its own; the directory does
        // not, and each launch mints a multi-megabyte one.
        let dir = short_tempdir();
        let root = dir.path();

        let finished = root.join("finished");
        std::fs::create_dir_all(&finished).unwrap();
        std::fs::write(
            finished.join(CODEX_HOME_SESSION_FILE),
            "omar-agent-no-such-session-1a2b3c",
        )
        .unwrap();

        // Made by `codex_home_dir`, not yet claimed by provisioning.
        let launching = root.join("launching");
        std::fs::create_dir_all(&launching).unwrap();

        prune_stale_codex_homes(root);

        assert!(!finished.exists(), "a home whose pane is gone must go");
        assert!(
            launching.exists(),
            "a home that has not been claimed yet is mid-launch"
        );
    }

    #[test]
    fn a_codex_config_keeps_the_operators_own_settings_but_not_their_trust_records() {
        let config = codex_config_toml(
            "model = \"gpt-5.6-terra\"\n\
             [features]\n\
             web_search = true\n\
             [projects.\"/somewhere/else\"]\n\
             trust_level = \"trusted\"\n",
            "be helpful",
            Path::new("/usr/local/bin/omar"),
            Path::new("/home/ea/context.json"),
        );
        let parsed: toml::Table = toml::from_str(&config).expect("valid toml");

        assert_eq!(parsed["model"].as_str(), Some("gpt-5.6-terra"));
        assert_eq!(parsed["features"]["web_search"].as_bool(), Some(true));
        assert_eq!(parsed["features"]["scheduled_tasks"].as_bool(), Some(false));
        assert_eq!(
            parsed["developer_instructions"].as_str(),
            Some("be helpful")
        );
        assert_eq!(
            parsed["mcp_servers"]["omar"]["command"].as_str(),
            Some("/usr/local/bin/omar")
        );
        // The pane appends its own working directory as a trusted project.
        // Carrying the operator's table across could name that directory too,
        // and a table declared twice makes codex refuse to start at all.
        assert!(
            parsed.get("projects").is_none(),
            "trust records must not be carried over: {config}"
        );
    }

    #[test]
    fn an_unreadable_codex_config_still_yields_a_usable_one() {
        // The operator may have no `~/.codex/config.toml` at all, and a
        // damaged one must not take the pane down with it.
        for base in ["", "this is not toml [[["] {
            let config = codex_config_toml(
                base,
                "be helpful",
                Path::new("/usr/local/bin/omar"),
                Path::new("/home/ea/context.json"),
            );
            let parsed: toml::Table = toml::from_str(&config).expect("valid toml");
            assert_eq!(
                parsed["developer_instructions"].as_str(),
                Some("be helpful")
            );
            assert!(parsed["mcp_servers"]["omar"]["args"].is_array());
        }
    }

    #[test]
    fn test_build_ea_command_codex() {
        let dir = short_tempdir();
        let omar_dir = dir.path();
        let state_dir = ea::ea_state_dir(3, omar_dir);
        std::fs::create_dir_all(&state_dir).unwrap();
        let (cmd, workspace) = build_ea_command(
            "codex --no-alt-screen --dangerously-bypass-approvals-and-sandbox",
            3,
            "CapX",
            omar_dir,
            &test_mcp_context(omar_dir),
        );
        // codex keeps launching in the user's own project directory: its
        // `AGENTS.md` auto-discovery is anchored at the agent's working root
        // (`-C`), not the launch cwd, so a workspace-dir launch would either
        // load the wrong `AGENTS.md` or force the manager to operate outside
        // the user's project.
        assert!(
            cmd.ends_with("codex --no-alt-screen --dangerously-bypass-approvals-and-sandbox"),
            "unexpected codex manager command: {cmd}"
        );
        let home = crate::channel::codex_home(&cmd).expect("command names a codex home");
        let config = std::fs::read_to_string(home.join("config.toml"))
            .unwrap_or_else(|err| panic!("no config at {}: {err} ({cmd})", home.display()));
        assert!(config.contains("[mcp_servers.omar]"), "{config}");
        assert!(config.contains("mcp-server"), "{config}");
        assert!(config.contains("scheduled_tasks = false"), "{config}");
        // The EA's own name is substituted into the prompt before it lands in
        // the config rather than by a `sed` at launch.
        assert!(config.contains("CapX"), "{config}");
        assert!(
            workspace.is_none(),
            "codex manager must not override the launch cwd"
        );
    }

    #[test]
    fn test_build_agent_command_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = build_agent_command(
            "cursor agent --yolo",
            Path::new("/tmp/prompts/ea.md"),
            &[],
            &test_mcp_context(dir.path()),
        );
        assert!(cmd.contains("cursor agent --yolo --approve-mcps"));
        assert!(cmd.contains("Load the '/tmp/"));
    }

    #[test]
    fn test_build_agent_command_agy() {
        let _env_lock = global_home_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _home = EnvVarGuard::set("HOME", dir.path());
        let cmd = build_agent_command(
            "agy --dangerously-skip-permissions",
            Path::new("/tmp/prompts/ea.md"),
            &[],
            &test_mcp_context(dir.path()),
        );
        assert!(cmd.contains(
            "TERM=xterm-256color agy --dangerously-skip-permissions -i \"$(cat '/tmp/prompts/ea.md')\""
        ));
        let plugin = dir
            .path()
            .join(".gemini/config/plugins/omar-ea-0/plugin.json");
        let plugin = std::fs::read_to_string(plugin).unwrap();
        assert!(plugin.contains("\"omar-ea-0\""));
        let config = dir
            .path()
            .join(".gemini/config/plugins/omar-ea-0/mcp_config.json");
        let config = std::fs::read_to_string(config).unwrap();
        assert!(config.contains("\"omar-ea-0\""));
        assert!(config.contains("\"mcp-server\""));
        assert!(config.contains("\"--context-file\""));
        let manifest = dir.path().join(".gemini/config/import_manifest.json");
        let manifest = std::fs::read_to_string(manifest).unwrap();
        assert!(manifest.contains("\"omar-ea-0\""));
        assert!(manifest.contains("\"local-install\""));
        assert!(
            !cmd.contains("--allowed-mcp-server-names"),
            "agy does not advertise MCP config CLI flags"
        );
        assert!(
            !cmd.contains("--policy"),
            "agy does not advertise policy CLI flags"
        );
    }

    #[test]
    fn command_backend_name_detects_executable_tokens() {
        assert_eq!(
            command_backend_name("agy --dangerously-skip-permissions"),
            Some("agy")
        );
        assert_eq!(
            command_backend_name("env FOO=bar /opt/bin/codex --no-alt-screen"),
            Some("codex")
        );
        assert_eq!(command_backend_name("bash -lc 'echo hi'"), None);
    }

    #[test]
    fn test_remove_omar_antigravity_mcp_config_updates_plugin_and_manifest() {
        let _env_lock = global_home_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _home = EnvVarGuard::set("HOME", dir.path());
        let plugins_dir = dir.path().join(".gemini/config/plugins");
        std::fs::create_dir_all(plugins_dir.join("omar-ea-7")).unwrap();
        std::fs::create_dir_all(plugins_dir.join("other-plugin")).unwrap();
        let manifest_path = dir.path().join(".gemini/config/import_manifest.json");
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "imports": [
                    {"name": "omar-ea-7", "source": "local-install"},
                    {"name": "other-plugin", "source": "local-install"}
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        remove_omar_antigravity_mcp_config(7).unwrap();

        assert!(!plugins_dir.join("omar-ea-7").exists());
        assert!(plugins_dir.join("other-plugin").exists());
        let manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(manifest_path).unwrap()).unwrap();
        let imports = manifest["imports"].as_array().unwrap();
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0]["name"], "other-plugin");
    }

    #[test]
    fn test_remove_all_omar_antigravity_mcp_configs_preserves_user_plugins() {
        let _env_lock = global_home_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _home = EnvVarGuard::set("HOME", dir.path());
        let plugins_dir = dir.path().join(".gemini/config/plugins");
        std::fs::create_dir_all(plugins_dir.join("omar-ea-1")).unwrap();
        std::fs::create_dir_all(plugins_dir.join("omar-ea-2")).unwrap();
        std::fs::create_dir_all(plugins_dir.join("user-plugin")).unwrap();
        let manifest_path = dir.path().join(".gemini/config/import_manifest.json");
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "imports": [
                    {"name": "omar-ea-1", "source": "local-install"},
                    {"name": "omar-ea-2", "source": "local-install"},
                    {"name": "user-plugin", "source": "local-install"}
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        remove_all_omar_antigravity_mcp_configs().unwrap();

        assert!(!plugins_dir.join("omar-ea-1").exists());
        assert!(!plugins_dir.join("omar-ea-2").exists());
        assert!(plugins_dir.join("user-plugin").exists());
        let manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(manifest_path).unwrap()).unwrap();
        let imports = manifest["imports"].as_array().unwrap();
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0]["name"], "user-plugin");
    }

    #[test]
    fn test_build_agent_command_opencode() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = build_agent_command(
            "opencode",
            Path::new("/tmp/prompts/pm.md"),
            &[],
            &test_mcp_context(dir.path()),
        );
        assert!(cmd.contains("OPENCODE_CONFIG_CONTENT="));
        assert!(cmd.contains("\"mcp\""));
        assert!(cmd.contains("\"omar\""));
        assert!(cmd.contains("\"doom_loop\":\"deny\""));
        assert!(cmd.contains("\"ScheduleWakeup\":false"));
        // Subagent-dispatcher overlap with OMAR's spawn_agent.
        assert!(cmd.contains("\"Task\":false"));
        assert!(cmd.contains("\"dispatch_agent\":false"));
        // The prompt is still delivered via tmux after spawn, not as a flag.
        assert!(!cmd.contains("--prompt"));
        // ... but opencode is told a port, which is the only way it listens at
        // all — without one it talks to an in-process worker OMAR cannot reach.
        let port = cmd
            .split_whitespace()
            .skip_while(|token| *token != "--port")
            .nth(1)
            .expect("opencode must be given a port");
        assert!(
            port.parse::<u16>().is_ok_and(|port| port > 0),
            "port must be a real number: {port}"
        );
    }

    #[test]
    fn an_operators_own_opencode_port_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        for base in ["opencode --port 4096", "opencode --hostname 0.0.0.0"] {
            let cmd = build_agent_command(
                base,
                Path::new("/tmp/prompts/ea.md"),
                &[],
                &test_mcp_context(dir.path()),
            );
            assert_eq!(
                cmd.matches("--port").count(),
                usize::from(base.contains("--port")),
                "must not add a second port to {base}: {cmd}"
            );
        }
    }

    #[test]
    fn test_build_agent_command_env_wrapper_preserved() {
        // Backend detection must look past shell-env prefixes like
        // `env FOO=bar <backend>` so per-backend flags still get added.
        let dir = tempfile::tempdir().unwrap();
        let cmd = build_agent_command(
            "env ANTHROPIC_API_KEY=test claude --yolo",
            Path::new("/tmp/prompts/ea.md"),
            &[],
            &test_mcp_context(dir.path()),
        );
        assert!(cmd.starts_with("env ANTHROPIC_API_KEY=test claude --yolo --system-prompt"));
    }

    /// Regression for the codex-EA-manager-won't-start bug. With a very
    /// large notes file (here 200 KB), the legacy inline path generated a
    /// `-c "developer_instructions='''<huge>'''"` argv element that
    /// exceeded `MAX_ARG_STRLEN` (~128 KB) and crashed the manager spawn
    /// with `Argument list too long`. The file/workspace approach must
    /// keep every argv element comfortably under that limit regardless
    /// of prompt size.
    #[test]
    fn test_build_ea_command_handles_oversized_notes_for_all_backends() {
        // 200 KB of notes — well past MAX_ARG_STRLEN.
        let huge_notes: String = "x".repeat(200 * 1024);

        for backend in [
            "claude",
            "codex --no-alt-screen --dangerously-bypass-approvals-and-sandbox",
            "opencode",
            "agy --dangerously-skip-permissions",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let omar_dir = dir.path();
            let state_dir = ea::ea_state_dir(7, omar_dir);
            std::fs::create_dir_all(&state_dir).unwrap();
            std::fs::write(memory::manager_notes_path(omar_dir, 7), &huge_notes).unwrap();

            let (cmd, _ws) =
                build_ea_command(backend, 7, "Big", omar_dir, &test_mcp_context(omar_dir));

            // No single argv element (whitespace-separated token) may exceed
            // a safe ceiling — 96 KB leaves comfortable headroom under the
            // 128 KB MAX_ARG_STRLEN limit on Linux.
            const ARG_CEILING: usize = 96 * 1024;
            for tok in cmd.split_whitespace() {
                assert!(
                    tok.len() < ARG_CEILING,
                    "backend {backend}: argv token of {} bytes would risk \
                     exec(3) E2BIG (MAX_ARG_STRLEN ≈ 128 KB): {}",
                    tok.len(),
                    &tok[..tok.len().min(120)]
                );
            }
        }
    }

    #[test]
    fn test_build_agent_command_unknown_backend() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = build_agent_command(
            "vim",
            Path::new("/tmp/prompts/ea.md"),
            &[],
            &test_mcp_context(dir.path()),
        );
        assert_eq!(cmd, "vim");
    }

    #[test]
    fn test_build_agent_command_with_substitutions() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = build_agent_command(
            "claude",
            Path::new("/prompts/worker.md"),
            &[("{{TASK}}", "build it"), ("{{EA_ID}}", "0")],
            &test_mcp_context(dir.path()),
        );
        assert!(cmd.contains("s|{{TASK}}|build it|g"));
        assert!(cmd.contains("s|{{EA_ID}}|0|g"));
        assert!(cmd.contains("'/prompts/worker.md'"));
    }

    #[test]
    fn test_build_agent_command_with_ea_id() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = build_agent_command(
            "claude",
            Path::new("/prompts/agent.md"),
            &[("{{TASK}}", "do stuff"), ("{{EA_ID}}", "2")],
            &test_mcp_context(dir.path()),
        );
        assert!(cmd.contains("s|{{EA_ID}}|2|g"));
    }

    #[test]
    fn test_sed_escape() {
        assert_eq!(sed_escape("hello"), "hello");
        assert_eq!(sed_escape("a\\b"), "a\\\\b");
        assert_eq!(sed_escape("a|b"), "a\\|b");
        assert_eq!(sed_escape("a&b"), "a\\&b");
        assert_eq!(sed_escape("a\nb"), "a\\nb");
        // Combined
        assert_eq!(sed_escape("a\\|&\nb"), "a\\\\\\|\\&\\nb");
        // Single quotes must be escaped so they don't terminate the surrounding
        // shell single-quoted sed expression (BUG B fix).
        assert_eq!(sed_escape("it's"), "it'\\''s");
        assert_eq!(sed_escape("don't stop"), "don'\\''t stop");
    }

    #[test]
    fn test_build_ea_command_substitutes_ea_id() {
        let dir = tempfile::tempdir().unwrap();
        let omar_dir = dir.path();

        // Create state dir for EA
        let state_dir = ea::ea_state_dir(0, omar_dir);
        std::fs::create_dir_all(&state_dir).unwrap();

        let (cmd, workspace) = build_ea_command(
            "claude",
            0,
            "Default",
            omar_dir,
            &test_mcp_context(omar_dir),
        );
        // claude manager now uses --system-prompt-file (no sed substitution
        // expression in the command). EA_ID/EA_NAME are pre-substituted on
        // disk in the combined prompt file.
        assert!(
            workspace.is_none(),
            "claude manager doesn't need workspace cwd"
        );
        assert!(
            cmd.contains("--system-prompt-file"),
            "claude must use file flag: {cmd}"
        );
        assert!(
            !cmd.contains("s|{{EA_ID}}|"),
            "no sed expression expected: {cmd}"
        );
        let combined = state_dir.join("ea_prompt_combined.md");
        let body = std::fs::read_to_string(&combined).unwrap();
        assert!(
            !body.contains("{{EA_ID}}"),
            "combined prompt should have EA_ID resolved"
        );
        assert!(
            !body.contains("{{EA_NAME}}"),
            "combined prompt should have EA_NAME resolved"
        );
    }

    #[test]
    fn test_build_ea_command_writes_to_ea_scoped_dir() {
        let dir = tempfile::tempdir().unwrap();
        let omar_dir = dir.path();

        let state_dir = ea::ea_state_dir(1, omar_dir);
        std::fs::create_dir_all(&state_dir).unwrap();

        let _ = build_ea_command(
            "claude",
            1,
            "Research",
            omar_dir,
            &McpLaunchContext {
                omar_dir: omar_dir.to_path_buf(),
                ea_id: 1,
                session_prefix: "omar-agent-".to_string(),
                default_command: "claude".to_string(),
                default_workdir: ".".to_string(),
                health_idle_warning: 15,
                tmux_server: None,
                topology: None,
                serve: None,
            },
        );

        // Combined prompt should be in EA-scoped directory, not global
        let combined = state_dir.join("ea_prompt_combined.md");
        assert!(combined.exists());
        let content = std::fs::read_to_string(&combined).unwrap();
        assert!(content.contains("Executive Assistant"));
    }

    #[test]
    fn test_build_ea_command_includes_memory() {
        let dir = tempfile::tempdir().unwrap();
        let omar_dir = dir.path();

        let state_dir = ea::ea_state_dir(0, omar_dir);
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("memory.md"), "# Saved state\nSome memory").unwrap();

        let _ = build_ea_command(
            "claude",
            0,
            "Default",
            omar_dir,
            &test_mcp_context(omar_dir),
        );

        let combined = state_dir.join("ea_prompt_combined.md");
        let content = std::fs::read_to_string(&combined).unwrap();
        assert!(content.contains("Some memory"));
        assert!(content.contains("Current OMAR State"));
    }

    #[test]
    fn test_prompts_dir_creates_files() {
        let dir = tempfile::tempdir().unwrap();
        let pdir = prompts_dir(dir.path());
        assert!(pdir.join("executive-assistant.md").exists());
        assert!(pdir.join("agent.md").exists());
    }

    #[test]
    fn embedded_prompts_accept_namespaced_and_plain_omar_tool_names() {
        for prompt in [PROMPT_EA, PROMPT_AGENT] {
            assert!(prompt.contains("mcp__omar__<tool>"));
            assert!(prompt.contains("simply `<tool>`"));
            assert!(prompt.contains("`spawn_agent`"));
            assert!(prompt.contains("`schedule_omar_event`"));
        }
    }
}
