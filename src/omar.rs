mod app;
mod backend_probe;
mod channel;
mod computer;
mod config;
mod diagram;
mod ea;
mod event;
mod manager;
mod mcp;
mod memory;
mod metrics;
mod panic_hook;
mod paths;
mod process;
mod projects;
mod scheduler;
mod serve;
mod stub_agent;
mod terminal;
mod tmux;
mod topology;
mod ui;
mod web_assets;

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use crossterm::{
    event::{
        KeyCode, KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::Mutex;

use app::App;
use config::Config;
use event::{AppEvent, EventHandler};
use tmux::{tmux_command, TmuxClient};

#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Tmux session name used when omar auto-launches into tmux
pub const DASHBOARD_SESSION: &str = "omar-dashboard";

const TMUX_SETUP_WARNING: &str = "⚠ tmux not configured for omar — run 'omar setup-tmux' to fix";

#[derive(Parser)]
#[command(name = "omar", about = "Agent dashboard for tmux", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Path to config file
    #[arg(short, long)]
    config: Option<String>,

    /// Agent backend to use: claude, codex, cursor, opencode, agy
    #[arg(short, long)]
    agent: Option<String>,

    /// EA to target by id or name [default: active EA]
    #[arg(long, global = true)]
    ea: Option<String>,

    /// Enable global spawn metrics logging sink
    #[arg(long, global = true)]
    spawn_metrics: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Spawn a new agent session
    Spawn {
        /// Name for the agent session
        #[arg(short, long)]
        name: String,

        /// Command to run in the session (defaults to configured default_command)
        #[arg(short, long)]
        command: Option<String>,

        /// Working directory
        #[arg(short, long)]
        workdir: Option<String>,
    },

    /// List agent sessions in the target EA
    List {
        /// List sessions across all EAs
        #[arg(long)]
        all_eas: bool,
    },

    /// Kill an agent session
    Kill {
        /// Name of the session to kill
        name: String,
    },

    /// Hand queued OMAR events to a backend hook (invoked by the backend).
    ///
    /// Reads the spool named by $OMAR_EVENT_SPOOL, empties it, and prints the
    /// reply shape the backend expects on stdout.
    HookDrain {
        /// Which backend's hook contract to answer: cursor or agy.
        #[arg(long)]
        format: String,
    },

    /// Configure tmux for optimal omar experience
    SetupTmux,

    /// Start or interact with the manager agent
    Manager {
        /// Manager action (start, orchestrate)
        #[command(subcommand)]
        action: Option<ManagerAction>,
    },

    /// Manage scheduled events for the target EA
    Event {
        #[command(subcommand)]
        action: EventAction,
    },

    /// Start the OMAR MCP server over stdio
    McpServer {
        /// Path to a serialized MCP server context JSON file. When omitted,
        /// the server builds a default context from the current config and
        /// active EA — used by peer processes (e.g. the Slack bridge) that
        /// aren't spawned by a specific backend launch.
        #[arg(long)]
        context_file: Option<String>,
    },

    /// Run an OMAR program to completion
    Run {
        /// OMAR source program
        program: PathBuf,

        /// External input in NAME=VALUE form; repeat for multiple inputs.
        /// A timer-driven program takes none, and the runtime still rejects a
        /// missing one by name, so requiring it here only blocked those.
        #[arg(long = "input")]
        inputs: Vec<String>,

        /// Replace existing agent sessions with topology-scoped sessions
        #[arg(long)]
        replace: bool,

        /// Maximum time to wait for each prompt invocation
        #[arg(long, default_value_t = 300)]
        timeout_seconds: u64,

        /// Run the logical clock as fast as the work allows instead of holding
        /// it against the wall clock. A delay stays an ordering; it stops being
        /// a wait.
        #[arg(long)]
        fast: bool,

        /// Expose the live topology diagram API while the run is active
        #[arg(long)]
        diagram_server: bool,

        /// Address for the live topology diagram API. Requires
        /// `--diagram-server`; setting it alone silently did nothing.
        #[arg(long, default_value = "127.0.0.1:0", requires = "diagram_server")]
        diagram_address: std::net::SocketAddr,
    },

    /// Answer topology invocations without a model (test backend `stub`)
    #[command(hide = true)]
    StubAgent {
        /// MCP context written for this agent by the runtime
        #[arg(long)]
        context_file: PathBuf,
    },

    /// Accept OMAR programs over HTTP and supervise their runs
    Serve {
        /// Loopback address to bind the admission API
        #[arg(long, default_value = "127.0.0.1:7340")]
        address: std::net::SocketAddr,

        /// Restart the executive assistant so it can reply and propose designs.
        /// Its MCP context is fixed at launch, so an already running EA cannot
        /// gain those tools without this. Discards its current session.
        #[arg(long)]
        restart_ea: bool,

        /// Serve the API without starting an executive assistant. The agent
        /// context is still written, so a test harness can stand in for one.
        #[arg(long)]
        no_ea: bool,

        /// Open Mission Control in a browser. It is served from this same
        /// address, so the page and the API share an origin.
        #[arg(long)]
        ui: bool,
    },
}

#[derive(Subcommand)]
enum ManagerAction {
    /// Start the manager session
    Start,
    /// Run in orchestration mode (interactive)
    Orchestrate,
}

#[derive(Subcommand)]
enum EventAction {
    /// Schedule an event for an agent or the EA
    Schedule {
        /// Receiver name ("ea" for the manager)
        #[arg(long)]
        receiver: String,

        /// Event payload text
        #[arg(long)]
        payload: String,

        /// Sender label shown in the delivered message
        #[arg(long)]
        sender: Option<String>,

        /// Absolute trigger timestamp in nanoseconds since epoch
        #[arg(long)]
        at_ns: Option<u64>,

        /// Relative delay in seconds from now
        #[arg(long)]
        in_seconds: Option<u64>,

        /// Relative delay in nanoseconds from now
        #[arg(long)]
        in_ns: Option<u64>,

        /// Recurrence interval in seconds
        #[arg(long)]
        every_seconds: Option<u64>,

        /// Recurrence interval in nanoseconds
        #[arg(long)]
        every_ns: Option<u64>,
    },

    /// List scheduled events for the target EA
    List,

    /// Cancel a scheduled event by id
    Cancel {
        /// Event id
        id: String,
    },
}

/// Waits for `omar serve` to be listening, then opens Mission Control.
///
/// Opening before the listener exists shows the operator a connection error and
/// makes them reload; waiting a bounded time means a daemon that fails to start
/// does not leave a thread polling for the life of the process.
fn open_when_listening(address: std::net::SocketAddr) {
    for _ in 0..100 {
        if std::net::TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_ok() {
            open_browser(&format!("http://{address}"));
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!("Mission Control is at http://{address}");
}

/// Asks the desktop to open a URL, and says it plainly if there is no desktop.
fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let opened = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !opened {
        println!("Open {url}");
    }
}

fn main() -> Result<()> {
    // Install the persisted-panic hook FIRST, before tokio builds its
    // runtime (and spawns worker threads). If the tmux parent dies it
    // takes the stderr pane with it (see issue #118), so panics need
    // to be written to disk before unwind. The hook chains to whatever
    // hook was previously installed, preserving RUST_BACKTRACE /
    // default stderr behaviour for non-tmux runs.
    //
    // We deliberately don't use `#[tokio::main]`: that macro builds
    // the runtime BEFORE the `async fn` body runs, so any panic on a
    // worker spawned during runtime construction would miss the hook.
    panic_hook::install(omar_dir().join("logs").join("panics"));

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async_main())
}

async fn async_main() -> Result<()> {
    let cli = Cli::parse();
    let mut config = Config::load(cli.config.as_deref())?;
    if let Some(ref agent) = cli.agent {
        config.agent.default_command =
            config::resolve_backend(agent).map_err(|e| anyhow::anyhow!("{}", e))?;
    }
    if cli.spawn_metrics {
        config.metrics.spawn_metrics_enabled = true;
        config.save_to_path(&Config::resolve_path(cli.config.as_deref()));
    }
    metrics::configure(config.metrics.spawn_metrics_enabled);
    let omar_dir = omar_dir();
    let defer_active_ea_save = cli.command.is_none() && cli.agent.is_some();

    if !defer_active_ea_save {
        if let Some(ref selector) = cli.ea {
            let (ea_info, created) = ea::resolve_or_create_ea_selector(&omar_dir, Some(selector))?;
            if created {
                eprintln!("Created EA '{}' (id={})", ea_info.name, ea_info.id);
            }
            ea::save_active_ea(&omar_dir, ea_info.id)?;
        }
    }

    match cli.command {
        Some(Commands::Spawn {
            name,
            command,
            workdir,
        }) => {
            let target = resolve_cli_ea(&omar_dir, cli.ea.as_deref())?;
            let client =
                TmuxClient::new(ea::ea_prefix(target.id, &config.dashboard.session_prefix));
            let cmd = command.unwrap_or_else(|| config.agent.default_command.clone());
            spawn_agent(&client, &name, &cmd, workdir.as_deref())
        }
        Some(Commands::List { all_eas }) => {
            if all_eas {
                list_agents_all(&omar_dir, &config.dashboard.session_prefix)
            } else {
                let target = resolve_cli_ea(&omar_dir, cli.ea.as_deref())?;
                list_agents_for_ea(&config.dashboard.session_prefix, &target)
            }
        }
        Some(Commands::Kill { name }) => {
            let target = resolve_cli_ea(&omar_dir, cli.ea.as_deref())?;
            let client =
                TmuxClient::new(ea::ea_prefix(target.id, &config.dashboard.session_prefix));
            kill_agent(
                &client,
                &name,
                &scheduler::Scheduler::with_store(scheduler::events_store_path(&omar_dir)),
                target.id,
            )
        }
        Some(Commands::SetupTmux) => setup_tmux(),
        Some(Commands::Manager { action }) => {
            let target = resolve_cli_ea(&omar_dir, cli.ea.as_deref())?;
            let client =
                TmuxClient::new(ea::ea_prefix(target.id, &config.dashboard.session_prefix));
            match action {
                Some(ManagerAction::Start) | None => manager::start_manager(
                    &client,
                    &config.agent.default_command,
                    target.id,
                    &target.name,
                    &omar_dir,
                    &config.dashboard.session_prefix,
                    &manager::ManagerRuntimeOptions {
                        default_workdir: config.agent.default_workdir.clone(),
                        health_idle_warning: config.health.idle_warning,
                        serve: None,
                    },
                ),
                Some(ManagerAction::Orchestrate) => manager::run_manager_orchestration(
                    &client,
                    &config.agent.default_command,
                    target.id,
                    &target.name,
                    &omar_dir,
                    &config.dashboard.session_prefix,
                    &manager::ManagerRuntimeOptions {
                        default_workdir: config.agent.default_workdir.clone(),
                        health_idle_warning: config.health.idle_warning,
                        serve: None,
                    },
                ),
            }
        }
        Some(Commands::Event { action }) => {
            let target = resolve_cli_ea(&omar_dir, cli.ea.as_deref())?;
            let scheduler =
                scheduler::Scheduler::with_store(scheduler::events_store_path(&omar_dir));
            match action {
                EventAction::Schedule {
                    receiver,
                    payload,
                    sender,
                    at_ns,
                    in_seconds,
                    in_ns,
                    every_seconds,
                    every_ns,
                } => schedule_cli_event(
                    &scheduler,
                    target.id,
                    receiver,
                    payload,
                    sender,
                    at_ns,
                    in_seconds,
                    in_ns,
                    every_seconds,
                    every_ns,
                ),
                EventAction::List => list_cli_events(&scheduler, target.id),
                EventAction::Cancel { id } => cancel_cli_event(&scheduler, target.id, &id),
            }
        }
        Some(Commands::McpServer { context_file }) => match context_file {
            Some(path) => mcp::run_server_from_context_file(PathBuf::from(path)),
            None => mcp::run_server_with_default_context(),
        },
        Some(Commands::Run {
            program,
            inputs,
            replace,
            timeout_seconds,
            fast,
            diagram_server,
            diagram_address,
        }) => {
            let target = resolve_cli_ea(&omar_dir, cli.ea.as_deref())?;
            let bytecode = topology::load_program(&program)?;
            topology::run_topology(
                &bytecode,
                topology::TopologyRunConfig {
                    ea_id: target.id,
                    omar_dir: &omar_dir,
                    base_prefix: &config.dashboard.session_prefix,
                    default_workdir: &config.agent.default_workdir,
                    health_idle_warning: config.health.idle_warning,
                    inputs: &inputs,
                    replace,
                    timeout: Duration::from_secs(timeout_seconds),
                    pace: if fast {
                        topology::Pace::Fast
                    } else {
                        topology::Pace::RealTime
                    },
                    diagram_address: diagram_server.then_some(diagram_address),
                    diagram_ready: None,
                    // `omar run` has no HTTP surface to hang a panel on, so a
                    // web-backed reaction in a CLI run waits out its deadline.
                    panel_ready: None,
                },
            )
        }
        Some(Commands::HookDrain { format }) => {
            // Always print valid JSON, even on misconfiguration: a hook that
            // writes nothing reads as a failure to the backend.
            // Check the format before touching the spool: draining first
            // would destroy every queued event on a typo.
            let reply = match channel::HookFormat::parse(&format) {
                Some(hook) => {
                    let events = std::env::var("OMAR_EVENT_SPOOL")
                        .ok()
                        .map(|spool| channel::drain_spool(std::path::Path::new(&spool)))
                        .unwrap_or_default();
                    hook.render(&events)
                }
                None => "{}".to_string(),
            };
            println!("{}", reply);
            Ok(())
        }
        Some(Commands::StubAgent { context_file }) => stub_agent::run(&context_file),
        Some(Commands::Serve {
            address,
            restart_ea,
            no_ea,
            ui,
        }) => {
            if ui && !web_assets::is_bundled() {
                anyhow::bail!(web_assets::MISSING);
            }
            let target = resolve_cli_ea(&omar_dir, cli.ea.as_deref())?;
            if ui {
                // `serve::run` blocks, so the browser is opened from a thread
                // that waits for the listener rather than before it exists.
                std::thread::spawn(move || open_when_listening(address));
            }
            serve::run(address, &config, &omar_dir, target.id, restart_ea, !no_ea)
        }
        None => {
            if cli.agent.is_some() {
                let (target, created) =
                    ea::resolve_or_create_ea_selector(&omar_dir, cli.ea.as_deref())?;
                if created {
                    eprintln!("Created EA '{}' (id={})", target.name, target.id);
                }
                let client =
                    TmuxClient::new(ea::ea_prefix(target.id, &config.dashboard.session_prefix));
                let (_, result) = manager::ensure_manager_session(
                    &client,
                    &config.agent.default_command,
                    target.id,
                    &target.name,
                    &omar_dir,
                    &config.dashboard.session_prefix,
                    &manager::ManagerRuntimeOptions {
                        default_workdir: config.agent.default_workdir.clone(),
                        health_idle_warning: config.health.idle_warning,
                        serve: None,
                    },
                )?;
                match result {
                    manager::ManagerEnsureResult::Started => {
                        eprintln!("Started EA '{}' with requested backend", target.name);
                    }
                    manager::ManagerEnsureResult::ReplacedBackend => {
                        eprintln!("Replaced EA '{}' with requested backend", target.name);
                    }
                    manager::ManagerEnsureResult::AlreadyRunning => {}
                }
                ea::save_active_ea(&omar_dir, target.id)?;
            }
            if std::env::var("TMUX").is_err() {
                let target = resolve_cli_ea(&omar_dir, cli.ea.as_deref())?;
                relaunch_in_tmux(&config, &omar_dir, target.id, cli.agent.is_some())
            } else {
                run_dashboard(config).await
            }
        }
    }
}

fn omar_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".omar")
}

