#![allow(dead_code)]

use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use crate::config::Config;
use crate::ea::{self, EaId, EaInfo};
use crate::memory;
use crate::projects::{self, Project};
use crate::scheduler::{ScheduledEvent, Scheduler, TickerBuffer};
use crate::tmux::{HealthChecker, HealthState, Session, TmuxClient};
use crate::DASHBOARD_SESSION;

/// What kind of confirmation the user is being prompted for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmAction {
    /// Kill the selected agent
    Kill,
    /// Quit and reset persisted OMAR runtime state.
    ResetQuit,
    /// Delete the currently active EA (blocked only if it is the last one)
    DeleteEa,
}

/// Which left-sidebar panel is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarPanel {
    Projects,
    Events,
    ChainOfCommand,
}

/// Information about an agent for display
#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub session: Session,
    pub health: HealthState,
    pub is_unresolved: bool,
}

/// A node in the chain-of-command tree
#[derive(Debug, Clone)]
pub struct CommandTreeNode {
    /// Display name (e.g. "Executive Assistant", "Project Manager: rest-api")
    pub name: String,
    /// Full tmux session name (empty for EA root)
    pub session_name: String,
    /// Health state of this agent
    pub health: HealthState,
    /// Depth in the tree (0 = EA, 1 = PM, 2 = worker)
    pub depth: usize,
    /// Whether this is the last sibling at its depth
    pub is_last_sibling: bool,
    /// For each ancestor depth, whether that ancestor was the last sibling.
    /// Used to decide whether to draw "│" or " " for vertical continuation lines.
    pub ancestor_is_last: Vec<bool>,
    /// Whether this node came from a non-canonical OMAR session name.
    pub is_unresolved: bool,
}

/// Application state
pub struct App {
    // EA fields
    pub active_ea: EaId,
    pub registered_eas: Vec<EaInfo>,
    pub base_prefix: String,
    pub omar_dir: PathBuf,

    // Existing fields (scoped to active_ea)
    pub agents: Vec<AgentInfo>,
    pub manager: Option<AgentInfo>,
    pub command_tree: Vec<CommandTreeNode>,
    pub selected: usize,
    pub manager_selected: bool,
    pub should_quit: bool,
    pub reset_on_quit: bool,
    pub show_help: bool,
    pub pending_confirm: Option<ConfirmAction>,
    pub filter: String,
    pub status_message: Option<String>,
    status_set_at: Option<Instant>,
    /// Warning that persists across tick clears (e.g., tmux misconfiguration)
    pub persistent_warning: Option<String>,
    pub projects: Vec<Project>,
    pub project_input_mode: bool,
    pub project_input: String,
    pub ea_input_mode: bool,
    pub ea_input: String,
    pub show_events: bool,
    /// Enlarged sidebar popup (None = hidden)
    pub sidebar_popup: Option<SidebarPanel>,
    pub scheduled_events: Vec<ScheduledEvent>,
    pub ticker: TickerBuffer,
    pub ticker_offset: usize,
    pub quote_index: usize,
    pub quote_order: Vec<usize>,
    pub show_debug_console: bool,
    pub show_settings: bool,
    pub settings_selected: usize,
    /// `Some(buffer)` while the user is typing into a text-typed setting
    /// row. `None` otherwise. The buffer holds the live edit; it is only
    /// committed to `config` on Enter.
    pub settings_edit_buffer: Option<String>,
    pub config: Config,
    /// Session name of the agent shown in the bottom panel (the EA's manager session)
    pub focus_parent: String,
    /// Stack for Esc navigation (drill-up restores previous parent)
    focus_stack: Vec<String>,
    /// Indices into self.agents for the current focus_parent's direct children
    pub focus_child_indices: Vec<usize>,
    agent_parents: HashMap<String, String>,
    worker_tasks: HashMap<String, String>,
    /// Whether the left sidebar is focused (vs the right agent panels)
    pub sidebar_focused: bool,
    /// Which sidebar panel is active
    pub sidebar_panel: SidebarPanel,
    client: TmuxClient,
    health_checker: HealthChecker,
    health_threshold: i64,
    default_command: String,
    default_workdir: String,
    pub scheduler: Arc<Scheduler>,
}

impl App {
    pub fn new(config: &Config, ticker: TickerBuffer, scheduler: Arc<Scheduler>) -> Self {
        let omar_dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".omar");