fn resolve_cli_ea(omar_dir: &std::path::Path, selector: Option<&str>) -> Result<ea::EaInfo> {
    Ok(ea::resolve_or_create_ea_selector(omar_dir, selector)?.0)
}

fn list_agents_for_ea(base_prefix: &str, ea_info: &ea::EaInfo) -> Result<()> {
    let prefix = ea::ea_prefix(ea_info.id, base_prefix);
    let manager_session = ea::ea_manager_session(ea_info.id, base_prefix);
    let sessions = sessions_for_ea(base_prefix, ea_info.id)?;

    if sessions.is_empty() {
        println!(
            "No agent sessions found for EA {} ({})",
            ea_info.id, ea_info.name
        );
        return Ok(());
    }

    println!("EA {}: {}", ea_info.id, ea_info.name);
    println!("{:<20} {:<12} {:<10}", "NAME", "ATTACHED", "PID");
    println!("{}", "-".repeat(44));

    for session in sessions {
        let name = display_cli_session_name(&session.name, &prefix, &manager_session);
        let attached = if session.attached { "yes" } else { "no" };
        println!("{:<20} {:<12} {:<10}", name, attached, session.pane_pid);
    }

    Ok(())
}

fn list_agents_all(omar_dir: &std::path::Path, base_prefix: &str) -> Result<()> {
    let eas = ea::ensure_default_ea(omar_dir)?;
    let mut printed = false;

    println!(
        "{:<6} {:<20} {:<20} {:<12} {:<10}",
        "EA", "EA_NAME", "NAME", "ATTACHED", "PID"
    );
    println!("{}", "-".repeat(74));

    for ea_info in eas {
        let prefix = ea::ea_prefix(ea_info.id, base_prefix);
        let manager_session = ea::ea_manager_session(ea_info.id, base_prefix);
        for session in sessions_for_ea(base_prefix, ea_info.id)? {
            let name = display_cli_session_name(&session.name, &prefix, &manager_session);
            let attached = if session.attached { "yes" } else { "no" };
            println!(
                "{:<6} {:<20} {:<20} {:<12} {:<10}",
                ea_info.id, ea_info.name, name, attached, session.pane_pid
            );
            printed = true;
        }
    }

    if !printed {
        println!("No agent sessions found");
    }

    Ok(())
}