        Self::new_with_omar_dir(config, ticker, scheduler, omar_dir)
    }

    fn new_with_omar_dir(
        config: &Config,
        ticker: TickerBuffer,
        scheduler: Arc<Scheduler>,
        omar_dir: PathBuf,
    ) -> Self {
        let base_prefix = config.dashboard.session_prefix.clone();

        let registered_eas = ea::ensure_default_ea(&omar_dir).unwrap_or_else(|e| {
            eprintln!("warn: ensure default EA: {}", e);
            ea::load_registry(&omar_dir)
        });
        let active_ea = ea::resolve_active_ea(&omar_dir, &registered_eas);

        // EA-scoped prefix and manager session
        let session_prefix = ea::ea_prefix(active_ea, &base_prefix);
        let manager_session = ea::ea_manager_session(active_ea, &base_prefix);

        let client = TmuxClient::new(&session_prefix);
        let health_checker = HealthChecker::new(client.clone(), config.health.idle_warning);

        let state_dir = ea::ea_state_dir(active_ea, &omar_dir);
        std::fs::create_dir_all(state_dir.join("status")).ok();

        Self {
            active_ea,
            registered_eas,
            base_prefix,
            omar_dir,
            agents: Vec::new(),
            manager: None,
            command_tree: Vec::new(),
            selected: 0,
            manager_selected: true,
            should_quit: false,
            reset_on_quit: false,
            show_help: false,
            pending_confirm: None,
            filter: String::new(),
            status_message: None,
            status_set_at: None,
            persistent_warning: None,
            projects: projects::load_projects_from(&state_dir),
            project_input_mode: false,
            project_input: String::new(),
            ea_input_mode: false,
            ea_input: String::new(),
            show_events: false,
            sidebar_popup: None,
            scheduled_events: Vec::new(),
            ticker,
            ticker_offset: 0,
            quote_index: 0,
            quote_order: {
                // Shuffle quote indices using time-seeded LCG
                let n = crate::ui::QUOTE_COUNT;
                let mut order: Vec<usize> = (0..n).collect();
                let mut seed = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64;
                for i in (1..n).rev() {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let j = (seed >> 33) as usize % (i + 1);
                    order.swap(i, j);
                }
                order
            },
            show_debug_console: false,
            show_settings: false,
            settings_selected: 0,
            settings_edit_buffer: None,
            config: config.clone(),
            focus_parent: manager_session,
            focus_stack: Vec::new(),
            focus_child_indices: Vec::new(),
            agent_parents: HashMap::new(),
            worker_tasks: HashMap::new(),
            sidebar_focused: false,
            sidebar_panel: SidebarPanel::Projects,
            client,
            health_checker,
            health_threshold: config.health.idle_warning,
            default_command: config.agent.default_command.clone(),
            default_workdir: config.agent.default_workdir.clone(),
            scheduler,
        }
    }

    /// EA state directory for the active EA
    pub fn state_dir(&self) -> PathBuf {
        ea::ea_state_dir(self.active_ea, &self.omar_dir)
    }

    /// Manager session name for the active EA
    pub fn manager_session_name(&self) -> String {
        ea::ea_manager_session(self.active_ea, &self.base_prefix)
    }

    fn active_session_prefix(&self) -> String {
        ea::ea_prefix(self.active_ea, &self.base_prefix)
    }

    fn short_session_name<'a>(&self, session_name: &'a str) -> &'a str {
        let active_prefix = self.active_session_prefix();
        session_name
            .strip_prefix(&active_prefix)
            .or_else(|| session_name.strip_prefix(&self.base_prefix))
            .unwrap_or(session_name)
    }

    /// True when any popup or input overlay is active.
    pub fn has_popup(&self) -> bool {
        self.show_help
            || self.pending_confirm.is_some()
            || self.project_input_mode
            || self.ea_input_mode
            || self.show_events
            || self.show_debug_console
            || self.show_settings
            || self.sidebar_popup.is_some()
    }

    pub fn client(&self) -> &TmuxClient {
        &self.client
    }

    /// Refresh the list of agents (scoped to active EA)
    pub fn refresh(&mut self) -> Result<()> {
        self.apply_dashboard_launch_handoff()?;
        self.registered_eas = ea::load_registry(&self.omar_dir);

        // Pick up out-of-band EA switches (e.g. via the MCP `switch_ea` tool)
        // that mutated `~/.omar/active_ea` while the dashboard was running.
        // `load_active_ea` returns None on read/parse failure, in which case
        // we keep the current value rather than crash the UI loop.
        if let Some(persisted) = ea::load_active_ea(&self.omar_dir) {
            if persisted != self.active_ea && self.registered_eas.iter().any(|e| e.id == persisted)
            {
                return self.switch_ea(persisted);
            }
        }

        // Ensure manager exists
        self.ensure_manager()?;

        // Get all sessions (used for both active EA state and multi-EA CoC sidebar)
        let all_sessions = self.client.list_all_sessions()?;
        // Only snapshot health for OMAR-owned sessions. Every EA session name
        // starts with `base_prefix` (e.g. "omar-agent-") and the manager is
        // `<base_prefix>ea-<id>` — so the prefix check covers both agents and
        // managers across all EAs. This avoids running a tmux `capture-pane`
        // per unrelated shell session on hosts where the user has many tmux
        // sessions for their own work.
        let mut health_snapshot: HashMap<String, HealthState> = HashMap::new();
        for session in &all_sessions {
            if !self.base_prefix.is_empty() && !session.name.starts_with(&self.base_prefix) {
                continue;
            }
            health_snapshot.insert(
                session.name.clone(),
                self.health_checker.check(&session.name),
            );
        }

        let mut managers_by_ea: HashMap<EaId, Session> = HashMap::new();
        let mut agents_by_ea: HashMap<EaId, Vec<Session>> = HashMap::new();
        let mut unresolved_sessions: Vec<Session> = Vec::new();
        for session in &all_sessions {
            if session.name == DASHBOARD_SESSION
                || (!self.base_prefix.is_empty() && !session.name.starts_with(&self.base_prefix))
            {
                continue;
            }
            match parse_ea_session_owner(&session.name, &self.base_prefix) {
                Some(ParseSessionOwner::Manager(ea_id)) => {
                    managers_by_ea.insert(ea_id, session.clone());
                }
                Some(ParseSessionOwner::Worker(ea_id)) => {
                    agents_by_ea.entry(ea_id).or_default().push(session.clone());
                }
                Some(ParseSessionOwner::Unresolved) => {
                    unresolved_sessions.push(session.clone());
                }
                None => {}
            }
        }

        let unresolved_names: HashSet<String> = unresolved_sessions
            .iter()
            .map(|session| session.name.clone())
            .collect();

        // Update manager info
        self.manager = managers_by_ea.get(&self.active_ea).cloned().map(|session| {
            let health = health_snapshot
                .get(&session.name)
                .copied()
                .unwrap_or_else(|| self.health_checker.check(&session.name));
            AgentInfo {
                session,
                health,
                is_unresolved: false,
            }
        });

        // Update agents list.
        // NOTE: We intentionally include attached sessions. When a user opens
        // the popup view (tmux display-popup + attach), the agent session
        // becomes "attached" but is still a valid agent. Filtering attached
        // sessions would cause the API to return "not found" for that agent.
        let mut active_agents = agents_by_ea
            .get(&self.active_ea)
            .cloned()
            .unwrap_or_default();
        active_agents.extend(unresolved_sessions.clone());
        self.agents = active_agents
            .into_iter()
            .map(|session| {
                let health = health_snapshot
                    .get(&session.name)
                    .copied()
                    .unwrap_or_else(|| self.health_checker.check(&session.name));
                let is_unresolved = unresolved_names.contains(&session.name);
                AgentInfo {
                    session,
                    health,
                    is_unresolved,
                }
            })
            .collect();

        // Clean up stale frame data for sessions that no longer exist
        let active: Vec<String> = self
            .agents
            .iter()
            .map(|a| a.session.name.clone())
            .chain(self.manager.iter().map(|m| m.session.name.clone()))
            .collect();
        self.health_checker.retain_sessions(&active);

        // Apply filter if set
        if !self.filter.is_empty() {
            let filter = self.filter.to_lowercase();
            self.agents
                .retain(|a| a.session.name.to_lowercase().contains(&filter));
        }

        // Reload projects from EA-scoped file (picks up API-side changes)
        let state_dir = self.state_dir();
        self.projects = projects::load_projects_from(&state_dir);

        // Load parent mappings, worker tasks, and build the chain-of-command tree
        self.agent_parents = memory::load_agent_parents_from(&state_dir);
        self.worker_tasks = memory::load_worker_tasks_from(&state_dir);

        // Build multi-EA CoC: all EAs sorted by ID, each with its real subtree and health.

        let mut sorted_eas = self.registered_eas.clone();
        sorted_eas.sort_by_key(|e| e.id);

        let mut all_nodes: Vec<CommandTreeNode> = Vec::new();
        for ea_info in &sorted_eas {
            let ea_prefix = ea::ea_prefix(ea_info.id, &self.base_prefix);
            let ea_manager = ea::ea_manager_session(ea_info.id, &self.base_prefix);
            let ea_state_dir = ea::ea_state_dir(ea_info.id, &self.omar_dir);
            let ea_parents = memory::load_agent_parents_from(&ea_state_dir);

            let manager_info = managers_by_ea.get(&ea_info.id).cloned().map(|session| {
                let health = health_snapshot
                    .get(&session.name)
                    .copied()
                    .unwrap_or_else(|| self.health_checker.check(&session.name));
                AgentInfo {
                    session,
                    health,
                    is_unresolved: false,
                }
            });
            let ea_agents: Vec<AgentInfo> = agents_by_ea
                .get(&ea_info.id)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|session| {
                    let health = health_snapshot
                        .get(&session.name)
                        .copied()
                        .unwrap_or_else(|| self.health_checker.check(&session.name));
                    AgentInfo {
                        session: session.clone(),
                        health,
                        is_unresolved: unresolved_names.contains(&session.name)
                            && ea_info.id == self.active_ea,
                    }
                })
                .collect();
            let mut ea_agents = ea_agents;
            if ea_info.id == self.active_ea {
                ea_agents.extend(unresolved_sessions.iter().cloned().map(|session| {
                    let health = health_snapshot
                        .get(&session.name)
                        .copied()
                        .unwrap_or_else(|| self.health_checker.check(&session.name));
                    AgentInfo {
                        session,
                        health,
                        is_unresolved: true,
                    }
                }));
            }

            let mut nodes = build_tree(
                &ea_agents,
                manager_info.as_ref(),
                &ea_parents,
                &ea_prefix,
                &ea_manager,
            );
            if let Some(root) = nodes.first_mut() {
                root.name = ea_info.name.clone();
            }
            all_nodes.extend(nodes);
        }
        self.command_tree = all_nodes;

        // Recompute focus children indices
        self.focus_child_indices = self.compute_focus_child_indices();

        // Keep selection in bounds relative to focus children
        if !self.manager_selected
            && !self.focus_child_indices.is_empty()
            && self.selected >= self.focus_child_indices.len()
        {
            self.selected = self.focus_child_indices.len() - 1;
        }

        Ok(())
    }

    fn apply_dashboard_launch_handoff(&mut self) -> Result<()> {
        let Some(handoff) = ea::take_dashboard_launch_handoff(&self.omar_dir) else {
            return Ok(());
        };

        // Capture the pre-handoff backend so we only restart the manager when
        // the user actually switched backends. `omar -a claude` while already
        // on claude shouldn't kill the running EA — that throws away the
        // backend's in-memory conversation for nothing.
        let backend_changed = self.default_command != handoff.default_command;

        self.config.agent.default_command = handoff.default_command.clone();
        self.config.agent.default_workdir = handoff.default_workdir.clone();
        self.default_command = handoff.default_command;
        self.default_workdir = handoff.default_workdir;
        self.activate_ea_local(handoff.active_ea)?;
        ea::save_active_ea(&self.omar_dir, handoff.active_ea)?;

        if handoff.restart_manager && backend_changed {
            let manager_session = ea::ea_manager_session(handoff.active_ea, &self.base_prefix);
            if self.client.has_session(&manager_session)? {
                self.client.kill_session(&manager_session)?;
                self.manager = None;
            }
        }
        Ok(())
    }

    /// Build the startup command attempts for the manager.
    ///
    /// In strict mode we issue exactly one launch command so behavior matches
    /// main and does not silently degrade to alternate variants.
    fn manager_startup_attempts(&self) -> Vec<(String, bool)> {
        vec![(self.default_command.clone(), true)]
    }

    /// Ensure manager session exists, start if not
    fn ensure_manager(&mut self) -> Result<()> {
        let canonical_manager_session = self.manager_session_name();
        let manager_session = canonical_manager_session.clone();

        if self.client.has_session(&manager_session)? {
            if self.client.session_has_live_pane(&manager_session)? {
                return Ok(());
            }
            let _ = self.client.kill_session(&manager_session);
        }

        // Reload registry on cache miss so we have the latest EA names
        self.registered_eas = ea::load_registry(&self.omar_dir);

        // Get EA name for prompt
        let ea_name = self
            .registered_eas
            .iter()
            .find(|ea| ea.id == self.active_ea)
            .map(|ea| ea.name.as_str())
            .unwrap_or("Default");

        let workdir = self.default_workdir.clone();

        let (default_command, inject_prompt) = self
            .manager_startup_attempts()
            .into_iter()
            .next()
            .unwrap_or_else(|| (self.default_command.clone(), true));

        let context = crate::manager::McpLaunchContext {
            omar_dir: self.omar_dir.clone(),
            ea_id: self.active_ea,
            session_prefix: self.base_prefix.clone(),
            default_command: default_command.clone(),
            default_workdir: self.default_workdir.clone(),
            health_idle_warning: self.health_threshold,
            tmux_server: std::env::var("OMAR_TMUX_SERVER")
                .ok()
                .map(|server| server.trim().to_string())
                .filter(|server| !server.is_empty()),
            topology: None,
        };

        let (cmd, workspace_cwd) = if inject_prompt {
            crate::manager::build_ea_command(
                &default_command,
                self.active_ea,
                ea_name,
                &self.omar_dir,
                &context,
            )
        } else {
            (default_command.clone(), None)
        };

        // For backends whose manager prompt is now loaded from an auto-
        // discovered file (codex `AGENTS.md`, agy MCP plugin config, opencode
        // `AGENTS.md`), build_ea_command returns the workspace dir we must
        // launch in. Fall back to the user's workdir for claude/cursor.
        let launch_cwd = workspace_cwd
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or(workdir);

        self.client
            .new_session(&manager_session, &cmd, Some(&launch_cwd))
            .map_err(|err| {
                let msg = format!(
                    "tmux failed to start manager '{}' (cwd '{}') with command '{}': {}",
                    manager_session, launch_cwd, default_command, err
                );
                anyhow::anyhow!(msg)
            })?;

        let state_dir = self.state_dir();
        let events = self.scheduler.list_by_ea(self.active_ea);
        memory::write_memory_to(
            &state_dir,
            &self.agents,
            None,
            &manager_session,
            &self.client,
            &events,
        );
        Ok(())
    }

    /// Get default command
    pub fn default_command(&self) -> &str {
        &self.default_command
    }

    /// Get the worker_tasks map (for display)
    pub fn worker_tasks(&self) -> &HashMap<String, String> {
        &self.worker_tasks
    }

    /// Compute indices into self.agents for focus_parent's direct children
    fn compute_focus_child_indices(&self) -> Vec<usize> {
        let manager_session = self.manager_session_name();
        let mut indices = Vec::new();
        if self.focus_parent == manager_session {
            // Root view: show agents that are direct children of EA, plus orphans
            for (i, agent) in self.agents.iter().enumerate() {
                let parent = self.agent_parents.get(&agent.session.name);
                match parent {
                    // Explicit child of EA (manager session)
                    Some(p) if *p == manager_session => indices.push(i),
                    // Has a live parent that is NOT the EA → belongs deeper in the tree
                    Some(p) if self.agents.iter().any(|a| a.session.name == *p) => {}
                    // Parent is dead or missing → show as orphan at root
                    _ => indices.push(i),
                }
            }
        } else {
            // Non-root: show agents whose parent matches focus_parent
            for (i, agent) in self.agents.iter().enumerate() {
                if let Some(parent) = self.agent_parents.get(&agent.session.name) {
                    if *parent == self.focus_parent {
                        indices.push(i);
                    }
                }
            }
        }
        indices
    }

    /// Get the direct children of the current focus parent
    pub fn focus_children(&self) -> Vec<&AgentInfo> {
        self.focus_child_indices
            .iter()
            .filter_map(|&i| self.agents.get(i))
            .collect()
    }

    /// Get AgentInfo for the focus parent
    pub fn focus_parent_info(&self) -> Option<&AgentInfo> {
        let manager_session = self.manager_session_name();
        if self.focus_parent == manager_session {
            self.manager.as_ref()
        } else {
            self.agents
                .iter()
                .find(|a| a.session.name == self.focus_parent)
        }
    }

    /// Count children for a given agent
    pub fn child_count(&self, session_name: &str) -> usize {
        if session_name == self.manager_session_name() {
            // Count PMs + orphans
            return self.compute_focus_child_indices().len();
        }
        self.agent_parents
            .values()
            .filter(|p| *p == session_name)
            .count()
    }

    /// Drill down into the selected agent (Tab).
    pub fn drill_down(&mut self) {
        let session_name = if self.manager_selected {
            // On EA: hint to select a child if children exist, otherwise silent
            if !self.focus_child_indices.is_empty() {
                self.set_status("Highlight sub-agents to drill into them");
            }
            return;
        } else {
            // Get the session name of the selected focus child
            if let Some(&idx) = self.focus_child_indices.get(self.selected) {
                if let Some(agent) = self.agents.get(idx) {
                    agent.session.name.clone()
                } else {
                    self.set_status("No agent selected");
                    return;
                }
            } else {
                self.set_status("No agent selected");
                return;
            }
        };

        self.focus_stack.push(self.focus_parent.clone());
        self.focus_parent = session_name.clone();
        self.selected = 0;
        self.focus_child_indices = self.compute_focus_child_indices();
        // Park the cursor on the focus parent when it has no children yet,
        // otherwise `selected = 0` would point at a nonexistent child until
        // the user spawns one.
        self.manager_selected = self.focus_child_indices.is_empty();

        let short = self.short_session_name(&session_name);
        self.set_status(format!("Viewing: {}", short));
    }

    /// Drill up to the parent view (Esc). Returns true if drilled up, false if at root.
    pub fn drill_up(&mut self) -> bool {
        if self.focus_stack.is_empty() {
            self.set_status("Already at the top level");
            return false;
        }
        self.focus_parent = self.focus_stack.pop().expect("checked is_empty above");
        self.selected = 0;
        self.manager_selected = true;
        self.focus_child_indices = self.compute_focus_child_indices();
        true
    }

    /// Move selection down (within focus children + focus parent)
    pub fn next(&mut self) {
        let child_count = self.focus_child_indices.len();
        if self.manager_selected {
            // From focus parent, go to first child (if any)
            if child_count > 0 {
                self.manager_selected = false;
                self.selected = 0;
            }
        } else if child_count > 0 {
            if self.selected + 1 >= child_count {
                // From last child, go to focus parent
                self.manager_selected = true;
            } else {
                self.selected += 1;
            }
        } else {
            self.manager_selected = true;
        }
    }

    /// Move selection up (within focus children + focus parent)
    pub fn previous(&mut self) {
        let child_count = self.focus_child_indices.len();
        if self.manager_selected {
            // From focus parent, go to last child (if any)
            if child_count > 0 {
                self.manager_selected = false;
                self.selected = child_count - 1;
            }
        } else if child_count > 0 {
            if self.selected == 0 {
                self.manager_selected = true;
            } else {
                self.selected -= 1;
            }
        } else {
            self.manager_selected = true;
        }
    }

    /// Move sidebar focus to the next panel, skipping Events when hidden.
    pub fn sidebar_next(&mut self) {
        self.sidebar_panel = match self.sidebar_panel {
            SidebarPanel::Projects => {
                if self.config.dashboard.show_event_queue {
                    SidebarPanel::Events
                } else {
                    SidebarPanel::ChainOfCommand
                }
            }
            SidebarPanel::Events => SidebarPanel::ChainOfCommand,
            SidebarPanel::ChainOfCommand => SidebarPanel::Projects,
        };
    }

    /// Move sidebar focus to the previous panel, skipping Events when hidden.
    pub fn sidebar_previous(&mut self) {
        self.sidebar_panel = match self.sidebar_panel {
            SidebarPanel::Projects => SidebarPanel::ChainOfCommand,
            SidebarPanel::Events => SidebarPanel::Projects,
            SidebarPanel::ChainOfCommand => {
                if self.config.dashboard.show_event_queue {
                    SidebarPanel::Events
                } else {
                    SidebarPanel::Projects
                }
            }
        };
    }

    /// Grid column count (matches render_agent_grid logic).
    fn grid_cols(&self) -> usize {
        2.min(self.focus_child_indices.len()).max(1)
    }

    /// Try to move selection left within the agent grid.
    /// Returns true if the move happened, false if already at the left edge.
    pub fn grid_left(&mut self) -> bool {
        if self.sidebar_focused || self.manager_selected {
            return false;
        }
        let cols = self.grid_cols();
        if cols <= 1 {
            return false;
        }
        let col = self.selected % cols;
        if col > 0 {
            self.selected -= 1;
            true
        } else {
            false
        }
    }

    /// Try to move selection right within the agent grid.
    /// Returns true if the move happened, false if already at the right edge.
    pub fn grid_right(&mut self) -> bool {
        if self.sidebar_focused || self.manager_selected {
            return false;
        }
        let cols = self.grid_cols();
        if cols <= 1 {
            return false;
        }
        let col = self.selected % cols;
        let child_count = self.focus_child_indices.len();
        if col + 1 < cols && self.selected + 1 < child_count {
            self.selected += 1;
            true
        } else {
            false
        }
    }

    /// Get currently selected agent (could be focus parent or a focus child)
    pub fn selected_agent(&self) -> Option<&AgentInfo> {
        if self.manager_selected {
            self.focus_parent_info()
        } else {
            self.focus_child_indices
                .get(self.selected)
                .and_then(|&idx| self.agents.get(idx))
        }
    }

    /// Get the short name (receiver name) of the selected agent.
    pub fn selected_agent_short_name(&self) -> Option<String> {
        self.selected_agent().map(|a| {
            a.session
                .name
                .strip_prefix(self.client.prefix())
                .unwrap_or(&a.session.name)
                .to_string()
        })
    }

    /// Receiver-side name for the selected agent, suitable for matching the
    /// `receiver` field of a `ScheduledEvent`.
    ///
    /// Workers are addressed by their short name (e.g., "worker1").
    /// The EA manager is addressed as "ea" — the session name
    /// `omar-agent-ea-<id>` never appears in scheduled-event payloads.
    ///
    /// Used by the dashboard when the user opens an agent popup so the
    /// scheduler can defer events bound for that pane while it is open.
    pub fn selected_popup_receiver_name(&self) -> Option<String> {
        let selected = self.selected_agent()?;
        Some(popup_receiver_name_for(
            &selected.session.name,
            &self.manager_session_name(),
            self.client.prefix(),
        ))
    }

    /// Attach to the selected agent via popup
    pub fn attach_selected(&self) -> Result<()> {
        if let Some(agent) = self.selected_agent() {
            self.client
                .attach_popup(&agent.session.name, "90%", "90%")?;
        }
        Ok(())
    }

    /// Kill the selected agent
    pub fn kill_selected(&mut self) -> Result<()> {
        let manager_session = self.manager_session_name();
        if let Some(agent) = self.selected_agent() {
            // Safety: don't kill attached sessions (user's terminal)
            if self
                .client
                .ensure_session_not_attached(&agent.session.name)
                .is_err()
            {
                self.set_status("Cannot kill attached session");
                self.pending_confirm = None;
                return Ok(());
            }

            // Safety: don't kill manager from 'd' key (use separate mechanism)
            if agent.session.name == manager_session {
                self.status_message = Some("Cannot kill manager with 'd'".to_string());
                self.pending_confirm = None;
                return Ok(());
            }

            let name = agent.session.name.clone();
            let state_dir = self.state_dir();

            let short_name = name
                .strip_prefix(self.client.prefix())
                .unwrap_or(&name)
                .to_string();
            // Cancel any scheduled events targeting this agent (the outer
            // main-loop handler also cancels; this keeps kill_selected
            // self-contained for any other caller).
            self.scheduler
                .cancel_by_receiver_and_ea(&short_name, self.active_ea);

            self.client.kill_session(&name)?;
            memory::remove_agent_parent_in(&state_dir, &name);
            self.status_message = Some(format!("Killed agent: {}", name));
            self.refresh()?;
            let events = self.scheduler.list_by_ea(self.active_ea);
            memory::write_memory_to(
                &state_dir,
                &self.agents,
                self.manager.as_ref(),
                &manager_session,
                &self.client,
                &events,
            );
        }
        self.pending_confirm = None;
        Ok(())
    }

    /// Generate a unique agent name (within the active EA's namespace).
    pub fn generate_agent_name(&self) -> String {
        let mut existing: std::collections::HashSet<String> =
            self.agents.iter().map(|a| a.session.name.clone()).collect();
        if let Ok(live_sessions) = self.client.list_sessions() {
            existing.extend(live_sessions.into_iter().map(|session| session.name));
        }
        let existing_refs: std::collections::HashSet<&str> =
            existing.iter().map(String::as_str).collect();
        next_agent_name(&self.active_session_prefix(), &existing_refs)
    }

    /// Spawn a new agent with default settings
    pub fn spawn_agent(&mut self) -> Result<()> {
        // Refresh first to get current state
        self.refresh()?;

        let workdir = if self.config.agent.default_workdir == "." {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| ".".to_string())
        } else {
            self.config.agent.default_workdir.clone()
        };

        let mut name = None;
        for _ in 0..5 {
            let candidate = self.generate_agent_name();
            if self.client.has_session(&candidate).unwrap_or(false) {
                self.refresh()?;
                continue;
            }
            match self.client.new_session(
                &candidate,
                &self.config.agent.default_command,
                Some(&workdir),
            ) {
                Ok(()) => {
                    name = Some(candidate);
                    break;
                }
                Err(err) if err.to_string().contains("duplicate session") => {
                    self.refresh()?;
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
        let name = name.ok_or_else(|| anyhow::anyhow!("Unable to allocate a unique agent name"))?;

        let state_dir = self.state_dir();
        memory::save_agent_parent_in(&state_dir, &name, &self.focus_parent);

        let short_name = self.short_session_name(&name).to_string();

        let state_dir = self.state_dir();
        memory::save_worker_task_in(&state_dir, &name, "dashboard-manual spawn");

        self.set_status(format!("Spawned agent: {}", short_name));
        self.refresh()?;

        if let Some(pos) = focus_view_index(&self.agents, &self.focus_child_indices, &name) {
            self.selected = pos;
            self.manager_selected = false;
        }

        let manager_session = self.manager_session_name();
        let events = self.scheduler.list_by_ea(self.active_ea);
        memory::write_memory_to(
            &state_dir,
            &self.agents,
            self.manager.as_ref(),
            &manager_session,
            &self.client,
            &events,
        );

        Ok(())
    }

    /// Set status message (persists for 3 seconds before auto-clearing)
    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_message = Some(msg.into());
        self.status_set_at = Some(Instant::now());
    }

    /// Set a persistent warning that survives clear_status() calls
    pub fn set_persistent_warning(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        self.persistent_warning = Some(msg.clone());
        self.status_message = Some(msg);
        self.status_set_at = None; // persistent warnings don't expire
    }

    /// Set a warning only when no unrelated persistent warning is active.
    pub fn set_persistent_warning_if_clear_or_same(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        if self.persistent_warning.is_none() || self.persistent_warning.as_deref() == Some(&msg) {
            self.set_persistent_warning(msg);
        }
    }

    /// Clear a specific persistent warning without disturbing unrelated warnings.
    pub fn clear_persistent_warning_if(&mut self, msg: &str) {
        if self.persistent_warning.as_deref() == Some(msg) {
            self.persistent_warning = None;
            if self.status_message.as_deref() == Some(msg) {
                self.status_message = None;
                self.status_set_at = None;
            }
        }
    }

    /// Clear status message if it has expired (3 seconds).
    /// Persistent warnings are restored.
    pub fn clear_status(&mut self) {
        let expired = self
            .status_set_at
            .map(|t| t.elapsed().as_secs() >= 3)
            .unwrap_or(true);
        if expired {
            self.status_message = self.persistent_warning.clone();
            self.status_set_at = None;
        }
    }

    /// Get counts by health state: (running, idle)
    /// Includes manager in the count
    pub fn health_counts(&self) -> (usize, usize) {
        let mut running = 0;
        let mut idle = 0;

        for agent in &self.agents {
            match agent.health {
                HealthState::Running => running += 1,
                HealthState::Idle => idle += 1,
            }
        }

        if let Some(ref manager) = self.manager {
            match manager.health {
                HealthState::Running => running += 1,
                HealthState::Idle => idle += 1,
            }
        }

        (running, idle)
    }

    /// Get total agent count (including manager)
    pub fn total_agents(&self) -> usize {
        self.agents.len() + if self.manager.is_some() { 1 } else { 0 }
    }

    /// Get focus parent pane output (more lines for display)
    pub fn get_focus_parent_output(&self, lines: i32) -> Result<String> {
        self.client.capture_pane(&self.focus_parent, lines)
    }

    /// Get agent pane output by session name
    pub fn get_agent_output(&self, session: &str, lines: i32) -> Result<String> {
        self.client.capture_pane(session, lines)
    }

    /// Add a project and update memory (EA-scoped)
    pub fn add_project(&mut self, name: &str) {
        let state_dir = self.state_dir();
        let _ = projects::add_project_in(&state_dir, name);
        self.projects = projects::load_projects_from(&state_dir);
        let manager_session = self.manager_session_name();
        let events = self.scheduler.list_by_ea(self.active_ea);
        memory::write_memory_to(
            &state_dir,
            &self.agents,
            self.manager.as_ref(),
            &manager_session,
            &self.client,
            &events,
        );
    }

    /// Complete (remove) a project by id and update memory (EA-scoped)
    pub fn complete_project(&mut self, id: usize) {
        let state_dir = self.state_dir();
        let active_sessions: Vec<String> = memory::load_agent_projects_from(&state_dir)
            .into_iter()
            .filter_map(|(session_name, project_id)| {
                if project_id == id && self.client.has_session(&session_name).unwrap_or(false) {
                    Some(session_name)
                } else {
                    None
                }
            })
            .collect();
        if !active_sessions.is_empty() {
            self.set_status(format!(
                "Project {} still has active agents: {}",
                id,
                active_sessions.join(", ")
            ));
            return;
        }
        let _ = projects::remove_project_in(&state_dir, id);
        self.projects = projects::load_projects_from(&state_dir);
        let manager_session = self.manager_session_name();
        let events = self.scheduler.list_by_ea(self.active_ea);
        memory::write_memory_to(
            &state_dir,
            &self.agents,
            self.manager.as_ref(),
            &manager_session,
            &self.client,
            &events,
        );
    }

    // ── Multi-EA methods ──

    /// Switch the dashboard to a different EA. Full reload of all state.
    pub fn switch_ea(&mut self, ea_id: EaId) -> Result<()> {
        self.activate_ea_local(ea_id)?;
        ea::save_active_ea(&self.omar_dir, ea_id)?;

        // Refresh discovers agents via the new prefix and reloads all EA state
        self.refresh()?;
        let ea_label = self
            .registered_eas
            .iter()
            .find(|ea| ea.id == ea_id)
            .map(|ea| ea.name.clone())
            .unwrap_or_else(|| format!("EA {}", ea_id));
        self.set_status(format!("Switched to {}", ea_label));
        self.ea_input_mode = false;
        self.ea_input.clear();
        self.project_input_mode = false;
        self.show_help = false;
        self.show_events = false;
        self.show_debug_console = false;
        self.show_settings = false;
        self.settings_edit_buffer = None;
        self.sidebar_popup = None;
        self.pending_confirm = None;
        Ok(())
    }

    fn activate_ea_local(&mut self, ea_id: EaId) -> Result<()> {
        self.registered_eas = ea::load_registry(&self.omar_dir);
        if !self.registered_eas.iter().any(|e| e.id == ea_id) {
            anyhow::bail!("EA {} not registered", ea_id);
        }

        self.active_ea = ea_id;
        let new_prefix = ea::ea_prefix(ea_id, &self.base_prefix);
        self.client = TmuxClient::new(&new_prefix);
        self.health_checker = HealthChecker::new(self.client.clone(), self.health_threshold);
        self.focus_parent = ea::ea_manager_session(ea_id, &self.base_prefix);
        self.focus_stack.clear();
        self.selected = 0;
        self.manager_selected = true;

        let state_dir = ea::ea_state_dir(ea_id, &self.omar_dir);
        std::fs::create_dir_all(&state_dir).ok();
        Ok(())
    }

    /// Cycle to the next registered EA
    pub fn cycle_next_ea(&mut self) {
        self.registered_eas = ea::load_registry(&self.omar_dir);
        if self.registered_eas.len() <= 1 {
            return;
        }
        let current_idx = match self
            .registered_eas
            .iter()
            .position(|ea| ea.id == self.active_ea)
        {
            Some(idx) => idx,
            None => return,
        };
        let next_idx = (current_idx + 1) % self.registered_eas.len();
        let next_ea = self.registered_eas[next_idx].id;
        if let Err(e) = self.switch_ea(next_ea) {
            self.set_status(format!("Error switching EA: {}", e));
        }
    }

    /// Cycle to the previous registered EA
    pub fn cycle_previous_ea(&mut self) {
        self.registered_eas = ea::load_registry(&self.omar_dir);
        if self.registered_eas.len() <= 1 {
            return;
        }
        let current_idx = match self
            .registered_eas
            .iter()
            .position(|ea| ea.id == self.active_ea)
        {
            Some(idx) => idx,
            None => return,
        };
        let prev_idx = if current_idx == 0 {
            self.registered_eas.len() - 1
        } else {
            current_idx - 1
        };
        let prev_ea = self.registered_eas[prev_idx].id;
        if let Err(e) = self.switch_ea(prev_ea) {
            self.set_status(format!("Error switching EA: {}", e));
        }
    }

    /// Create a new EA and add it to the registry
    pub fn create_ea(&mut self, name: String, desc: Option<String>) -> Result<EaId> {
        match ea::register_ea(&self.omar_dir, &name, desc.as_deref()) {
            Ok(ea_id) => {
                self.registered_eas = ea::load_registry(&self.omar_dir);
                self.set_status(format!("Created EA {}: {}", ea_id, name));
                Ok(ea_id)
            }
            Err(e) => {
                self.set_status(format!("Error creating EA: {}", e));
                Err(e)
            }
        }
    }

    /// Delete the specified EA: kill all its tmux sessions, remove state, unregister.
    /// Blocked if it is the last EA. Switches to the lowest remaining EA when active.
    pub fn delete_ea(&mut self, ea_id: EaId) -> Result<()> {
        if self.registered_eas.len() <= 1 {
            self.set_status("Cannot delete the only EA");
            self.pending_confirm = None;
            return Ok(());
        }

        // Kill all tmux sessions for this EA
        let ea_prefix = ea::ea_prefix(ea_id, &self.base_prefix);
        let manager_session = ea::ea_manager_session(ea_id, &self.base_prefix);
        let ea_client = TmuxClient::new(&ea_prefix);

        let sessions = ea_client.list_sessions()?;
        let all_sessions = ea_client.list_all_sessions()?;
        let mut sessions_to_delete = Vec::new();
        for session in sessions {
            if session.name != manager_session {
                sessions_to_delete.push(session.name);
            }
        }
        let manager_present = all_sessions
            .iter()
            .any(|session| session.name == manager_session);
        if manager_present {
            sessions_to_delete.push(manager_session.clone());
        }

        for session_name in &sessions_to_delete {
            if all_sessions
                .iter()
                .any(|session| session.name == *session_name && session.attached)
            {
                self.set_status("Cannot delete attached session");
                self.pending_confirm = None;
                return Ok(());
            }
        }

        // Transactional delete: cleanup must succeed before registry mutation.
        for session_name in &sessions_to_delete {
            ea_client.kill_session(session_name)?;
        }

        // Remove state directory
        let state_dir = ea::ea_state_dir(ea_id, &self.omar_dir);
        if state_dir.exists() {
            std::fs::remove_dir_all(&state_dir)?;
        }

        let notes_path = memory::manager_notes_path(&self.omar_dir, ea_id);
        if notes_path.exists() {
            std::fs::remove_file(&notes_path)?;
        }
        crate::manager::remove_omar_antigravity_mcp_config(ea_id)?;

        let events_cancelled = self.scheduler.cancel_by_ea(ea_id);

        // Unregister EA
        ea::unregister_ea(&self.omar_dir, ea_id)?;
        self.registered_eas = ea::load_registry(&self.omar_dir);

        // Switch to lowest remaining EA if we just deleted the active EA
        if self.active_ea == ea_id {
            let next_id = self
                .registered_eas
                .iter()
                .map(|e| e.id)
                .filter(|id| *id != ea_id)
                .min()
                .unwrap_or(0);
            self.switch_ea(next_id)?;
        }

        self.set_status(format!(
            "Deleted EA {} ({} workers, {} events)",
            ea_id,
            sessions_to_delete.len(),
            events_cancelled
        ));
        self.pending_confirm = None;
        Ok(())
    }
}