fn sessions_for_ea(base_prefix: &str, ea_id: ea::EaId) -> Result<Vec<tmux::Session>> {
    let prefix = ea::ea_prefix(ea_id, base_prefix);
    let manager_session = ea::ea_manager_session(ea_id, base_prefix);
    let client = TmuxClient::new("");
    let mut sessions = client.list_all_sessions()?;
    sessions.retain(|session| session.name == manager_session || session.name.starts_with(&prefix));
    sessions.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(sessions)
}

fn display_cli_session_name(session_name: &str, prefix: &str, manager_session: &str) -> String {
    if session_name == manager_session {
        "ea".to_string()
    } else {
        session_name
            .strip_prefix(prefix)
            .unwrap_or(session_name)
            .to_string()
    }
}

fn spawn_agent(
    client: &TmuxClient,
    name: &str,
    command: &str,
    workdir: Option<&str>,
) -> Result<()> {
    let full_name = client.session_for(name);

    if client.has_session(&full_name)? {
        anyhow::bail!("Session '{}' already exists", name);
    }

    client.new_session(&full_name, command, workdir)?;
    println!("Spawned agent: {}", name);
    Ok(())
}

fn kill_agent(
    client: &TmuxClient,
    name: &str,
    scheduler: &scheduler::Scheduler,
    ea_id: ea::EaId,
) -> Result<()> {
    let full_name = client.session_for(name);

    if !client.has_session(&full_name)? {
        anyhow::bail!("Session '{}' not found", name);
    }
    let _ = client.ensure_session_not_attached(&full_name)?;

    client.kill_session(&full_name)?;
    let _ = scheduler.cancel_by_receiver_and_ea(name, ea_id);
    println!("Killed agent: {}", name);
    Ok(())
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos() as u64
}

#[allow(clippy::too_many_arguments)]
fn schedule_cli_event(
    scheduler: &scheduler::Scheduler,
    ea_id: ea::EaId,
    receiver: String,
    payload: String,
    sender: Option<String>,
    at_ns: Option<u64>,
    in_seconds: Option<u64>,
    in_ns: Option<u64>,
    every_seconds: Option<u64>,
    every_ns: Option<u64>,
) -> Result<()> {
    let base = now_ns();
    let delay_ns = scheduler::combine_seconds_and_ns(in_seconds, in_ns).unwrap_or(0);
    let timestamp = at_ns.unwrap_or_else(|| base.saturating_add(delay_ns));
    let recurring_ns = scheduler::combine_seconds_and_ns(every_seconds, every_ns);

    let event = scheduler::ScheduledEvent {
        id: uuid::Uuid::new_v4().to_string(),
        sender: sender.unwrap_or_else(|| "ea".to_string()),
        receiver,
        timestamp,
        payload,
        created_at: base,
        recurring_ns,
        ea_id,
    };
    scheduler.insert(event.clone());
    println!(
        "Scheduled event: {} -> {} at {}",
        event.sender, event.receiver, event.timestamp
    );
    println!("Event id: {}", event.id);
    Ok(())
}

fn list_cli_events(scheduler: &scheduler::Scheduler, ea_id: ea::EaId) -> Result<()> {
    let mut events = scheduler.list_by_ea(ea_id);
    if events.is_empty() {
        println!("No scheduled events found for EA {}", ea_id);
        return Ok(());
    }
    events.sort_by_key(|event| (event.timestamp, event.created_at));
    println!(
        "{:<36} {:<14} {:<14} {:<18} {:<12} PAYLOAD",
        "ID", "SENDER", "RECEIVER", "TIMESTAMP_NS", "RECURRING"
    );
    println!("{}", "-".repeat(120));
    for event in events {
        let recurring = event
            .recurring_ns
            .map(|ns| ns.to_string())
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{:<36} {:<14} {:<14} {:<18} {:<12} {}",
            event.id, event.sender, event.receiver, event.timestamp, recurring, event.payload
        );
    }
    Ok(())
}

fn cancel_cli_event(
    scheduler: &scheduler::Scheduler,
    ea_id: ea::EaId,
    event_id: &str,
) -> Result<()> {
    match scheduler.cancel_if_ea(event_id, ea_id) {
        Ok(event) => {
            println!("Cancelled event: {}", event.id);
            Ok(())
        }
        Err(true) => anyhow::bail!("Event '{}' belongs to a different EA", event_id),
        Err(false) => anyhow::bail!("Event '{}' not found", event_id),
    }
}

/// Re-launch omar inside a tmux session.
/// Called when the dashboard is started outside of tmux so that popups,
/// attach, and other tmux-dependent features work correctly.
///
/// If the dashboard is already running, hands this invocation's EA/backend/cwd
/// to it and attaches. This preserves the in-memory scheduler (cron jobs,
/// pending events) across detach/reattach cycles. If attach fails (stale
/// session), kills the stale session and creates a fresh one.
fn relaunch_in_tmux(
    config: &Config,
    omar_dir: &std::path::Path,
    active_ea: ea::EaId,
    restart_manager: bool,
) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let client = TmuxClient::new("");
    let exe = std::env::current_exe()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    if client.has_session(DASHBOARD_SESSION)? {
        let handoff = ea::DashboardLaunchHandoff {
            active_ea,
            default_command: config.agent.default_command.clone(),
            default_workdir: current_dir.to_string_lossy().into_owned(),
            restart_manager,
        };
        ea::save_dashboard_launch_handoff(omar_dir, &handoff)?;
        let target = format!("={}", DASHBOARD_SESSION);
        let status = tmux_command()
            .args(["-2", "attach-session", "-t", &target])
            .status();

        match status {
            Ok(s) if s.success() => return Ok(()),
            _ => {
                let _ = client.kill_session(DASHBOARD_SESSION);
            }
        }
    }

    let mut cmd = tmux_command();
    // Force 256-color mode when launching the dashboard session.
    cmd.arg("-2");
    cmd.args(["new-session", "-s", DASHBOARD_SESSION, "-c"]);
    cmd.arg(&current_dir);
    cmd.arg(&exe);
    cmd.args(&args);

    // exec() replaces the current process; only returns on error
    let err = cmd.exec();
    anyhow::bail!("Failed to launch tmux: {}", err)
}

/// Recommended tmux settings for omar, keyed by option name.
const TMUX_RECOMMENDED: &[(&str, &str, &str)] = &[
    (
        "default-terminal",
        "set -g default-terminal tmux-256color",
        "256-color terminal support",
    ),
    ("mouse", "set -g mouse on", "mouse scrolling and selection"),
    (
        "history-limit",
        "set -g history-limit 9999",
        "larger scrollback history",
    ),
    (
        "extended-keys",
        "set -g extended-keys on",
        "Shift+Enter in agents (on, not always — always breaks Shift+Tab in omar)",
    ),
    (
        "set-clipboard",
        "set -g set-clipboard on",
        "clipboard integration",
    ),
];

#[cfg(target_os = "macos")]
const TMUX_PLATFORM_RECOMMENDED: &[(&str, &str, &str)] = &[(
    "copy-command",
    "set -g copy-command pbcopy",
    "copy tmux selections to the macOS clipboard",
)];

#[cfg(not(target_os = "macos"))]
const TMUX_PLATFORM_RECOMMENDED: &[(&str, &str, &str)] = &[];

/// Additional raw lines that need to appear in tmux.conf (checked by substring).
const TMUX_EXTRA_LINES: &[(&str, &str, &str)] = &[
    (
        "terminal-features',*:RGB'",
        "set -as terminal-features ',*:RGB'",
        "truecolor support",
    ),
    (
        "terminal-features',*:extkeys'",
        "set -as terminal-features ',*:extkeys'",
        "extended key passthrough",
    ),
    (
        "terminal-features',*:clipboard'",
        "set -as terminal-features ',*:clipboard'",
        "clipboard passthrough",
    ),
    (
        "bind-key-nC-\\\\",
        "bind-key -n C-\\\\ detach-client",
        "Ctrl+\\\\ to detach from popup",
    ),
];

/// Check if any recommended tmux settings are missing.
fn tmux_setup_needed() -> bool {
    for &(opt, cmd, _) in TMUX_RECOMMENDED
        .iter()
        .chain(TMUX_PLATFORM_RECOMMENDED.iter())
    {
        // Extract expected value from the command string (last word)
        let expected = cmd.split_whitespace().last().unwrap_or("on");
        if let Ok(out) = tmux_command().args(["show-options", "-gv", opt]).output() {
            let val = String::from_utf8_lossy(&out.stdout);
            if val.trim() != expected {
                return true;
            }
        }
    }
    false
}

fn sync_tmux_setup_warning(app: &mut app::App) {
    if tmux_setup_needed() {
        app.set_persistent_warning_if_clear_or_same(TMUX_SETUP_WARNING);
    } else {
        app.clear_persistent_warning_if(TMUX_SETUP_WARNING);
    }
}

/// Interactive tmux configuration setup.
fn setup_tmux() -> Result<()> {
    use std::io::Write;

    let conf_path = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".tmux.conf");

    let existing = std::fs::read_to_string(&conf_path).unwrap_or_default();

    // Collect missing settings
    let mut to_add: Vec<(&str, &str)> = Vec::new();

    for &(opt, line, desc) in TMUX_RECOMMENDED
        .iter()
        .chain(TMUX_PLATFORM_RECOMMENDED.iter())
    {
        // Check runtime value — even if the line is in the config,
        // a later conflicting line may override it.
        let expected = line.split_whitespace().last().unwrap_or("on");
        let runtime_ok = tmux_command()
            .args(["show-options", "-gv", opt])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == expected)
            .unwrap_or(false);
        if !runtime_ok {
            to_add.push((line, desc));
        }
    }

    let normalized = existing.replace(' ', "");
    for &(needle, line, desc) in TMUX_EXTRA_LINES {
        if !normalized.contains(needle) {
            to_add.push((line, desc));
        }
    }

    if to_add.is_empty() {
        println!("✓ tmux is already configured for omar.");
        return Ok(());
    }

    println!(
        "The following settings will be added to {}:\n",
        conf_path.display()
    );
    for (line, desc) in &to_add {
        println!("  {}  # {}", line, desc);
    }

    print!("\nApply? [Y/n] ");
    std::io::stdout().flush()?;

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let input = input.trim().to_lowercase();

    if !input.is_empty() && input != "y" && input != "yes" {
        println!("Aborted.");
        return Ok(());
    }

    // Append to tmux.conf
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&conf_path)?;

    writeln!(file, "\n# omar recommended settings")?;
    for (line, _) in &to_add {
        writeln!(file, "{}", line)?;
    }

    // Apply settings directly to the running tmux server.
    // source-file alone isn't reliable because earlier conflicting lines
    // in the config (e.g., oh-my-tmux sets mouse off) can override ours.
    let tmux_running = tmux_command()
        .args(["list-sessions"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if tmux_running {
        for (line, _) in &to_add {
            // Each line is a full tmux command (e.g. "set -g mouse on")
            let args: Vec<&str> = line.split_whitespace().collect();
            let _ = tmux_command().args(&args).status();
        }
        println!("✓ Applied to ~/.tmux.conf and running tmux server.");
    } else {
        println!("✓ Applied to ~/.tmux.conf (tmux not running, will take effect next session).");
    }

    Ok(())
}

/// Locate the `omar-slack` binary. Checks next to the current executable
/// first, then falls back to a PATH lookup.
fn find_slack_binary() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("omar-slack");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    // Fall back to PATH lookup
    Some(PathBuf::from("omar-slack"))
}