/// Build the chain-of-command tree from current agents and parent mappings.
///
/// Tree structure (recursive, arbitrary depth):
///   EA (root, depth 0)
///   ├── Agent with children (depth 1)
///   │   └── Sub-agent (depth 2)
///   │       └── Sub-sub-agent (depth 3) ...
///   └── Orphan agents with no parent (depth 1, under EA)
pub fn build_tree(
    agents: &[AgentInfo],
    manager: Option<&AgentInfo>,
    agent_parents: &HashMap<String, String>,
    session_prefix: &str,
    manager_session: &str,
) -> Vec<CommandTreeNode> {
    let mut nodes = Vec::new();

    // Root: EA
    let ea_health = manager.map(|m| m.health).unwrap_or(HealthState::Idle);
    nodes.push(CommandTreeNode {
        name: "Executive Assistant".to_string(),
        session_name: manager_session.to_string(),
        health: ea_health,
        depth: 0,
        is_last_sibling: true,
        ancestor_is_last: vec![],
        is_unresolved: false,
    });

    // Build a children map: parent_session -> vec of child agents
    let mut children_map: HashMap<String, Vec<&AgentInfo>> = HashMap::new();
    let mut orphans: Vec<&AgentInfo> = Vec::new();

    for agent in agents {
        if let Some(parent_session) = agent_parents.get(&agent.session.name) {
            // Check that the parent actually exists (either as agent or EA)
            let parent_exists = *parent_session == manager_session
                || agents.iter().any(|a| a.session.name == *parent_session);
            if parent_exists {
                children_map
                    .entry(parent_session.clone())
                    .or_default()
                    .push(agent);
            } else {
                orphans.push(agent);
            }
        } else {
            orphans.push(agent);
        }
    }

    // Recursively add nodes starting from EA's children
    let ea_children = children_map.get(manager_session);
    let total_root_children = ea_children.map(|c| c.len()).unwrap_or(0) + orphans.len();

    #[allow(clippy::too_many_arguments)]
    fn add_children(
        nodes: &mut Vec<CommandTreeNode>,
        children_map: &HashMap<String, Vec<&AgentInfo>>,
        parent_session: &str,
        depth: usize,
        ancestor_is_last: &[bool],
        session_prefix: &str,
        total_siblings: usize,
        start_idx: usize,
    ) {
        if let Some(children) = children_map.get(parent_session) {
            for (idx, child) in children.iter().enumerate() {
                let sibling_idx = start_idx + idx;
                let is_last = sibling_idx == total_siblings - 1;
                let short = child
                    .session
                    .name
                    .strip_prefix(session_prefix)
                    .unwrap_or(&child.session.name);

                nodes.push(CommandTreeNode {
                    name: short.to_string(),
                    session_name: child.session.name.clone(),
                    health: child.health,
                    depth,
                    is_last_sibling: is_last,
                    ancestor_is_last: ancestor_is_last.to_vec(),
                    is_unresolved: child.is_unresolved,
                });

                // Recurse into this child's children
                let grandchildren_count = children_map
                    .get(&child.session.name)
                    .map(|c| c.len())
                    .unwrap_or(0);
                if grandchildren_count > 0 {
                    let mut next_ancestors = ancestor_is_last.to_vec();
                    next_ancestors.push(is_last);
                    add_children(
                        nodes,
                        children_map,
                        &child.session.name,
                        depth + 1,
                        &next_ancestors,
                        session_prefix,
                        grandchildren_count,
                        0,
                    );
                }
            }
        }
    }

    // Add EA's direct children
    let ea_direct_count = ea_children.map(|c| c.len()).unwrap_or(0);
    add_children(
        &mut nodes,
        &children_map,
        manager_session,
        1,
        &[true], // EA is always last at depth 0
        session_prefix,
        total_root_children,
        0,
    );

    // Add orphan agents (no parent or dead parent) under EA
    if !orphans.is_empty() {
        for (orphan_idx, orphan) in orphans.iter().enumerate() {
            let short = orphan
                .session
                .name
                .strip_prefix(session_prefix)
                .unwrap_or(&orphan.session.name);
            let sibling_idx = ea_direct_count + orphan_idx;
            nodes.push(CommandTreeNode {
                name: short.to_string(),
                session_name: orphan.session.name.clone(),
                health: orphan.health,
                depth: 1,
                is_last_sibling: sibling_idx == total_root_children - 1,
                ancestor_is_last: vec![true],
                is_unresolved: orphan.is_unresolved,
            });

            // Orphans can also have children
            let child_count = children_map
                .get(&orphan.session.name)
                .map(|c| c.len())
                .unwrap_or(0);
            if child_count > 0 {
                add_children(
                    &mut nodes,
                    &children_map,
                    &orphan.session.name,
                    2,
                    &[true, sibling_idx == total_root_children - 1],
                    session_prefix,
                    child_count,
                    0,
                );
            }
        }
    }

    nodes
}

/// Map a selected session name to the canonical receiver name used in
/// scheduled events. The EA manager's session name collapses to "ea"; worker
/// sessions drop the EA-scoped prefix.
///
/// Kept as a free fn so it can be unit-tested without standing up a full
/// `App` (which needs a Config + tmux client).
pub(crate) fn popup_receiver_name_for(
    selected_session_name: &str,
    manager_session_name: &str,
    prefix: &str,
) -> String {
    if selected_session_name == manager_session_name {
        "ea".to_string()
    } else {
        selected_session_name
            .strip_prefix(prefix)
            .unwrap_or(selected_session_name)
            .to_string()
    }
}

/// Position of the agent named `name` within `focus_child_indices`, so the
/// dashboard can land its cursor on a newly spawned worker that has just
/// been re-refreshed into the current view.
fn focus_view_index(
    agents: &[AgentInfo],
    focus_child_indices: &[usize],
    name: &str,
) -> Option<usize> {
    focus_child_indices
        .iter()
        .position(|&i| agents.get(i).is_some_and(|a| a.session.name == name))
}

/// Build the next unique agent name for `prefix`, skipping names already in
/// `existing`. Must use the EA-scoped prefix: `refresh()` filters
/// `self.agents` by that prefix, so a name built from the base prefix is
/// invisible to the dashboard.
fn next_agent_name(prefix: &str, existing: &std::collections::HashSet<&str>) -> String {
    for i in 1..1000 {
        let candidate = format!("{}{}", prefix, i);
        if !existing.contains(candidate.as_str()) {
            return candidate;
        }
    }
    format!(
        "{}{}",
        prefix,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    )
}