/// Spawn the Slack bridge binary if SLACK_BOT_TOKEN and SLACK_APP_TOKEN are set.
fn spawn_slack_bridge() -> Option<std::process::Child> {
    if std::env::var("SLACK_BOT_TOKEN").is_err() || std::env::var("SLACK_APP_TOKEN").is_err() {
        return None;
    }

    let binary = find_slack_binary()?;
    match std::process::Command::new(&binary)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => {
            eprintln!("[omar] Slack bridge started (pid {})", child.id());
            Some(child)
        }
        Err(e) => {
            eprintln!("[omar] Failed to start Slack bridge: {}", e);
            None
        }
    }
}

/// Locate the `omar-computer` binary. Checks next to the current executable
/// first, then falls back to a PATH lookup.
fn find_computer_binary() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("omar-computer");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    // Fall back to PATH lookup
    Some(PathBuf::from("omar-computer"))
}

/// Spawn the computer-use bridge binary on Linux when an X11 display is
/// available. The bridge wraps xdotool / ImageMagick `import`, which are
/// X11-only tools, so skip it entirely on non-Linux platforms — an
/// XQuartz `DISPLAY` on macOS would otherwise trigger a noisy spawn of a
/// bridge that cannot do anything useful there.
fn spawn_computer_bridge() -> Option<std::process::Child> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    // Treat both unset and empty DISPLAY as "no X11 session". A bare
    // `export DISPLAY=` in a shell profile would otherwise satisfy
    // `is_err() == false` and still trigger a useless bridge spawn.
    let display_ok = std::env::var("DISPLAY")
        .ok()
        .is_some_and(|v| !v.trim().is_empty());
    if !display_ok {
        return None;
    }

    let binary = find_computer_binary()?;
    match std::process::Command::new(&binary)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => {
            eprintln!("[omar] Computer bridge started (pid {})", child.id());
            Some(child)
        }
        Err(e) => {
            eprintln!("[omar] Failed to start computer bridge: {}", e);
            None
        }
    }
}