fn parse_ea_id_segment(raw: &str) -> Option<EaId> {
    if raw.is_empty() {
        return None;
    }

    let normalized = raw.trim_start_matches('0');
    let normalized = if normalized.is_empty() {
        "0"
    } else {
        normalized
    };
    normalized.parse::<EaId>().ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseSessionOwner {
    Manager(EaId),
    Worker(EaId),
    Unresolved,
}

fn parse_ea_session_owner(session_name: &str, base_prefix: &str) -> Option<ParseSessionOwner> {
    let rest = session_name.strip_prefix(base_prefix)?;

    // Manager sessions are explicit: "omar-agent-ea-<id>".
    if let Some(raw_id) = rest.strip_prefix("ea-") {
        if let Some(id) = parse_ea_id_segment(raw_id) {
            return Some(ParseSessionOwner::Manager(id));
        }
        return Some(ParseSessionOwner::Unresolved);
    }

    // Worker sessions are usually "<id>-<name>". Parse `<id>` when present.
    let raw_id = match rest.split_once('-') {
        Some((raw_id, _)) => raw_id,
        None => return Some(ParseSessionOwner::Unresolved),
    };
    if let Some(id) = parse_ea_id_segment(raw_id) {
        return Some(ParseSessionOwner::Worker(id));
    }

    Some(ParseSessionOwner::Unresolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentConfig, DashboardConfig, HealthConfig, MetricsConfig};
    use crate::projects;
    use crate::scheduler;
    use crate::tmux::{HealthState, Session, TmuxClient};

    /// Manager session name used in tests (EA 0 with "omar-agent-" prefix)
    const TEST_MANAGER: &str = "omar-agent-ea-0";

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_env_lock()
    }

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

    struct TmuxServerEnvGuard {
        previous: Option<std::ffi::OsString>,
    }

    impl TmuxServerEnvGuard {
        fn set(server: &str) -> Self {
            let previous = std::env::var_os("OMAR_TMUX_SERVER");
            std::env::set_var("OMAR_TMUX_SERVER", server);
            Self { previous }
        }
    }

    impl Drop for TmuxServerEnvGuard {
        fn drop(&mut self) {
            match self.previous.as_ref() {
                Some(value) => std::env::set_var("OMAR_TMUX_SERVER", value),
                None => std::env::remove_var("OMAR_TMUX_SERVER"),
            }
        }
    }

    fn make_agent(name: &str, health: HealthState) -> AgentInfo {
        AgentInfo {
            session: Session {
                name: name.to_string(),
                activity: 0,
                attached: false,
                pane_pid: 0,
            },
            health,
            is_unresolved: false,
        }
    }

    #[test]
    fn parse_ea_id_segment_accepts_leading_zero_ids() {
        assert_eq!(parse_ea_id_segment("01"), Some(1));
        assert_eq!(parse_ea_id_segment("001"), Some(1));
    }

    #[test]
    fn parse_ea_id_segment_accepts_canonical_ids() {
        assert_eq!(parse_ea_id_segment("0"), Some(0));
        assert_eq!(parse_ea_id_segment("42"), Some(42));
    }

    #[test]
    fn parse_ea_id_segment_rejects_empty_segment() {
        assert_eq!(parse_ea_id_segment(""), None);
    }

    #[test]
    fn parse_ea_session_owner_classifies_omarprefix_variants() {
        assert_eq!(
            parse_ea_session_owner("omar-agent-rest-api", "omar-agent-"),
            Some(ParseSessionOwner::Unresolved)
        );
        assert_eq!(
            parse_ea_session_owner("omar-agent-ea", "omar-agent-"),
            Some(ParseSessionOwner::Unresolved)
        );
        assert_eq!(
            parse_ea_session_owner("omar-agent-ea", "omar-agent"),
            Some(ParseSessionOwner::Unresolved)
        );
        assert_eq!(
            parse_ea_session_owner("omar-agent-01-worker", "omar-agent-"),
            Some(ParseSessionOwner::Worker(1))
        );
        assert_eq!(
            parse_ea_session_owner("omar-agent-1-ea", "omar-agent-"),
            Some(ParseSessionOwner::Worker(1))
        );
        assert_eq!(
            parse_ea_session_owner("omar-agent-0-ea", "omar-agent-"),
            Some(ParseSessionOwner::Worker(0))
        );
        assert_eq!(
            parse_ea_session_owner("omar-agent-ea-foo", "omar-agent-"),
            Some(ParseSessionOwner::Unresolved)
        );
        assert_eq!(
            parse_ea_session_owner("omar-agent-01", "omar-agent"),
            Some(ParseSessionOwner::Unresolved)
        );
        assert_eq!(
            parse_ea_session_owner("external-session", "omar-agent-"),
            None
        );
    }

    #[test]
    fn manager_startup_attempts_single_prompted_variant() {
        let mut config = test_config_with_prefix(format!("omar-test-{}-", uuid::Uuid::new_v4()));
        config.agent.default_command =
            "codex --no-alt-screen --dangerously-bypass-approvals-and-sandbox".to_string();
        let scheduler = Arc::new(Scheduler::new());
        let mut app = App::new(&config, TickerBuffer::new(), scheduler);
        app.default_command = config.agent.default_command.clone();

        let attempts = app.manager_startup_attempts();
        assert_eq!(attempts.len(), 1);
        assert_eq!(
            attempts[0],
            (
                "codex --no-alt-screen --dangerously-bypass-approvals-and-sandbox".to_string(),
                true
            )
        );
    }

    #[test]
    fn manager_startup_attempts_is_single_attempt_for_all_backends() {
        let config = test_config_with_prefix(format!("omar-test-{}-", uuid::Uuid::new_v4()));
        let scheduler = Arc::new(Scheduler::new());
        let mut app = App::new(&config, TickerBuffer::new(), scheduler);

        let mut commands: Vec<String> = ["claude", "codex", "cursor", "opencode", "agy"]
            .iter()
            .map(|name| crate::config::resolve_backend(name).unwrap())
            .collect();
        commands.push("custom --no-flags".to_string());

        for cmd in &commands {
            app.default_command = cmd.clone();
            let attempts = app.manager_startup_attempts();
            assert_eq!(attempts.len(), 1);
            assert_eq!(&attempts[0].0, cmd);
            assert!(attempts[0].1);
        }
    }

    #[test]
    fn dashboard_launch_handoff_updates_runtime_command_and_workdir() {
        let _guard = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _home = HomeEnvGuard::set(dir.path());
        let config = test_config_with_prefix(format!("omar-test-{}-", uuid::Uuid::new_v4()));
        let scheduler = Arc::new(Scheduler::new());
        let mut app = App::new(&config, TickerBuffer::new(), scheduler);
        let handoff = ea::DashboardLaunchHandoff {
            active_ea: 0,
            default_command: crate::config::resolve_backend("claude").unwrap(),
            default_workdir: "/tmp/omar-launch".to_string(),
            restart_manager: false,
        };
        ea::save_dashboard_launch_handoff(&app.omar_dir, &handoff).unwrap();

        app.apply_dashboard_launch_handoff().unwrap();

        assert_eq!(app.default_command(), handoff.default_command);
        assert_eq!(app.default_workdir, handoff.default_workdir);
        assert_eq!(app.config.agent.default_command, handoff.default_command);
        assert_eq!(app.config.agent.default_workdir, handoff.default_workdir);
        assert!(ea::take_dashboard_launch_handoff(&app.omar_dir).is_none());
    }

    #[test]
    fn dashboard_launch_handoff_preserves_manager_when_backend_unchanged() {
        // Regression guard for the no-op case: `omar -a claude` typed while
        // the EA is already running under claude must NOT kill the manager
        // tmux session, since that throws away the backend's in-memory
        // conversation for no behavioral change.
        if !std::process::Command::new("tmux")
            .arg("-V")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
        {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let _guard = env_lock();
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = HomeEnvGuard::set(dir.path());
        let tmux_server = format!("omar-handoff-noop-{}", uuid::Uuid::new_v4());
        let _tmux = TmuxServerEnvGuard::set(&tmux_server);
        let prefix = format!("omar-test-{}-", uuid::Uuid::new_v4());
        let config = test_config_with_prefix(prefix.clone());
        let mut app = App::new(&config, TickerBuffer::new(), Arc::new(Scheduler::new()));

        let claude = crate::config::resolve_backend("claude").unwrap();
        app.default_command = claude.clone();
        app.config.agent.default_command = claude.clone();

        let manager_session = ea::ea_manager_session(0, &prefix);
        let spawned = std::process::Command::new("tmux")
            .args([
                "-L",
                &tmux_server,
                "new-session",
                "-d",
                "-s",
                &manager_session,
                "sleep 600",
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !spawned {
            eprintln!("Skipping test: failed to create tmux session");
            return;
        }
        assert!(app.client.has_session(&manager_session).unwrap());

        let handoff = ea::DashboardLaunchHandoff {
            active_ea: 0,
            default_command: claude.clone(),
            default_workdir: "/tmp/omar-noop".to_string(),
            restart_manager: true,
        };
        ea::save_dashboard_launch_handoff(&app.omar_dir, &handoff).unwrap();

        app.apply_dashboard_launch_handoff().unwrap();

        assert!(
            app.client.has_session(&manager_session).unwrap(),
            "no-op handoff (same backend) must not kill the manager session"
        );

        let _ = std::process::Command::new("tmux")
            .args(["-L", &tmux_server, "kill-session", "-t", &manager_session])
            .status();
    }

    #[test]
    fn dashboard_launch_handoff_kills_manager_when_backend_changes() {
        // Positive case: a real backend swap (claude -> opencode) must kill
        // the manager session so it respawns under the new backend.
        if !std::process::Command::new("tmux")
            .arg("-V")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
        {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let _guard = env_lock();
        let dir = tempfile::tempdir().expect("temp dir");
        let _home = HomeEnvGuard::set(dir.path());
        let tmux_server = format!("omar-handoff-swap-{}", uuid::Uuid::new_v4());
        let _tmux = TmuxServerEnvGuard::set(&tmux_server);
        let prefix = format!("omar-test-{}-", uuid::Uuid::new_v4());
        let config = test_config_with_prefix(prefix.clone());
        let mut app = App::new(&config, TickerBuffer::new(), Arc::new(Scheduler::new()));

        let claude = crate::config::resolve_backend("claude").unwrap();
        let opencode = crate::config::resolve_backend("opencode").unwrap();
        assert_ne!(
            claude, opencode,
            "backends must resolve to distinct commands"
        );
        app.default_command = claude.clone();
        app.config.agent.default_command = claude.clone();

        let manager_session = ea::ea_manager_session(0, &prefix);
        let spawned = std::process::Command::new("tmux")
            .args([
                "-L",
                &tmux_server,
                "new-session",
                "-d",
                "-s",
                &manager_session,
                "sleep 600",
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !spawned {
            eprintln!("Skipping test: failed to create tmux session");
            return;
        }
        assert!(app.client.has_session(&manager_session).unwrap());

        let handoff = ea::DashboardLaunchHandoff {
            active_ea: 0,
            default_command: opencode.clone(),
            default_workdir: "/tmp/omar-swap".to_string(),
            restart_manager: true,
        };
        ea::save_dashboard_launch_handoff(&app.omar_dir, &handoff).unwrap();

        app.apply_dashboard_launch_handoff().unwrap();

        assert!(
            !app.client.has_session(&manager_session).unwrap(),
            "real backend swap must kill the manager session so it respawns under the new backend"
        );
        assert!(
            app.manager.is_none(),
            "App::manager must be cleared after kill"
        );
    }

    #[test]
    fn persistent_warning_set_if_clear_or_same_preserves_unrelated_warning() {
        let config = test_config_with_prefix(format!("omar-test-{}-", uuid::Uuid::new_v4()));
        let scheduler = Arc::new(Scheduler::new());
        let mut app = App::new(&config, TickerBuffer::new(), scheduler);

        app.set_persistent_warning_if_clear_or_same("tmux setup missing");
        assert_eq!(
            app.persistent_warning.as_deref(),
            Some("tmux setup missing")
        );
        assert_eq!(app.status_message.as_deref(), Some("tmux setup missing"));

        app.set_persistent_warning("auth failure");
        app.set_persistent_warning_if_clear_or_same("tmux setup missing");
        assert_eq!(app.persistent_warning.as_deref(), Some("auth failure"));
        assert_eq!(app.status_message.as_deref(), Some("auth failure"));
    }

    fn test_config_with_prefix(session_prefix: String) -> Config {
        Config {
            dashboard: DashboardConfig {
                session_prefix,
                ..DashboardConfig::default()
            },
            health: HealthConfig::default(),
            agent: AgentConfig {
                default_command: "true".to_string(),
                default_workdir: ".".to_string(),
            },
            metrics: MetricsConfig::default(),
            slack_bridge: crate::config::SlackBridgeConfig::default(),
        }
    }

    fn tmux_session_attached(server: &str, session_name: &str) -> bool {
        let output = std::process::Command::new("tmux")
            .args([
                "-L",
                server,
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
            .find_map(|(name, attached)| {
                if name == session_name {
                    Some(attached == "1")
                } else {
                    None
                }
            })
            .unwrap_or(false)
    }

    #[test]
    fn refresh_replaces_dead_manager_session() {
        let _env_lock = env_lock();
        if !std::process::Command::new("tmux")
            .arg("-V")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
        {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let dir = tempfile::tempdir().expect("temp dir");
        let _home = HomeEnvGuard::set(dir.path());
        let tmux_server = format!("omar-app-dead-manager-{}", uuid::Uuid::new_v4());
        let _tmux = TmuxServerEnvGuard::set(&tmux_server);
        let mut config = test_config_with_prefix(format!("omar-test-{}-", uuid::Uuid::new_v4()));
        config.agent.default_command = "sleep 600".to_string();

        let manager_session = ea::ea_manager_session(0, &config.dashboard.session_prefix);
        let client = TmuxClient::new(&config.dashboard.session_prefix);

        let ok = std::process::Command::new("tmux")
            .args([
                "-L",
                &tmux_server,
                "new-session",
                "-d",
                "-s",
                &manager_session,
                "sh",
            ])
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("Skipping test: failed to create tmux session");
            return;
        }

        let set_ok = std::process::Command::new("tmux")
            .args([
                "-L",
                &tmux_server,
                "set-option",
                "-t",
                &manager_session,
                "remain-on-exit",
                "on",
            ])
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !set_ok {
            let _ = std::process::Command::new("tmux")
                .args(["-L", &tmux_server, "kill-session", "-t", &manager_session])
                .status();
            eprintln!("Skipping test: failed to enable remain-on-exit");
            return;
        }

        let target = format!("{manager_session}:0.0");
        let _ = std::process::Command::new("tmux")
            .args([
                "-L",
                &tmux_server,
                "send-keys",
                "-t",
                &target,
                "exit 7",
                "C-m",
            ])
            .status();
        for _ in 0..20 {
            if !client
                .session_has_live_pane(&manager_session)
                .unwrap_or(true)
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if client
            .session_has_live_pane(&manager_session)
            .unwrap_or(true)
        {
            let _ = std::process::Command::new("tmux")
                .args(["-L", &tmux_server, "kill-session", "-t", &manager_session])
                .status();
            eprintln!("Skipping test: failed to create dead manager pane");
            return;
        }
        assert!(client.has_session(&manager_session).unwrap());

        let mut app = App::new(&config, TickerBuffer::new(), Arc::new(Scheduler::new()));
        app.refresh().expect("refresh should replace dead manager");

        assert!(client.has_session(&manager_session).unwrap());
        assert!(
            client.session_has_live_pane(&manager_session).unwrap(),
            "refresh should replace dead manager session with a live pane"
        );

        let _ = std::process::Command::new("tmux")
            .args(["-L", &tmux_server, "kill-session", "-t", &manager_session])
            .status();
    }

    #[test]
    fn app_startup_resumes_existing_registry_and_preserves_state() {
        let dir = tempfile::tempdir().unwrap();
        let omar_dir = dir.path().join(".omar");
        let ea0_dir = ea::ea_state_dir(0, &omar_dir);
        let ea1_dir = ea::ea_state_dir(1, &omar_dir);
        std::fs::create_dir_all(ea0_dir.join("status")).unwrap();
        std::fs::create_dir_all(ea1_dir.join("status")).unwrap();

        let registry = vec![
            EaInfo {
                id: 0,
                name: "Primary".to_string(),
                description: Some("existing default".to_string()),
                created_at: 10,
            },
            EaInfo {
                id: 1,
                name: "Research".to_string(),
                description: Some("existing second ea".to_string()),
                created_at: 11,
            },
        ];
        std::fs::write(
            omar_dir.join("eas.json"),
            serde_json::to_string_pretty(&registry).unwrap(),
        )
        .unwrap();
        std::fs::write(omar_dir.join("active_ea"), "1").unwrap();
        std::fs::write(omar_dir.join("ea_next_id"), "42").unwrap();
        std::fs::write(
            memory::manager_notes_path(&omar_dir, 1),
            "persisted manager note",
        )
        .unwrap();

        std::fs::write(
            ea1_dir.join("worker_tasks.json"),
            r#"{"omar-test-worker":"resume this task"}"#,
        )
        .unwrap();
        std::fs::write(ea1_dir.join("tasks.md"), "7. Persisted project\n").unwrap();
        std::fs::write(
            ea1_dir.join("task_registry.json"),
            r#"{"legacy":"registry entry"}"#,
        )
        .unwrap();
        std::fs::write(
            ea1_dir.join("agent_parents.json"),
            r#"{"omar-test-worker":"omar-test-parent"}"#,
        )
        .unwrap();
        std::fs::write(ea1_dir.join("memory.md"), "manager resume context").unwrap();
        std::fs::write(ea1_dir.join("status/omar-test-worker.md"), "stale status").unwrap();

        let scheduler = Arc::new(Scheduler::with_store(scheduler::events_store_path(
            &omar_dir,
        )));
        scheduler.insert(ScheduledEvent {
            id: "persisted-event".to_string(),
            sender: "ea".to_string(),
            receiver: "worker".to_string(),
            timestamp: 123,
            payload: "wake later".to_string(),
            created_at: 100,
            recurring_ns: Some(5_000_000_000),
            ea_id: 1,
        });

        let config = test_config_with_prefix(format!("omar-test-resume-{}-", uuid::Uuid::new_v4()));
        let app = App::new_with_omar_dir(
            &config,
            TickerBuffer::new(),
            scheduler.clone(),
            omar_dir.clone(),
        );

        assert_eq!(app.active_ea, 1);
        assert_eq!(
            app.registered_eas
                .iter()
                .map(|ea| (ea.id, ea.name.as_str()))
                .collect::<Vec<_>>(),
            vec![(0, "Primary"), (1, "Research")]
        );
        assert_eq!(
            std::fs::read_to_string(omar_dir.join("active_ea")).unwrap(),
            "1"
        );
        assert_eq!(
            std::fs::read_to_string(omar_dir.join("ea_next_id")).unwrap(),
            "42"
        );
        assert_eq!(
            std::fs::read_to_string(memory::manager_notes_path(&omar_dir, 1)).unwrap(),
            "persisted manager note"
        );
        assert_eq!(
            std::fs::read_to_string(ea1_dir.join("worker_tasks.json")).unwrap(),
            r#"{"omar-test-worker":"resume this task"}"#
        );
        assert_eq!(
            std::fs::read_to_string(ea1_dir.join("tasks.md")).unwrap(),
            "7. Persisted project\n"
        );
        assert_eq!(
            std::fs::read_to_string(ea1_dir.join("task_registry.json")).unwrap(),
            r#"{"legacy":"registry entry"}"#
        );
        assert_eq!(
            std::fs::read_to_string(ea1_dir.join("agent_parents.json")).unwrap(),
            r#"{"omar-test-worker":"omar-test-parent"}"#
        );
        assert_eq!(
            std::fs::read_to_string(ea1_dir.join("memory.md")).unwrap(),
            "manager resume context"
        );
        assert_eq!(
            std::fs::read_to_string(ea1_dir.join("status/omar-test-worker.md")).unwrap(),
            "stale status"
        );
        assert_eq!(scheduler.list_by_ea(1).len(), 1);
        assert!(
            std::fs::read_to_string(scheduler::events_store_path(&omar_dir))
                .unwrap()
                .contains("wake later")
        );
    }

    #[test]
    fn app_startup_from_home_loads_config_and_preserves_runtime_state() {
        let _guard = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _home = HomeEnvGuard::set(dir.path());

        let omar_dir = dir.path().join(".omar");
        let ea0_dir = ea::ea_state_dir(0, &omar_dir);
        std::fs::create_dir_all(ea0_dir.join("status")).unwrap();
        std::fs::write(
            omar_dir.join("config.toml"),
            r#"
[dashboard]
session_prefix = "omar-agent-"

[agent]
default_command = "codex --no-alt-screen --dangerously-bypass-approvals-and-sandbox"
default_workdir = "."
"#,
        )
        .unwrap();
        ea::ensure_default_ea(&omar_dir).unwrap();
        projects::save_projects_to(
            &ea0_dir,
            &[projects::Project {
                id: 3,
                name: "Keep project".to_string(),
            }],
        )
        .unwrap();
        std::fs::write(
            ea0_dir.join("worker_tasks.json"),
            r#"{"omar-agent-0-worker":"keep task"}"#,
        )
        .unwrap();
        std::fs::write(
            ea0_dir.join("agent_parents.json"),
            r#"{"omar-agent-0-worker":"omar-agent-ea"}"#,
        )
        .unwrap();
        std::fs::write(ea0_dir.join("memory.md"), "keep memory").unwrap();
        std::fs::write(ea0_dir.join("task_registry.json"), r#"{"task":"registry"}"#).unwrap();
        std::fs::write(ea0_dir.join("status/omar-agent-0-worker.md"), "keep status").unwrap();

        let scheduler = Arc::new(Scheduler::with_store(scheduler::events_store_path(
            &omar_dir,
        )));
        scheduler.insert(ScheduledEvent {
            id: "home-event".to_string(),
            sender: "ea".to_string(),
            receiver: "worker".to_string(),
            timestamp: 456,
            payload: "home wake".to_string(),
            created_at: 123,
            recurring_ns: None,
            ea_id: 0,
        });

        let config = Config::load(None).unwrap();
        let app = App::new(&config, TickerBuffer::new(), scheduler.clone());

        assert_eq!(
            app.default_command(),
            "codex --no-alt-screen --dangerously-bypass-approvals-and-sandbox"
        );
        assert_eq!(
            std::fs::read_to_string(ea0_dir.join("tasks.md")).unwrap(),
            "3. Keep project\n"
        );
        assert_eq!(
            std::fs::read_to_string(ea0_dir.join("worker_tasks.json")).unwrap(),
            r#"{"omar-agent-0-worker":"keep task"}"#
        );
        assert_eq!(
            std::fs::read_to_string(ea0_dir.join("agent_parents.json")).unwrap(),
            r#"{"omar-agent-0-worker":"omar-agent-ea"}"#
        );
        assert_eq!(
            std::fs::read_to_string(ea0_dir.join("memory.md")).unwrap(),
            "keep memory"
        );
        assert_eq!(
            std::fs::read_to_string(ea0_dir.join("task_registry.json")).unwrap(),
            r#"{"task":"registry"}"#
        );
        assert_eq!(
            std::fs::read_to_string(ea0_dir.join("status/omar-agent-0-worker.md")).unwrap(),
            "keep status"
        );
        assert_eq!(scheduler.list_by_ea(0).len(), 1);
        assert!(
            std::fs::read_to_string(scheduler::events_store_path(&omar_dir))
                .unwrap()
                .contains("home wake")
        );
    }

    // ── build_tree tests ──

    #[test]
    fn test_build_tree_ea_only() {
        let agents = vec![];
        let ea = make_agent(TEST_MANAGER, HealthState::Running);
        let parents = HashMap::new();
        let tree = build_tree(&agents, Some(&ea), &parents, "omar-agent-", TEST_MANAGER);

        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].name, "Executive Assistant");
        assert_eq!(tree[0].depth, 0);
    }

    #[test]
    fn test_build_tree_with_parent_and_children() {
        let agents = vec![
            make_agent("omar-agent-rest-api", HealthState::Running),
            make_agent("omar-agent-api", HealthState::Running),
            make_agent("omar-agent-auth", HealthState::Idle),
        ];
        let ea = make_agent(TEST_MANAGER, HealthState::Running);
        let mut parents = HashMap::new();
        parents.insert("omar-agent-rest-api".to_string(), TEST_MANAGER.to_string());
        parents.insert(
            "omar-agent-api".to_string(),
            "omar-agent-rest-api".to_string(),
        );
        parents.insert(
            "omar-agent-auth".to_string(),
            "omar-agent-rest-api".to_string(),
        );

        let tree = build_tree(&agents, Some(&ea), &parents, "omar-agent-", TEST_MANAGER);

        // EA + rest-api + 2 children = 4 nodes
        assert_eq!(tree.len(), 4);
        assert_eq!(tree[0].name, "Executive Assistant");
        assert_eq!(tree[0].depth, 0);
        assert_eq!(tree[1].name, "rest-api");
        assert_eq!(tree[1].depth, 1);
        assert!(tree[1].is_last_sibling);
        assert_eq!(tree[2].name, "api");
        assert_eq!(tree[2].depth, 2);
        assert!(!tree[2].is_last_sibling);
        assert_eq!(tree[3].name, "auth");
        assert_eq!(tree[3].depth, 2);
        assert!(tree[3].is_last_sibling);
    }

    #[test]
    fn test_build_tree_orphan_agents() {
        let agents = vec![
            make_agent("omar-agent-debug", HealthState::Running),
            make_agent("omar-agent-test", HealthState::Idle),
        ];
        let ea = make_agent(TEST_MANAGER, HealthState::Running);
        let parents = HashMap::new();

        let tree = build_tree(&agents, Some(&ea), &parents, "omar-agent-", TEST_MANAGER);

        // EA + 2 orphans directly under EA = 3 nodes
        assert_eq!(tree.len(), 3);
        assert_eq!(tree[1].name, "debug");
        assert_eq!(tree[1].depth, 1);
        assert!(!tree[1].is_last_sibling);
        assert_eq!(tree[2].name, "test");
        assert_eq!(tree[2].depth, 1);
        assert!(tree[2].is_last_sibling);
    }

    #[test]
    fn test_build_tree_deep_hierarchy() {
        // EA -> agent-a -> agent-b -> agent-c (3 levels deep)
        let agents = vec![
            make_agent("omar-agent-a", HealthState::Running),
            make_agent("omar-agent-b", HealthState::Running),
            make_agent("omar-agent-c", HealthState::Idle),
        ];
        let ea = make_agent(TEST_MANAGER, HealthState::Running);
        let mut parents = HashMap::new();
        parents.insert("omar-agent-a".to_string(), TEST_MANAGER.to_string());
        parents.insert("omar-agent-b".to_string(), "omar-agent-a".to_string());
        parents.insert("omar-agent-c".to_string(), "omar-agent-b".to_string());

        let tree = build_tree(&agents, Some(&ea), &parents, "omar-agent-", TEST_MANAGER);

        assert_eq!(tree.len(), 4);
        assert_eq!(tree[0].name, "Executive Assistant");
        assert_eq!(tree[0].depth, 0);
        assert_eq!(tree[1].name, "a");
        assert_eq!(tree[1].depth, 1);
        assert_eq!(tree[2].name, "b");
        assert_eq!(tree[2].depth, 2);
        assert_eq!(tree[3].name, "c");
        assert_eq!(tree[3].depth, 3);
    }

    #[test]
    fn test_build_tree_stale_parent_treated_as_orphan() {
        let agents = vec![make_agent("omar-agent-worker1", HealthState::Running)];
        let ea = make_agent(TEST_MANAGER, HealthState::Running);
        let mut parents = HashMap::new();
        parents.insert(
            "omar-agent-worker1".to_string(),
            "omar-agent-gone".to_string(),
        );

        let tree = build_tree(&agents, Some(&ea), &parents, "omar-agent-", TEST_MANAGER);

        // worker1 should be orphan under EA since parent doesn't exist
        assert_eq!(tree.len(), 2);
        assert_eq!(tree[1].name, "worker1");
        assert_eq!(tree[1].depth, 1);
    }

    // ── focus navigation tests ──

    #[test]
    fn test_focus_children_root_shows_direct_ea_children_and_orphans() {
        // EA -> api (with children: api-worker, auth), orphan
        let agents = [
            make_agent("omar-agent-api", HealthState::Running),
            make_agent("omar-agent-api-worker", HealthState::Running),
            make_agent("omar-agent-auth", HealthState::Idle),
            make_agent("omar-agent-orphan", HealthState::Idle),
        ];
        let mut parents = HashMap::new();
        parents.insert("omar-agent-api".to_string(), TEST_MANAGER.to_string());
        parents.insert(
            "omar-agent-api-worker".to_string(),
            "omar-agent-api".to_string(),
        );
        parents.insert("omar-agent-auth".to_string(), "omar-agent-api".to_string());

        // Compute focus children at root using the new logic
        let mut indices = Vec::new();
        for (i, agent) in agents.iter().enumerate() {
            let parent = parents.get(&agent.session.name);
            match parent {
                Some(p) if *p == TEST_MANAGER => indices.push(i),
                Some(p) if agents.iter().any(|a| a.session.name == *p) => {}
                _ => indices.push(i),
            }
        }

        // Root should show: api (EA child) + orphan (no parent)
        assert_eq!(indices.len(), 2);
        assert_eq!(agents[indices[0]].session.name, "omar-agent-api");
        assert_eq!(agents[indices[1]].session.name, "omar-agent-orphan");

        // Drill into api: should show its children
        let focus_parent = "omar-agent-api";
        let mut child_indices = Vec::new();
        for (i, agent) in agents.iter().enumerate() {
            if let Some(parent) = parents.get(&agent.session.name) {
                if *parent == focus_parent {
                    child_indices.push(i);
                }
            }
        }
        assert_eq!(child_indices.len(), 2);
        assert_eq!(
            agents[child_indices[0]].session.name,
            "omar-agent-api-worker"
        );
        assert_eq!(agents[child_indices[1]].session.name, "omar-agent-auth");
    }

    #[test]
    fn next_agent_name_uses_ea_scoped_prefix_so_refresh_keeps_the_agent() {
        // Simulates EA 0: refresh() filters agents by the EA-scoped prefix
        // "omar-agent-0-". A name built from the base prefix ("omar-agent-")
        // would be stripped out, which is exactly the bug this guards against.
        let ea_prefix = "omar-agent-0-";
        let existing: std::collections::HashSet<&str> = std::collections::HashSet::new();

        let name = next_agent_name(ea_prefix, &existing);

        assert!(
            name.starts_with(ea_prefix),
            "generated name {:?} must start with EA-scoped prefix {:?}",
            name,
            ea_prefix
        );
        assert_eq!(name, "omar-agent-0-1");
    }

    #[test]
    fn next_agent_name_skips_names_already_in_the_ea() {
        let ea_prefix = "omar-agent-0-";
        let mut existing: std::collections::HashSet<&str> = std::collections::HashSet::new();
        existing.insert("omar-agent-0-1");
        existing.insert("omar-agent-0-2");

        assert_eq!(next_agent_name(ea_prefix, &existing), "omar-agent-0-3");
    }

    #[test]
    fn spawn_bookkeeping_persists_parent_under_focus_and_selects_in_view() {
        use crate::memory;

        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path();

        let focus_parent = "omar-agent-api";
        let new_child = "omar-agent-api-helper";

        memory::save_agent_parent_in(state_dir, focus_parent, TEST_MANAGER);
        memory::save_agent_parent_in(state_dir, new_child, focus_parent);

        let parents = memory::load_agent_parents_from(state_dir);
        assert_eq!(
            parents.get(new_child).map(String::as_str),
            Some(focus_parent),
            "the post-spawn mapping must survive a refresh-style reload"
        );

        let agents = vec![
            make_agent(focus_parent, HealthState::Running),
            make_agent(new_child, HealthState::Running),
        ];

        let focus_child_indices: Vec<usize> = agents
            .iter()
            .enumerate()
            .filter_map(|(i, a)| {
                parents
                    .get(&a.session.name)
                    .filter(|p| *p == focus_parent)
                    .map(|_| i)
            })
            .collect();

        let pos = focus_view_index(&agents, &focus_child_indices, new_child)
            .expect("spawned child must resolve to a focus-view index");
        assert_eq!(agents[focus_child_indices[pos]].session.name, new_child);

        assert!(
            focus_view_index(&agents, &focus_child_indices, "omar-agent-ghost").is_none(),
            "unrelated sessions must not appear in the current view"
        );
    }

    #[test]
    fn drilling_into_childless_agent_yields_empty_view_with_cursor_on_parent() {
        // `api` is the EA's only live child and has no sub-agents of its own.
        // Drilling into it must still succeed so the user can spawn the first
        // grandchild with 'n'; the cursor parks on the focus parent because
        // there is no child at index 0 to land on.
        let agents = [make_agent("omar-agent-api", HealthState::Running)];
        let mut parents = HashMap::new();
        parents.insert("omar-agent-api".to_string(), TEST_MANAGER.to_string());

        let focus_parent = "omar-agent-api";
        let focus_child_indices: Vec<usize> = agents
            .iter()
            .enumerate()
            .filter(|(_, a)| {
                parents
                    .get(&a.session.name)
                    .is_some_and(|p| p == focus_parent)
            })
            .map(|(i, _)| i)
            .collect();

        assert!(focus_child_indices.is_empty());
        // The drill_down body sets `manager_selected = focus_child_indices.is_empty()`
        // after recomputing indices — this mirrors that contract.
        let manager_selected_after_drill = focus_child_indices.is_empty();
        assert!(manager_selected_after_drill);
    }

    #[test]
    fn test_child_count() {
        let parents: HashMap<String, String> = [
            ("omar-agent-api", "omar-agent-rest-api"),
            ("omar-agent-auth", "omar-agent-rest-api"),
            ("omar-agent-ui", "omar-agent-frontend"),
        ]
        .iter()
        .map(|(c, p)| (c.to_string(), p.to_string()))
        .collect();

        let count = parents
            .values()
            .filter(|p| *p == "omar-agent-rest-api")
            .count();
        assert_eq!(count, 2);

        let count = parents
            .values()
            .filter(|p| *p == "omar-agent-frontend")
            .count();
        assert_eq!(count, 1);

        let count = parents.values().filter(|p| *p == "omar-agent-api").count();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_breadcrumb_building() {
        let session_prefix = "omar-agent-";

        // At root: just ["EA"]
        let focus_stack: Vec<String> = vec![];
        let focus_parent = TEST_MANAGER.to_string();
        let mut crumbs = vec!["EA".to_string()];
        for session in &focus_stack {
            if *session == TEST_MANAGER {
                continue;
            }
            let short = session.strip_prefix(session_prefix).unwrap_or(session);
            crumbs.push(short.to_string());
        }
        if focus_parent != TEST_MANAGER {
            let short = focus_parent
                .strip_prefix(session_prefix)
                .unwrap_or(&focus_parent);
            crumbs.push(short.to_string());
        }
        assert_eq!(crumbs, vec!["EA"]);

        // Drilled into agent: ["EA", "rest-api"]
        let focus_stack = vec![TEST_MANAGER.to_string()];
        let focus_parent = "omar-agent-rest-api".to_string();
        let mut crumbs = vec!["EA".to_string()];
        for session in &focus_stack {
            if *session == TEST_MANAGER {
                continue;
            }
            let short = session.strip_prefix(session_prefix).unwrap_or(session);
            crumbs.push(short.to_string());
        }
        if focus_parent != TEST_MANAGER {
            let short = focus_parent
                .strip_prefix(session_prefix)
                .unwrap_or(&focus_parent);
            crumbs.push(short.to_string());
        }
        assert_eq!(crumbs, vec!["EA", "rest-api"]);
    }

    // ── attached session tests ──

    #[test]
    fn test_attached_agent_included_in_tree() {
        // An attached agent should still appear in the command tree
        let mut attached = make_agent("omar-agent-api", HealthState::Running);
        attached.session.attached = true;
        let agents = vec![attached];
        let ea = make_agent("omar-agent-ea", HealthState::Running);
        let mut parents = HashMap::new();
        parents.insert("omar-agent-api".to_string(), TEST_MANAGER.to_string());

        let tree = build_tree(&agents, Some(&ea), &parents, "omar-agent-", TEST_MANAGER);

        assert_eq!(tree.len(), 2);
        assert_eq!(tree[1].name, "api");
        assert_eq!(tree[1].depth, 1);
    }

    #[test]
    fn test_delete_ea_refuses_attached_session() {
        let _env_lock = env_lock();
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

        let dir = tempfile::tempdir().expect("temp dir");
        let _home = HomeEnvGuard::set(dir.path());
        let tmux_server = format!("omar-app-delete-{}", uuid::Uuid::new_v4());
        let _tmux = TmuxServerEnvGuard::set(&tmux_server);
        let config = test_config_with_prefix(format!("omar-test-{}-", uuid::Uuid::new_v4()));

        let state_dir = dir.path().join(".omar");
        ea::ensure_default_ea(&state_dir).expect("ensure default ea");
        let attached_ea = ea::register_ea(&state_dir, "attached-ea", None).expect("register ea");

        let mut app = App::new(&config, TickerBuffer::new(), Arc::new(Scheduler::new()));

        let worker_session = format!(
            "{}app-attached",
            ea::ea_prefix(attached_ea, &config.dashboard.session_prefix)
        );
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

        let mut attach = std::process::Command::new("script")
            .args([
                "-qec",
                &format!(
                    "tmux -L {} attach-session -t {}",
                    tmux_server, worker_session
                ),
                "/dev/null",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("attach harness");

        for _ in 0..10 {
            if tmux_session_attached(&tmux_server, &worker_session) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if !tmux_session_attached(&tmux_server, &worker_session) {
            attach.kill().expect("stop attach harness");
            attach.wait().expect("wait attach harness");
            let _ = std::process::Command::new("tmux")
                .args(["-L", &tmux_server, "kill-session", "-t", &worker_session])
                .status();
            eprintln!("Skipping test: failed to attach worker session");
            return;
        }
        let _ = app.delete_ea(attached_ea);

        assert_eq!(
            app.status_message.as_deref(),
            Some("Cannot delete attached session")
        );

        assert!(
            std::process::Command::new("tmux")
                .args(["-L", &tmux_server, "has-session", "-t", &worker_session])
                .status()
                .map(|o| o.success())
                .unwrap_or(false),
            "attached session should remain after refused delete"
        );

        attach.kill().expect("stop attach harness");
        attach.wait().expect("wait attach harness");

        std::thread::sleep(std::time::Duration::from_millis(200));
        app.delete_ea(attached_ea).expect("delete after detach");
        assert!(
            !std::process::Command::new("tmux")
                .args(["-L", &tmux_server, "has-session", "-t", &worker_session])
                .status()
                .map(|o| o.success())
                .unwrap_or(false),
            "session should be removed after delete"
        );
    }

    #[test]
    fn test_delete_ea_refuses_attached_manager_session() {
        let _env_lock = env_lock();
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

        let dir = tempfile::tempdir().expect("temp dir");
        let _home = HomeEnvGuard::set(dir.path());
        let tmux_server = format!("omar-app-manager-delete-{}", uuid::Uuid::new_v4());
        let _tmux = TmuxServerEnvGuard::set(&tmux_server);
        let config = test_config_with_prefix(format!("omar-test-{}-", uuid::Uuid::new_v4()));

        let state_root = dir.path().join(".omar");
        let mut app = App::new(&config, TickerBuffer::new(), Arc::new(Scheduler::new()));

        let attached_ea = app
            .create_ea("attached-manager-ea".to_string(), None)
            .expect("register ea");
        let state_dir = ea::ea_state_dir(attached_ea, &state_root);
        std::fs::create_dir_all(&state_dir).expect("create ea state");
        std::fs::write(state_dir.join("sentinel.txt"), b"keep").expect("state sentinel");

        let manager_session = ea::ea_manager_session(attached_ea, &config.dashboard.session_prefix);
        let worker_session = format!(
            "{}app-attached-manager",
            ea::ea_prefix(attached_ea, &config.dashboard.session_prefix)
        );

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
            .expect("create attached manager session");

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
            .expect("attach manager");

        for _ in 0..10 {
            if tmux_session_attached(&tmux_server, &manager_session) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if !tmux_session_attached(&tmux_server, &manager_session) {
            attachment.kill().expect("stop attach harness");
            attachment.wait().expect("wait attach harness");
            let _ = std::process::Command::new("tmux")
                .args(["-L", &tmux_server, "kill-session", "-t", &worker_session])
                .status();
            let _ = std::process::Command::new("tmux")
                .args(["-L", &tmux_server, "kill-session", "-t", &manager_session])
                .status();
            eprintln!("Skipping test: failed to attach manager session");
            return;
        }
        let _ = app.delete_ea(attached_ea);

        assert_eq!(
            app.status_message.as_deref(),
            Some("Cannot delete attached session")
        );
        assert!(
            std::process::Command::new("tmux")
                .args(["-L", &tmux_server, "has-session", "-t", &worker_session])
                .status()
                .map(|o| o.success())
                .unwrap_or(false),
            "worker session should remain when manager is attached"
        );
        assert!(
            std::process::Command::new("tmux")
                .args(["-L", &tmux_server, "has-session", "-t", &manager_session])
                .status()
                .map(|o| o.success())
                .unwrap_or(false),
            "manager session should remain when manager is attached"
        );
        assert!(
            state_dir.join("sentinel.txt").exists(),
            "delete_ea should not remove state on attached manager"
        );

        attachment.kill().expect("stop attach harness");
        attachment.wait().expect("wait attach harness");

        app.delete_ea(attached_ea).expect("delete after detach");
        assert!(
            !std::process::Command::new("tmux")
                .args(["-L", &tmux_server, "has-session", "-t", &worker_session])
                .status()
                .map(|o| o.success())
                .unwrap_or(false),
            "worker session should be removed after detached delete"
        );
        assert!(
            !std::process::Command::new("tmux")
                .args(["-L", &tmux_server, "has-session", "-t", &manager_session])
                .status()
                .map(|o| o.success())
                .unwrap_or(false),
            "manager session should be removed after detached delete"
        );
        assert!(
            !state_dir.exists(),
            "EA state should be removed after successful delete"
        );
    }

    // ── popup_receiver_name_for tests ──
    //
    // The tmux pane the dashboard opens must be addressed by the same
    // identifier the scheduler sees in event `receiver` fields, so that the
    // per-pane 30s defer actually matches. Workers use their short name; the
    // EA pane normalizes to "ea" (not the session name `omar-agent-ea-N`,
    // which never appears in event payloads).

    #[test]
    fn popup_receiver_name_for_worker_strips_prefix() {
        let name =
            popup_receiver_name_for("omar-agent-0-worker1", "omar-agent-ea-0", "omar-agent-0-");
        assert_eq!(name, "worker1");
    }

    #[test]
    fn popup_receiver_name_for_ea_manager_normalizes_to_ea() {
        let name = popup_receiver_name_for("omar-agent-ea-0", "omar-agent-ea-0", "omar-agent-0-");
        assert_eq!(name, "ea");
    }

    #[test]
    fn popup_receiver_name_for_ea_manager_ea1_also_normalizes_to_ea() {
        // EA 1: prefix is "omar-agent-1-", manager is "omar-agent-ea-1".
        let name = popup_receiver_name_for("omar-agent-ea-1", "omar-agent-ea-1", "omar-agent-1-");
        assert_eq!(name, "ea");
    }

    #[test]
    fn popup_receiver_name_for_unprefixed_falls_back_to_full_name() {
        // Safety net: a session that doesn't carry the active EA prefix at
        // all shouldn't silently become an empty string.
        let name = popup_receiver_name_for("legacy-session", "omar-agent-ea-0", "omar-agent-0-");
        assert_eq!(name, "legacy-session");
    }
}