/// Kill a child process gracefully: SIGTERM first, then SIGKILL after timeout.
fn kill_child_gracefully(child: &mut std::process::Child, timeout: Duration) {
    // Send SIGTERM
    let _ = std::process::Command::new("kill")
        .arg(child.id().to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    // Wait for the process to exit
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            _ => {
                if start.elapsed() >= timeout {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    // Force kill if still running
    let _ = child.kill();
    let _ = child.wait();
}

async fn run_dashboard(config: Config) -> Result<()> {
    // Some shells/dev tools export NO_COLOR globally. That disables all ANSI
    // styling and makes the TUI monochrome. The dashboard is explicitly color-coded.
    if std::env::var_os("NO_COLOR").is_some() {
        std::env::remove_var("NO_COLOR");
    }

    // Create the ticker buffer and scheduler, then spawn the event loop
    let ticker = scheduler::TickerBuffer::new();
    let omar_dir = omar_dir();
    let scheduler = Arc::new(scheduler::Scheduler::with_store(
        scheduler::events_store_path(&omar_dir),
    ));
    let popup_receiver = scheduler::new_popup_receiver();
    let base_prefix = config.dashboard.session_prefix.clone();
    tokio::spawn(scheduler::run_event_loop(
        scheduler.clone(),
        ticker.clone(),
        popup_receiver.clone(),
        base_prefix,
    ));

    // Create SINGLE shared App instance for the dashboard/runtime state.
    let shared_app = Arc::new(Mutex::new(App::new(
        &config,
        ticker.clone(),
        scheduler.clone(),
    )));

    // Spawn Slack bridge if configured
    let mut slack_bridge = spawn_slack_bridge();

    // Spawn computer-use bridge if X11 is available
    let mut computer_bridge = spawn_computer_bridge();

    // Initialize terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    // Enable keyboard enhancement where supported (improves key reporting).
    let keyboard_enhanced = supports_keyboard_enhancement().unwrap_or(false);
    if keyboard_enhanced {
        let _ = execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Show bridge status
    {
        let mut app = shared_app.lock().await;
        match (slack_bridge.is_some(), computer_bridge.is_some()) {
            (true, true) => app.set_status("Slack & computer bridges started"),
            (true, false) => app.set_status("Slack bridge started"),
            (false, true) => app.set_status("Computer bridge started"),
            _ => {}
        }
    }

    // Warn if tmux config is missing recommended settings
    {
        let mut app = shared_app.lock().await;
        sync_tmux_setup_warning(&mut app);
    }

    // Initial refresh
    {
        let mut app = shared_app.lock().await;
        if let Err(e) = app.refresh() {
            app.set_status(format!("Error: {}", e));
        }
    }

    // Event loop — locks shared_app per-phase (render, then handle).
    // The lock is NOT held across events.next().await so API calls proceed.
    let tick_rate = Duration::from_secs(config.dashboard.refresh_interval);
    let mut events = EventHandler::new(tick_rate);
    let mut tick_count: u64 = 0;

    loop {
        // Phase 1: Render (brief lock — read-only access to App)
        {
            let app = shared_app.lock().await;
            terminal.draw(|f| ui::render(f, &app))?;
        }
        // Lock released — API calls can proceed during event wait

        // Phase 2: Wait for event (no lock held)
        let event = events.next().await;

        // Phase 3: Handle event (lock for mutation)
        if let Some(event) = event {
            match event {
                AppEvent::Key(key) => {
                    let mut app = shared_app.lock().await;

                    // Handle project input mode
                    if app.project_input_mode {
                        match key.code {
                            KeyCode::Esc => {
                                app.project_input_mode = false;
                                app.project_input.clear();
                            }
                            KeyCode::Enter => {
                                let name = app.project_input.clone();
                                if !name.trim().is_empty() {
                                    app.add_project(name.trim());
                                    app.set_status("Project added");
                                }
                                app.project_input_mode = false;
                                app.project_input.clear();
                            }
                            KeyCode::Backspace => {
                                app.project_input.pop();
                            }
                            KeyCode::Char(c) => {
                                app.project_input.push(c);
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // Handle EA name input mode (for spawning a new EA)
                    if app.ea_input_mode {
                        match key.code {
                            KeyCode::Esc => {
                                app.ea_input_mode = false;
                                app.ea_input.clear();
                            }
                            KeyCode::Enter => {
                                let name = app.ea_input.clone();
                                if !name.trim().is_empty() {
                                    if let Err(e) = app.create_ea(name.trim().to_string(), None) {
                                        app.set_status(format!("Error: {}", e));
                                    }
                                }
                                app.ea_input_mode = false;
                                app.ea_input.clear();
                            }
                            KeyCode::Backspace => {
                                app.ea_input.pop();
                            }
                            KeyCode::Char(c) => {
                                app.ea_input.push(c);
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // Handle confirmation dialog (kill, quit, or delete EA)
                    if let Some(action) = app.pending_confirm {
                        match key.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') => match action {
                                app::ConfirmAction::Kill => {
                                    let short_name = app.selected_agent_short_name();
                                    if let Err(e) = app.kill_selected() {
                                        app.set_status(format!("Error: {}", e));
                                    } else if let Some(name) = short_name {
                                        scheduler.cancel_by_receiver_and_ea(&name, app.active_ea);
                                    }
                                }
                                app::ConfirmAction::ResetQuit => {
                                    app.reset_on_quit = true;
                                    app.should_quit = true;
                                }
                                app::ConfirmAction::DeleteEa => {
                                    let ea_id = app.active_ea;
                                    if let Err(e) = app.delete_ea(ea_id) {
                                        app.set_status(format!("Error: {}", e));
                                    }
                                }
                            },
                            _ => {
                                app.pending_confirm = None;
                            }
                        }
                        continue;
                    }

                    // Handle help popup
                    if app.show_help {
                        app.show_help = false;
                        continue;
                    }

                    // Handle events popup
                    if app.show_events {
                        match key.code {
                            KeyCode::Esc | KeyCode::Char('e') | KeyCode::Enter => {
                                app.show_events = false;
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // Handle debug console popup
                    if app.show_debug_console {
                        match key.code {
                            KeyCode::Esc | KeyCode::Char('G') => {
                                app.show_debug_console = false;
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // Handle settings popup
                    if app.show_settings {
                        // Text-edit mode: capture characters and structural keys.
                        // The buffer is only committed to config on Enter, so
                        // Esc safely discards in-flight edits.
                        if let Some(buf) = app.settings_edit_buffer.as_mut() {
                            match key.code {
                                KeyCode::Esc => {
                                    app.settings_edit_buffer = None;
                                }
                                KeyCode::Enter => {
                                    let value = buf.clone();
                                    let idx = app.settings_selected;
                                    app.settings_edit_buffer = None;
                                    app.config.set_text_setting(idx, &value);
                                }
                                KeyCode::Backspace => {
                                    buf.pop();
                                }
                                // Block control chars; allow normal text.
                                KeyCode::Char(c) if !c.is_control() => {
                                    buf.push(c);
                                }
                                _ => {}
                            }
                            continue;
                        }
                        match key.code {
                            KeyCode::Esc | KeyCode::Char('S') => {
                                app.show_settings = false;
                            }
                            KeyCode::Up | KeyCode::Char('k') if app.settings_selected > 0 => {
                                app.settings_selected -= 1;
                            }
                            KeyCode::Down | KeyCode::Char('j')
                                if app.settings_selected + 1 < app.config.settings_count() =>
                            {
                                app.settings_selected += 1;
                            }
                            KeyCode::Enter => {
                                let idx = app.settings_selected;
                                let is_text = app
                                    .config
                                    .settings_item(idx)
                                    .map(|item| item.is_text())
                                    .unwrap_or(false);
                                if is_text {
                                    let current = match app.config.settings_item(idx) {
                                        Some(config::SettingItem::Text { value, .. }) => {
                                            value.to_string()
                                        }
                                        _ => String::new(),
                                    };
                                    app.settings_edit_buffer = Some(current);
                                } else {
                                    app.config.toggle_setting(idx);
                                    // If event queue was just hidden, move sidebar off Events panel
                                    if !app.config.dashboard.show_event_queue
                                        && app.sidebar_panel == app::SidebarPanel::Events
                                    {
                                        app.sidebar_panel = app::SidebarPanel::Projects;
                                    }
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // Handle sidebar enlarged popup
                    if app.sidebar_popup.is_some() {
                        match key.code {
                            KeyCode::Esc | KeyCode::Enter => {
                                app.sidebar_popup = None;
                            }
                            _ => {}
                        }
                        continue;
                    }

                    // Normal key handling
                    match key.code {
                        KeyCode::Char('Q') => {
                            app.pending_confirm = Some(app::ConfirmAction::ResetQuit);
                        }
                        KeyCode::Esc => {
                            app.drill_up();
                        }
                        KeyCode::Tab => {
                            if key.modifiers.contains(KeyModifiers::SHIFT) {
                                app.drill_up();
                            } else {
                                app.drill_down();
                            }
                        }
                        KeyCode::BackTab => {
                            // Shift+Tab sends BackTab in most terminals
                            app.drill_up();
                        }
                        KeyCode::Right => {
                            if app.config.dashboard.sidebar_right {
                                // Sidebar is on the right: try grid right first, then sidebar
                                if !app.grid_right() {
                                    app.sidebar_focused = true;
                                }
                            } else {
                                // Sidebar is on the left: try grid right (no fallback)
                                // If at right edge of grid, stay put (sidebar is the other direction)
                                if !app.grid_right() && app.sidebar_focused {
                                    app.sidebar_focused = false;
                                }
                            }
                        }
                        KeyCode::Left => {
                            if app.config.dashboard.sidebar_right {
                                // Sidebar is on the right: try grid left (no fallback)
                                if !app.grid_left() && app.sidebar_focused {
                                    app.sidebar_focused = false;
                                }
                            } else {
                                // Sidebar is on the left: try grid left first, then sidebar
                                if !app.grid_left() {
                                    app.sidebar_focused = true;
                                }
                            }
                        }
                        KeyCode::Char(']') => {
                            app.cycle_next_ea();
                        }
                        KeyCode::Char('[') => {
                            app.cycle_previous_ea();
                        }
                        KeyCode::Char('j') | KeyCode::Down => {
                            if app.sidebar_focused {
                                app.sidebar_next();
                            } else {
                                app.next();
                            }
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            if app.sidebar_focused {
                                app.sidebar_previous();
                            } else {
                                app.previous();
                            }
                        }
                        KeyCode::Char('h') => {
                            // h = physical left
                            if app.config.dashboard.sidebar_right {
                                // Sidebar on right: left means try grid left, no sidebar fallback
                                if !app.grid_left() && app.sidebar_focused {
                                    app.sidebar_focused = false;
                                }
                            } else {
                                // Sidebar on left: left means try grid left, then sidebar
                                if !app.grid_left() {
                                    app.sidebar_focused = true;
                                }
                            }
                        }
                        KeyCode::Char('l') => {
                            // l = physical right
                            if app.config.dashboard.sidebar_right {
                                // Sidebar on right: right means try grid right, then sidebar
                                if !app.grid_right() {
                                    app.sidebar_focused = true;
                                }
                            } else {
                                // Sidebar on left: right means try grid right, no sidebar fallback
                                if !app.grid_right() && app.sidebar_focused {
                                    app.sidebar_focused = false;
                                }
                            }
                        }
                        KeyCode::Enter => {
                            if app.sidebar_focused {
                                if app.sidebar_panel == app::SidebarPanel::Events {
                                    app.scheduled_events = scheduler.list_by_ea(app.active_ea);
                                    app.scheduled_events.sort_by_key(|e| e.timestamp);
                                    app.show_events = true;
                                } else {
                                    app.sidebar_popup = Some(app.sidebar_panel);
                                }
                                continue;
                            }

                            if let Err(e) = app.refresh() {
                                app.set_status(format!("Error: {}", e));
                                continue;
                            }

                            let selected_popup_receiver = app
                                .selected_popup_receiver_name()
                                .map(|name| (name, app.active_ea));
                            let popup_info = app
                                .selected_agent()
                                .map(|a| (a.session.name.clone(), app.client().clone()));

                            // Tell the scheduler which agent popup is open so it
                            // defers events for that receiver until the popup closes.
                            // Include ea_id so suppression is scoped per-EA.
                            *popup_receiver.lock().unwrap() = selected_popup_receiver;

                            // Release App lock before blocking popup call
                            drop(app);

                            if std::env::var("TMUX").is_ok() {
                                // Inside tmux: use display-popup overlay.
                                // Holding the lock across attach_popup blocks all API handlers
                                // that need app.lock() for the entire popup lifetime.
                                if let Some((session_name, client)) = popup_info {
                                    let popup_result =
                                        client.attach_popup(&session_name, "90%", "90%");
                                    let session_live = client
                                        .session_has_live_pane(&session_name)
                                        .unwrap_or(false);

                                    let mut app = shared_app.lock().await;
                                    match popup_result {
                                        Err(e) => app.set_status(format!("Error: {}", e)),
                                        Ok(()) if !session_live => {
                                            let _ = app.refresh();
                                            app.set_status(format!(
                                                "{} exited; press Enter to restart it",
                                                session_name
                                            ));
                                        }
                                        Ok(()) => {
                                            if let Err(e) = app.refresh() {
                                                app.set_status(format!("Error: {}", e));
                                            }
                                        }
                                    }
                                }
                                // Discard ticks that accumulated while popup was open
                                events.drain();
                                terminal.clear()?;
                            } else {
                                // Outside tmux: temporarily exit alternate screen
                                if keyboard_enhanced {
                                    let _ = execute!(
                                        terminal.backend_mut(),
                                        PopKeyboardEnhancementFlags
                                    );
                                }
                                disable_raw_mode()?;
                                execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

                                let app = shared_app.lock().await;
                                let result = app.attach_selected();
                                drop(app);

                                // Restore terminal
                                execute!(terminal.backend_mut(), EnterAlternateScreen)?;
                                enable_raw_mode()?;
                                if keyboard_enhanced {
                                    let _ = execute!(
                                        terminal.backend_mut(),
                                        PushKeyboardEnhancementFlags(
                                            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                                        )
                                    );
                                }
                                events.drain();
                                terminal.clear()?;

                                if let Err(e) = result {
                                    let mut app = shared_app.lock().await;
                                    app.set_status(format!("Error: {}", e));
                                }
                            }

                            // Popup closed — clear so events resume delivery
                            *popup_receiver.lock().unwrap() = None;
                        }
                        KeyCode::Char('n') => {
                            if let Err(e) = app.spawn_agent() {
                                app.set_status(format!("Error: {}", e));
                            }
                        }
                        KeyCode::Char('d') if app.selected_agent().is_some() => {
                            app.pending_confirm = Some(app::ConfirmAction::Kill);
                        }
                        KeyCode::Char('N') => {
                            // Open EA name prompt to spawn a new EA
                            app.ea_input_mode = true;
                        }
                        KeyCode::Char('D') => {
                            // Delete the currently active EA (last EA is protected)
                            if app.registered_eas.len() > 1 {
                                app.pending_confirm = Some(app::ConfirmAction::DeleteEa);
                            } else {
                                app.set_status("Cannot delete the only EA");
                            }
                        }
                        KeyCode::Char('p') => {
                            app.project_input_mode = true;
                        }
                        KeyCode::Char('r') => {
                            if let Err(e) = app.refresh() {
                                app.set_status(format!("Error: {}", e));
                            } else {
                                app.set_status("Refreshed");
                            }
                        }
                        KeyCode::Char('e') => {
                            // Fix V2: EA-scoped events instead of global list
                            app.scheduled_events = scheduler.list_by_ea(app.active_ea);
                            app.scheduled_events.sort_by_key(|e| e.timestamp);
                            app.show_events = true;
                        }
                        KeyCode::Char('G') => {
                            app.show_debug_console = true;
                        }
                        // Detach from tmux — dashboard + agents keep running
                        KeyCode::Char('z') if std::env::var("TMUX").is_ok() => {
                            let _ = tmux_command().args(["detach-client"]).status();
                        }
                        KeyCode::Char('S') => {
                            app.show_settings = true;
                        }
                        KeyCode::Char('?') => {
                            app.show_help = !app.show_help;
                        }
                        _ => {}
                    }
                }
                AppEvent::Tick => {
                    let mut app = shared_app.lock().await;
                    // Rotate quotes every ~30 ticks
                    tick_count += 1;
                    if tick_count.is_multiple_of(30) {
                        app.quote_index = app.quote_index.wrapping_add(1);
                        sync_tmux_setup_warning(&mut app);
                    }

                    // Fix V2: EA-scoped events instead of global list
                    app.scheduled_events = scheduler.list_by_ea(app.active_ea);
                    app.scheduled_events.sort_by_key(|e| e.timestamp);

                    // Skip refresh while a popup/input overlay is active
                    // to avoid interrupting user input.
                    if !app.has_popup() {
                        app.clear_status();
                        if let Err(e) = app.refresh() {
                            app.set_status(format!("Error: {}", e));
                        }
                    }

                    // Keep system_state.md reasonably fresh without capturing
                    // the manager pane and rewriting JSON on every dashboard
                    // tick. State-changing actions still write immediately.
                    if tick_count.is_multiple_of(3) && !app.has_popup() {
                        let state_dir = app.state_dir();
                        let manager_session = app.manager_session_name();
                        memory::write_memory_to(
                            &state_dir,
                            &app.agents,
                            app.manager.as_ref(),
                            &manager_session,
                            app.client(),
                            &app.scheduled_events,
                        );
                    }
                }
                AppEvent::TickerScroll => {
                    let mut app = shared_app.lock().await;
                    app.ticker_offset = app.ticker_offset.wrapping_add(1);
                }
                AppEvent::Resize(_, _) => {
                    // Terminal will handle resize automatically
                }
            }
        }

        // Check quit flag
        let should_quit = {
            let app = shared_app.lock().await;
            app.should_quit
        };
        if should_quit {
            break;
        }
    }

    // Restore terminal
    if keyboard_enhanced {
        let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    }
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    // Kill ALL OMAR EA sessions on quit (managers + workers), even if
    // registry and tmux are temporarily out of sync.
    {
        let app = shared_app.lock().await;
        let client = TmuxClient::new("");
        let base_prefix = app.base_prefix.clone();

        if let Ok(sessions) = client.list_all_sessions() {
            for session in sessions {
                if session.name.starts_with(&base_prefix) {
                    let _ = client.kill_session(&session.name);
                }
            }
        }
    }

    // Kill Slack bridge on exit
    if let Some(ref mut child) = slack_bridge {
        kill_child_gracefully(child, Duration::from_secs(3));
    }

    // Kill computer bridge on exit
    if let Some(ref mut child) = computer_bridge {
        kill_child_gracefully(child, Duration::from_secs(3));
    }

    let reset_on_quit = {
        let app = shared_app.lock().await;
        app.reset_on_quit
    };
    if reset_on_quit {
        purge_persisted_runtime_state_on_quit(&omar_dir)?;
    }

    Ok(())
}

fn purge_persisted_runtime_state_on_quit(omar_dir: &std::path::Path) -> Result<()> {
    let archive_timestamp = now_ns();
    archive_action_logs(omar_dir, archive_timestamp)?;
    archive_manager_notes(omar_dir, archive_timestamp)?;

    for file in [
        "active_ea",
        "ea_next_id",
        "eas.json",
        "eas.json.tmp",
        "scheduled_events.json",
        "scheduled_events.lock",
        "scheduled_events.tmp",
    ] {
        remove_file_if_exists(omar_dir.join(file))?;
    }

    remove_dir_if_exists(omar_dir.join("ea"))?;
    remove_dir_if_exists(omar_dir.join("mcp"))?;
    manager::remove_all_omar_antigravity_mcp_configs()?;

    Ok(())
}

fn archive_action_logs(omar_dir: &std::path::Path, archive_timestamp: u64) -> Result<()> {
    let ea_dir = omar_dir.join("ea");
    let Ok(entries) = std::fs::read_dir(&ea_dir) else {
        return Ok(());
    };

    let archive_dir = omar_dir.join("logs").join("action_logs");
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let action_log = path.join("action_log.jsonl");
        if !action_log.exists() {
            continue;
        }

        std::fs::create_dir_all(&archive_dir)?;
        let ea_id = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("unknown");
        let archive_path = unique_archive_path(
            &archive_dir,
            &format!("ea-{}-{}", ea_id, archive_timestamp),
            "jsonl",
        );
        std::fs::rename(action_log, archive_path)?;
    }

    Ok(())
}

fn archive_manager_notes(omar_dir: &std::path::Path, archive_timestamp: u64) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(omar_dir) else {
        return Ok(());
    };

    let archive_dir = omar_dir.join("logs").join("manager_notes");
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !file_name.starts_with("manager_notes_ea") || !file_name.ends_with(".md") {
            continue;
        }

        std::fs::create_dir_all(&archive_dir)?;
        let stem = file_name.strip_suffix(".md").unwrap_or(file_name);
        let archive_path = unique_archive_path(
            &archive_dir,
            &format!("{}-{}", stem, archive_timestamp),
            "md",
        );
        std::fs::rename(path, archive_path)?;
    }

    Ok(())
}

fn unique_archive_path(dir: &std::path::Path, stem: &str, extension: &str) -> PathBuf {
    let mut path = dir.join(format!("{}.{}", stem, extension));
    let mut suffix = 1;
    while path.exists() {
        path = dir.join(format!("{}-{}.{}", stem, suffix, extension));
        suffix += 1;
    }
    path
}

fn remove_file_if_exists(path: PathBuf) -> Result<()> {
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn remove_dir_if_exists(path: PathBuf) -> Result<()> {
    match std::fs::remove_dir_all(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct HomeEnvGuard {
        previous: Option<std::ffi::OsString>,
    }

    impl HomeEnvGuard {
        fn set(path: &std::path::Path) -> Self {
            let previous = std::env::var_os("HOME");
            std::env::set_var("HOME", path);
            Self { previous }
        }
    }

    impl Drop for HomeEnvGuard {
        fn drop(&mut self) {
            match self.previous.as_ref() {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    /// Regression: `extended-keys always` forces tmux to emit modify-other-keys
    /// sequences to every client, including omar's dashboard, which doesn't
    /// push the kitty flag. Crossterm can't parse those sequences and silently
    /// collapses Shift+Tab to plain Tab — so `drill_up` never runs. `on`
    /// emits extended sequences only for clients that opt in via DECSET 2017,
    /// which keeps Shift+Enter working in Claude panes while leaving the
    /// dashboard on legacy xterm encoding (where Shift+Tab → `\x1b[Z` →
    /// `KeyCode::BackTab`). Do not flip back to `always`.
    #[test]
    fn tmux_extended_keys_recommendation_is_on_not_always() {
        let entry = TMUX_RECOMMENDED
            .iter()
            .find(|(opt, _, _)| *opt == "extended-keys")
            .expect("TMUX_RECOMMENDED must include an extended-keys entry");
        let value = entry.1.split_whitespace().last().unwrap_or("");
        assert_eq!(
            value, "on",
            "extended-keys must be `on`, not `{}` — `always` breaks Shift+Tab \
             in the dashboard (see tests comment)",
            value
        );
    }

    #[test]
    fn purge_persisted_runtime_state_archives_logs_and_notes() {
        let _env_lock = crate::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _home = HomeEnvGuard::set(dir.path());
        let omar_dir = dir.path();
        let agy_plugins_dir = dir.path().join(".gemini/config/plugins");
        std::fs::create_dir_all(agy_plugins_dir.join("omar-ea-7")).unwrap();
        std::fs::create_dir_all(agy_plugins_dir.join("user-plugin")).unwrap();
        let agy_manifest_path = dir.path().join(".gemini/config/import_manifest.json");
        std::fs::write(
            &agy_manifest_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "imports": [
                    {"name": "omar-ea-7", "source": "local-install"},
                    {"name": "user-plugin", "source": "local-install"}
                ]
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::create_dir_all(omar_dir.join("ea/7/status")).unwrap();
        std::fs::create_dir_all(omar_dir.join("mcp/ea-7")).unwrap();
        std::fs::create_dir_all(omar_dir.join("slack_outbox")).unwrap();
        std::fs::create_dir_all(omar_dir.join("logs/panics")).unwrap();
        std::fs::write(omar_dir.join("config.toml"), "[dashboard]\n").unwrap();
        std::fs::write(omar_dir.join("slack_outbox/keep"), "queued").unwrap();
        std::fs::write(omar_dir.join("logs/panics/panic.log"), "panic").unwrap();
        std::fs::write(omar_dir.join("eas.json"), "[]").unwrap();
        std::fs::write(omar_dir.join("active_ea"), "7").unwrap();
        std::fs::write(omar_dir.join("ea_next_id"), "7").unwrap();
        std::fs::write(omar_dir.join("scheduled_events.json"), "[]").unwrap();
        std::fs::write(omar_dir.join("ea/7/tasks.md"), "- [1] stale\n").unwrap();
        std::fs::write(omar_dir.join("ea/7/action_log.jsonl"), "action log\n").unwrap();
        std::fs::write(omar_dir.join("manager_notes_ea7.md"), "manager notes\n").unwrap();

        purge_persisted_runtime_state_on_quit(omar_dir).unwrap();

        assert!(omar_dir.join("config.toml").exists());
        assert!(omar_dir.join("slack_outbox/keep").exists());
        assert!(omar_dir.join("logs/panics/panic.log").exists());
        assert!(!omar_dir.join("eas.json").exists());
        assert!(!omar_dir.join("active_ea").exists());
        assert!(!omar_dir.join("ea_next_id").exists());
        assert!(!omar_dir.join("scheduled_events.json").exists());
        assert!(!omar_dir.join("ea").exists());
        assert!(!omar_dir.join("mcp").exists());
        assert!(!omar_dir.join("manager_notes_ea7.md").exists());
        assert!(!agy_plugins_dir.join("omar-ea-7").exists());
        assert!(agy_plugins_dir.join("user-plugin").exists());
        let agy_manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(agy_manifest_path).unwrap()).unwrap();
        assert_eq!(agy_manifest["imports"].as_array().unwrap().len(), 1);
        assert_eq!(agy_manifest["imports"][0]["name"], "user-plugin");

        let action_logs: Vec<_> = std::fs::read_dir(omar_dir.join("logs/action_logs"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(action_logs.len(), 1);
        assert_eq!(
            std::fs::read_to_string(&action_logs[0]).unwrap(),
            "action log\n"
        );

        let manager_notes: Vec<_> = std::fs::read_dir(omar_dir.join("logs/manager_notes"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(manager_notes.len(), 1);
        assert_eq!(
            std::fs::read_to_string(&manager_notes[0]).unwrap(),
            "manager notes\n"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn tmux_macos_copy_command_uses_pbcopy() {
        let entry = TMUX_PLATFORM_RECOMMENDED
            .iter()
            .find(|(opt, _, _)| *opt == "copy-command")
            .expect("macOS setup must configure tmux copy-command");
        let value = entry.1.split_whitespace().last().unwrap_or("");
        assert_eq!(
            value, "pbcopy",
            "tmux copy-command should pipe native tmux selections into the macOS clipboard"
        );
    }
}
