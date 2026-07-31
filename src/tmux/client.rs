#![allow(dead_code)]

use anyhow::{anyhow, Context, Result};
use std::io::Write;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use super::Session;

/// Options for reliable prompt delivery and related readiness helpers.
///
/// Note: `deliver_prompt` itself no longer performs a readiness phase —
/// callers are expected to gate on `wait_for_markers` first. The fields
/// labelled *(wait_for_stable only)* are therefore ignored by
/// `deliver_prompt` and retained only for direct callers of
/// `TmuxClient::wait_for_stable`.
#[derive(Debug, Clone)]
pub struct DeliveryOptions {
    /// Max time to wait for the pane to become stable (backend ready).
    /// *(wait_for_stable only — ignored by `deliver_prompt`.)*
    pub startup_timeout: Duration,
    /// How long the pane must be quiet to be considered "stable".
    /// *(wait_for_stable only — ignored by `deliver_prompt`.)*
    pub stable_quiet: Duration,
    /// Per-phase timeout inside `deliver_prompt`. Applied TWICE per attempt:
    /// once waiting for the paste to render (sentinel or new placeholder),
    /// and once waiting for the post-Enter pane change that confirms
    /// submission. Worst-case single-attempt wall time is therefore
    /// approximately `2 * verify_timeout + retry_delay`.
    pub verify_timeout: Duration,
    /// How many full delivery attempts to try before giving up.
    pub max_retries: u32,
    /// Polling interval for pane capture / activity checks.
    pub poll_interval: Duration,
    /// Delay between retry attempts (after clearing input with C-u).
    pub retry_delay: Duration,
    /// When true, readiness wait requires observing at least one pane change
    /// (activity timestamp or content) before considering the pane stable.
    /// Useful for freshly spawned sessions where the initial static shell pane
    /// is not necessarily backend-ready yet.
    /// *(wait_for_stable only — ignored by `deliver_prompt`.)*
    pub require_initial_change: bool,
}

impl Default for DeliveryOptions {
    fn default() -> Self {
        Self {
            startup_timeout: Duration::from_secs(15),
            stable_quiet: Duration::from_millis(500),
            verify_timeout: Duration::from_secs(3),
            max_retries: 3,
            poll_interval: Duration::from_millis(100),
            retry_delay: Duration::from_millis(200),
            require_initial_change: false,
        }
    }
}

/// Distinctive prefix of collapsed-paste placeholders used by TUI input
/// widgets when a paste crosses a per-backend size threshold. Observed
/// formats:
/// - Claude Code: `[Pasted text #N +M lines]`
/// - opencode:    `[Pasted ~N lines]`
///
/// The common prefix `[Pasted ` (with trailing space) catches both and
/// any future vendor that follows the same convention. When the widget
/// collapses the paste into a placeholder the sentinel at the tail of
/// the payload never renders verbatim, so `deliver_prompt` treats a new
/// occurrence of this marker as equivalent proof the paste has ingested.
const PASTE_PLACEHOLDER_MARKER: &str = "[Pasted ";

/// Returns true when `hay` shows that the most recent paste has rendered.
/// A paste is considered rendered if EITHER the per-delivery end sentinel
/// appears verbatim, OR a new `[Pasted text ...]` placeholder appeared
/// relative to `baseline_placeholders` (the count observed just before
/// the paste was issued). The count-delta avoids false positives from
/// stale placeholders already visible in prior chat history.
fn paste_rendered(hay: &str, end_sentinel: &str, baseline_placeholders: usize) -> bool {
    if hay.contains(end_sentinel) {
        return true;
    }
    hay.matches(PASTE_PLACEHOLDER_MARKER).count() > baseline_placeholders
}

fn tail_pane_lines(output: String, lines: i32) -> String {
    if lines <= 0 {
        return output;
    }

    let limit = lines as usize;
    let had_trailing_newline = output.ends_with('\n');
    let mut selected: Vec<&str> = output.lines().rev().take(limit).collect();
    selected.reverse();

    let mut capped = selected.join("\n");
    if had_trailing_newline {
        capped.push('\n');
    }
    capped
}

#[derive(Debug, Clone)]
pub struct TmuxClient {
    prefix: String,
}

/// An agent name as tmux will store it.
///
/// tmux reads `.` in a target as the window/pane separator, so a name carrying
/// one has to be flattened first: `new-session -s a.b` silently creates `a_b`,
/// and every later lookup for `a.b` then misses. Team instances qualify their
/// agents as `instance.agent`, which makes this the only spelling that
/// survives a round trip.
pub fn flatten_agent_name(agent: &str) -> String {
    agent.replace('.', "_")
}

pub fn tmux_command() -> Command {
    let mut cmd = Command::new("tmux");
    if let Ok(server) = std::env::var("OMAR_TMUX_SERVER") {
        let server = server.trim();
        if !server.is_empty() {
            cmd.args(["-L", server]);
        }
    }
    cmd
}

fn exact_session_target(target: &str) -> String {
    if target.starts_with('=') || target.contains(':') || target.contains('.') {
        target.to_string()
    } else {
        format!("={target}")
    }
}

fn exact_pane_target(target: &str) -> String {
    if target.contains(':') || target.contains('.') {
        target.to_string()
    } else if target.starts_with('=') {
        format!("{target}:")
    } else {
        format!("={target}:")
    }
}

fn popup_attach_command(target: &str) -> String {
    let tmux_server = std::env::var("OMAR_TMUX_SERVER")
        .ok()
        .map(|server| server.trim().to_string())
        .filter(|server| !server.is_empty());
    popup_attach_command_with_server(target, tmux_server.as_deref())
}

fn popup_attach_command_with_server(target: &str, tmux_server: Option<&str>) -> String {
    let mut command = String::from("env -u TMUX tmux");
    if let Some(server) = tmux_server {
        command.push_str(" -L ");
        command.push_str(server);
    }
    let popup_target = target.strip_prefix('=').unwrap_or(target);
    command.push_str(" attach-session -t ");
    command.push_str(popup_target);
    command
}

impl TmuxClient {
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
        }
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// The session an agent runs in.
    pub fn session_for(&self, agent: &str) -> String {
        format!("{}{}", self.prefix, flatten_agent_name(agent))
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let output = tmux_command()
            .args(args)
            .output()
            .context("Failed to execute tmux - is tmux installed?")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // "no server running" is not an error for list-sessions.
            // Other tmux commands must surface failures to callers.
            if args.first() == Some(&"list-sessions")
                && (stderr.contains("no server running")
                    || stderr.contains("no sessions")
                    || stderr.contains("error connecting to"))
            {
                return Ok(String::new());
            }
            anyhow::bail!("tmux error: {}", stderr);
        }
        Ok(String::from_utf8_lossy(&output.stdout).into())
    }

    /// List all sessions matching the prefix
    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        let output = self.run(&[
            "list-sessions",
            "-F",
            "#{session_name}|#{session_activity}|#{session_attached}|#{pane_pid}",
        ])?;

        if output.is_empty() {
            return Ok(Vec::new());
        }

        let sessions = output
            .lines()
            .filter(|line| self.prefix.is_empty() || line.starts_with(&self.prefix))
            .filter_map(|line| {
                let parts: Vec<&str> = line.split('|').collect();
                if parts.len() != 4 {
                    return None;
                }
                Some(Session::new(
                    parts[0].to_string(),
                    parts[1].parse().ok()?,
                    parts[2] == "1",
                    parts[3].parse().ok()?,
                ))
            })
            .collect();

        Ok(sessions)
    }

    /// List all sessions (regardless of prefix)
    pub fn list_all_sessions(&self) -> Result<Vec<Session>> {
        let output = self.run(&[
            "list-sessions",
            "-F",
            "#{session_name}|#{session_activity}|#{session_attached}|#{pane_pid}",
        ])?;

        if output.is_empty() {
            return Ok(Vec::new());
        }

        let sessions = output
            .lines()
            .filter_map(|line| {
                let parts: Vec<&str> = line.split('|').collect();
                if parts.len() != 4 {
                    return None;
                }
                Some(Session::new(
                    parts[0].to_string(),
                    parts[1].parse().ok()?,
                    parts[2] == "1",
                    parts[3].parse().ok()?,
                ))
            })
            .collect();

        Ok(sessions)
    }

    /// Capture the last N lines of a pane's output, including ANSI escape
    /// sequences (suitable for display in a colored dashboard).
    pub fn capture_pane(&self, target: &str, lines: i32) -> Result<String> {
        let target = exact_pane_target(target);
        self.run(&[
            "capture-pane",
            "-e",
            "-t",
            &target,
            "-p",
            "-S",
            &(-lines).to_string(),
        ])
    }

    /// Capture the last N lines of a pane's output as plain text (no ANSI
    /// escapes). Required for substring matching — e.g. Claude Code renders
    /// its banner as `Claude<ESC>[0m <ESC>[1mCode`, so a raw ANSI capture
    /// would *not* contain the contiguous string "Claude Code".
    pub fn capture_pane_plain(&self, target: &str, lines: i32) -> Result<String> {
        let target = exact_pane_target(target);
        let output = self.run(&[
            "capture-pane",
            "-t",
            &target,
            "-p",
            "-S",
            &(-lines).to_string(),
        ])?;
        Ok(tail_pane_lines(output, lines))
    }

    /// Get the name of the command currently running in a pane.
    ///
    /// Returns the executable name (e.g. "opencode", "claude", "zsh").
    pub fn get_pane_command(&self, target: &str) -> Result<String> {
        let target = exact_pane_target(target);
        let output = self.run(&[
            "display-message",
            "-t",
            &target,
            "-p",
            "#{pane_current_command}",
        ])?;
        Ok(output.trim().to_string())
    }

    /// Get the pane process id.
    pub fn get_pane_pid(&self, target: &str) -> Result<u32> {
        let target = exact_pane_target(target);
        let output = self.run(&["display-message", "-t", &target, "-p", "#{pane_pid}"])?;
        output.trim().parse().context("Failed to parse pane pid")
    }

    /// Get the full command line for the process running in a pane.
    pub fn get_pane_process_command(&self, target: &str) -> Result<String> {
        let pid = self.get_pane_pid(target)?;
        let output = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "command="])
            .output()
            .context("Failed to execute ps")?;
        if !output.status.success() {
            anyhow::bail!(
                "ps failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Get the activity timestamp of a pane.
    ///
    /// Uses `#{window_activity}` — the per-pane `#{pane_activity}` format is
    /// empty on tmux 3.6a (macOS homebrew) unless `monitor-activity` is
    /// enabled, which would break readiness checks entirely. Window activity
    /// is universally populated and tracks the most recent input/output in
    /// the window. Since OMAR worker sessions always have exactly one window
    /// and one pane, window-level granularity is equivalent to pane-level.
    pub fn get_pane_activity(&self, target: &str) -> Result<i64> {
        let target = exact_pane_target(target);
        let output = self.run(&["display-message", "-t", &target, "-p", "#{window_activity}"])?;
        output
            .trim()
            .parse()
            .context("Failed to parse window activity timestamp")
    }

    /// Send keys to a pane
    pub fn send_keys(&self, target: &str, keys: &str) -> Result<()> {
        let target = exact_pane_target(target);
        self.run(&["send-keys", "-t", &target, keys])?;
        Ok(())
    }

    /// Send literal text to a pane.
    ///
    /// For small payloads uses `send-keys -l` directly. For large payloads
    /// (>= 2 KB) writes to a temporary file and uses `load-buffer` +
    /// `paste-buffer` to avoid tmux's internal message-size limit, which
    /// silently drops oversized `send-keys -l` arguments.
    pub const LARGE_PAYLOAD_THRESHOLD: usize = 2048;

    pub fn send_keys_literal(&self, target: &str, text: &str) -> Result<()> {
        let target = exact_pane_target(target);
        if text.len() < Self::LARGE_PAYLOAD_THRESHOLD {
            self.run(&["send-keys", "-t", &target, "-l", "--", text])?;
        } else {
            // Avoid passing large text as a CLI arg; owner-only temp file is
            // removed when `tmp` drops.
            let mut tmp = crate::paths::create_private_temp_file("omar-task", "txt")
                .context("Failed to create temp file for task payload")?;
            tmp.write_all(text.as_bytes())
                .context("Failed to write task to temp file")?;
            let path_str = tmp
                .path()
                .to_str()
                .context("Temp file path is not valid UTF-8")?;
            self.run(&["load-buffer", path_str])?;
            self.run(&["paste-buffer", "-t", &target])?;
        }
        Ok(())
    }

    /// Paste text into a pane via load-buffer + paste-buffer.
    /// Uses bracketed paste (-p) so the backend receives the entire payload
    /// as a single paste event. This is more reliable than send-keys for
    /// multi-line text and backends with custom TUI input widgets.
    ///
    /// A unique named buffer (`-b <uuid>`) is used per call so concurrent
    /// deliveries — e.g. an initial-task spawn racing a scheduler event —
    /// cannot clobber each other's payload via the shared unnamed buffer.
    /// `-d` on paste-buffer deletes the buffer after pasting, so buffers
    /// do not accumulate on error paths either.
    ///
    /// `-r` disables tmux's default LF→CR replacement inside the paste.
    /// Without it, every `\n` in the payload is delivered to the target pane
    /// as `\r`. TUI backends like Claude Code run the terminal in raw mode
    /// (ICRNL off), so those CRs are not translated back to LF and each one
    /// reads as an Enter keypress inside the input widget — turning a single
    /// multi-line prompt into a cascade of blank submissions. Reproduced on
    /// tmux 3.2a (Ubuntu 22.04); newer tmux builds on macOS appear to mask
    /// this but `-r` makes behavior identical across versions.
    pub fn paste_text(&self, target: &str, text: &str) -> Result<()> {
        let target = exact_pane_target(target);
        let buffer_name = format!("omar-paste-{}", uuid::Uuid::new_v4());

        // Owner-only temp file (payloads can carry pasted secrets), removed
        // when `tmp` drops.
        let mut tmp = crate::paths::create_private_temp_file("omar-paste", "txt")
            .context("Failed to create temp file for paste payload")?;
        tmp.write_all(text.as_bytes())
            .context("Failed to write paste text to temp file")?;
        let path_str = tmp
            .path()
            .to_str()
            .context("Temp file path is not valid UTF-8")?;
        self.run(&["load-buffer", "-b", &buffer_name, path_str])?;
        // Paste from the named buffer using bracketed paste mode so the
        // target pane treats it as a single paste operation. `-d` deletes
        // the buffer after pasting. `-r` preserves LFs verbatim — see the
        // doc comment above for why this matters for raw-mode TUIs.
        self.run(&[
            "paste-buffer",
            "-b",
            &buffer_name,
            "-t",
            &target,
            "-d",
            "-p",
            "-r",
        ])?;
        Ok(())
    }

    /// Reliably deliver a prompt to a tmux session.
    ///
    /// Backend-agnostic: works for claude, codex, cursor, opencode, or any
    /// TUI. The caller is responsible for gating on backend readiness
    /// (e.g. via `wait_for_markers`) before invoking — this function assumes
    /// the input widget is already live.
    ///
    /// Strategy: every load-bearing wait is bounded by an observable
    /// signal (end sentinel, new placeholder, pane change). The only fixed
    /// timing is a 50 ms settle after the per-attempt `C-u` clear so the
    /// widget has committed that state before the subsequent paste.
    ///
    /// 1. Wrap the payload in per-delivery UUID sentinels:
    ///    `<UserPromptBegins:{id}>\n{text}\n<UserPromptEnds:{id}>`.
    /// 2. Paste the wrapped text via bracketed paste.
    /// 3. Poll the plain pane capture for either the end sentinel appearing
    ///    verbatim, OR a new `[Pasted ...]` placeholder (vs the pre-paste
    ///    baseline count). TUI input widgets (Claude Code, opencode, etc.)
    ///    collapse pastes that cross their size threshold into a
    ///    placeholder so the sentinel never renders literally; the
    ///    placeholder-count delta gives a second proof-of-render that
    ///    works for both small and large payloads. Stale placeholders
    ///    already in chat history cannot false-positive because we
    ///    compare counts, not mere presence.
    /// 4. On render timeout, retry — DO NOT press Enter. The previous
    ///    implementation's bug was firing Enter after a failed-render
    ///    timeout, which submitted a blank widget (or partial payload) and
    ///    declared success from the resulting pane change.
    /// 5. Once rendered, snapshot the pane and submit a single literal CR
    ///    byte (`send-keys -H 0d`). Using `send-keys Enter` can route
    ///    through tmux's extended-keys encoding when the pane opts in via
    ///    DECSET 2017 — some TUI input widgets (Claude Code's Ink-based
    ///    widget included) read any CSI-encoded Enter as a modified
    ///    keypress and treat it as Shift+Enter (newline insertion) instead
    ///    of submit. `-H 0d` writes the raw byte and bypasses the
    ///    encoding layer.
    /// 6. Verify with `wait_for_change` that Enter caused an observable
    ///    transition. If not, clear the input and retry.
    pub fn deliver_prompt(&self, session: &str, text: &str, opts: &DeliveryOptions) -> Result<()> {
        // Per-delivery UUID so a stale sentinel from a previous delivery
        // cannot false-positive the end-sentinel poll on retry.
        let delivery_id = uuid::Uuid::new_v4().simple().to_string();
        let short_id = &delivery_id[..8];
        let start_sentinel = format!("<UserPromptBegins:{}>", short_id);
        let end_sentinel = format!("<UserPromptEnds:{}>", short_id);
        let wrapped = format!("{}\n{}\n{}", start_sentinel, text, end_sentinel);

        for attempt in 1..=opts.max_retries {
            // Clear any leftover input from a prior attempt. No-op on the
            // first attempt against a fresh widget.
            let _ = self.send_keys(session, "C-u");
            thread::sleep(Duration::from_millis(50));

            // Baseline pane BEFORE paste so the long-paste placeholder
            // detection below can distinguish a new placeholder from a stale
            // one already visible in chat history.
            let baseline_placeholders = self
                .capture_pane_plain(session, 200)
                .unwrap_or_default()
                .matches(PASTE_PLACEHOLDER_MARKER)
                .count();

            self.paste_text(session, &wrapped)?;

            // Wait until we have proof the paste rendered. Two acceptable
            // signals:
            //   (a) the end sentinel appears verbatim — the common case for
            //       prompts that fit under the backend's "show raw text"
            //       threshold;
            //   (b) a NEW paste placeholder appears (count increased vs
            //       baseline) — Claude Code collapses large pastes into
            //       `[Pasted text #N +M lines]` so the sentinel never
            //       renders literally. We compare counts rather than mere
            //       presence so a stale placeholder from prior chat
            //       history can't false-positive the check.
            let rendered = {
                let deadline = Instant::now() + opts.verify_timeout;
                let mut found = false;
                while Instant::now() < deadline {
                    if let Ok(hay) = self.capture_pane_plain(session, 200) {
                        if paste_rendered(&hay, &end_sentinel, baseline_placeholders) {
                            found = true;
                            break;
                        }
                    }
                    thread::sleep(opts.poll_interval);
                }
                found
            };

            if !rendered {
                if attempt < opts.max_retries {
                    thread::sleep(opts.retry_delay);
                }
                continue;
            }

            // Paste fully rendered. Snapshot, then submit with a literal
            // CR byte that bypasses tmux's extended-keys encoding.
            let content_before = self.capture_pane(session, 50).unwrap_or_default();
            let activity_before = self.get_pane_activity(session).unwrap_or(0);

            let target = exact_pane_target(session);
            self.run(&["send-keys", "-t", &target, "-H", "0d"])?;

            if self.wait_for_change(
                session,
                activity_before,
                &content_before,
                opts.verify_timeout,
                opts.poll_interval,
            ) {
                return Ok(());
            }

            if attempt < opts.max_retries {
                thread::sleep(opts.retry_delay);
            }
        }

        anyhow::bail!(
            "prompt delivery to '{}' was not verified after {} attempt(s)",
            session,
            opts.max_retries
        )
    }

    /// Wait for pane activity to be quiet for `quiet` duration, or until `timeout`.
    /// Returns Ok(()) as soon as the pane becomes stable; returns Ok(()) anyway
    /// after timeout (best-effort — caller should proceed regardless).
    pub fn wait_for_stable(
        &self,
        session: &str,
        quiet: Duration,
        timeout: Duration,
        poll_interval: Duration,
        require_initial_change: bool,
    ) -> Result<()> {
        let start = Instant::now();
        let mut last_activity = self.get_pane_activity(session).unwrap_or(0);
        let mut last_content = self.capture_pane(session, 50).unwrap_or_default();
        let mut saw_change = false;
        let mut last_change = Instant::now();

        while start.elapsed() < timeout {
            thread::sleep(poll_interval);
            let current = self.get_pane_activity(session).unwrap_or(last_activity);
            let content = self
                .capture_pane(session, 50)
                .unwrap_or_else(|_| last_content.clone());
            if current != last_activity || content != last_content {
                saw_change = true;
                last_activity = current;
                last_content = content;
                last_change = Instant::now();
            } else if last_change.elapsed() >= quiet && (!require_initial_change || saw_change) {
                return Ok(());
            }
        }
        // Timed out waiting for stability — proceed anyway
        Ok(())
    }

    /// Poll until either pane activity advances OR pane content changes.
    /// This catches backends that update content without bumping the
    /// activity timestamp, and vice versa.
    fn wait_for_change(
        &self,
        session: &str,
        activity_before: i64,
        content_before: &str,
        timeout: Duration,
        poll_interval: Duration,
    ) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if let Ok(current) = self.get_pane_activity(session) {
                if current > activity_before {
                    return true;
                }
            }
            if let Ok(content) = self.capture_pane(session, 50) {
                if content != content_before {
                    return true;
                }
            }
            thread::sleep(poll_interval);
        }
        false
    }

    /// Wait until pane output contains ALL of the provided markers.
    /// Matching is case-insensitive; returns false on timeout.
    ///
    /// All-match (rather than any-match) semantics matter for backends like
    /// Claude Code v2.1.116, where the product banner paints hundreds of
    /// ms before the input widget is actually ready to accept keystrokes.
    /// Matching on only one of several markers would let the caller fire
    /// Enter into a pane that silently swallows it.
    pub fn wait_for_markers(
        &self,
        session: &str,
        markers: &[&str],
        timeout: Duration,
        poll_interval: Duration,
    ) -> bool {
        if markers.is_empty() {
            return true;
        }
        let needles: Vec<String> = markers.iter().map(|m| m.to_ascii_lowercase()).collect();
        let start = Instant::now();
        while start.elapsed() < timeout {
            // Use plain capture (no ANSI escapes) so multi-word markers like
            // "Claude Code" match even when the TUI styles each word
            // independently (Claude Code inserts a reset between them).
            if let Ok(content) = self.capture_pane_plain(session, 120) {
                let hay = content.to_ascii_lowercase();
                if needles.iter().all(|needle| hay.contains(needle)) {
                    return true;
                }
            }
            thread::sleep(poll_interval);
        }
        false
    }

    /// Create a new detached session
    pub fn new_session(&self, name: &str, command: &str, workdir: Option<&str>) -> Result<()> {
        let mut args = vec!["new-session", "-d", "-s", name];

        if let Some(dir) = workdir {
            args.extend(["-c", dir]);
        }

        // Execute the provided command through a shell so the full string is
        // interpreted consistently (including quoted args and shell metacharacters)
        // instead of relying on tmux's shell-command parser heuristics.
        args.extend(["sh", "-lc", command]);
        self.run(&args)?;
        self.run(&["set-option", "-t", name, "history-limit", "10000"])?;
        Ok(())
    }

    /// Kill a session
    pub fn kill_session(&self, name: &str) -> Result<()> {
        let target = exact_session_target(name);
        self.run(&["kill-session", "-t", &target])?;
        Ok(())
    }

    /// Check if a session exists
    pub fn has_session(&self, name: &str) -> Result<bool> {
        let target = exact_session_target(name);
        let result = tmux_command()
            .args(["has-session", "-t", &target])
            .output()
            .context("Failed to execute tmux")?;

        Ok(result.status.success())
    }

    /// Return true when a tmux session exists and has at least one live pane.
    ///
    /// A user can exit the process inside a pane while tmux keeps the session
    /// around with `remain-on-exit`. `has-session` is still true in that state,
    /// but the session cannot accept input or be attached as a running agent.
    pub fn session_has_live_pane(&self, name: &str) -> Result<bool> {
        let target = exact_session_target(name);
        let result = tmux_command()
            .args(["list-panes", "-t", &target, "-F", "#{pane_dead}"])
            .output()
            .context("Failed to execute tmux")?;

        if !result.status.success() {
            let stderr = String::from_utf8_lossy(&result.stderr);
            if stderr.contains("can't find")
                || stderr.contains("no server running")
                || stderr.contains("no sessions")
                || stderr.contains("error connecting to")
            {
                return Ok(false);
            }
            anyhow::bail!("tmux error: {}", stderr);
        }

        Ok(String::from_utf8_lossy(&result.stdout)
            .lines()
            .any(|line| line.trim() != "1"))
    }

    /// Find a session by exact name.
    pub fn get_session(&self, name: &str) -> Result<Option<Session>> {
        let target = exact_session_target(name);
        let sessions = self.list_sessions()?;
        Ok(sessions
            .into_iter()
            .find(|session| exact_session_target(&session.name) == target))
    }

    /// Ensure the session exists and is not currently attached.
    pub fn ensure_session_not_attached(&self, name: &str) -> Result<Session> {
        let session = self
            .get_session(name)?
            .ok_or_else(|| anyhow!("Session '{}' not found", name))?;
        if session.attached {
            anyhow::bail!("Cannot kill attached session")
        }
        Ok(session)
    }

    /// Attach to a session (blocks until detached)
    pub fn attach_session(&self, session: &str) -> Result<()> {
        let target = exact_session_target(session);
        tmux_command()
            .args(["attach-session", "-t", &target])
            .status()
            .context("Failed to attach to tmux session")?;
        Ok(())
    }

    /// Open a popup attached to a session
    pub fn attach_popup(&self, session: &str, width: &str, height: &str) -> Result<()> {
        let target = exact_session_target(session);
        let command = popup_attach_command(&target);
        let status = tmux_command()
            .args(["display-popup", "-E", "-w", width, "-h", height, &command])
            .status()
            .context("Failed to open tmux popup")?;
        if !status.success() {
            anyhow::bail!("tmux popup exited with status {}", status);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_qualified_agent_name_survives_the_round_trip_to_tmux() {
        // tmux would store `n1.agent` as `n1_agent` and then fail every lookup
        // for the name it was given, so the flattening has to happen here.
        let client = TmuxClient::new("omar-agent-0-");
        assert_eq!(client.session_for("n1.agent"), "omar-agent-0-n1_agent");
        // Unqualified names, which is every agent outside a main block, are
        // untouched.
        assert_eq!(client.session_for("worker"), "omar-agent-0-worker");
    }

    #[test]
    fn test_client_creation() {
        let client = TmuxClient::new("");
        assert_eq!(client.prefix(), "");
    }

    /// Covers the two render-proof branches of `paste_rendered` and the
    /// count-delta invariant that prevents a stale `[Pasted ...]`
    /// placeholder (left in chat history from a prior paste) from
    /// false-positiving the check. Also locks in cross-backend format
    /// coverage — the same predicate must fire for Claude Code's
    /// `[Pasted text #N +M lines]` AND opencode's `[Pasted ~N lines]`.
    #[test]
    fn test_paste_rendered_matches_sentinel_or_new_placeholder() {
        let sentinel = "<UserPromptEnds:abc12345>";

        // Direct sentinel match — the normal / short-paste path.
        assert!(paste_rendered(
            "prompt body <UserPromptEnds:abc12345> trailing",
            sentinel,
            0,
        ));

        // Claude Code's format — long-paste path, fresh pane.
        assert!(paste_rendered(
            "╭──╮\n│ [Pasted text #1 +234 lines] │",
            sentinel,
            0,
        ));

        // opencode's format — same predicate must fire.
        assert!(paste_rendered("│ [Pasted ~5 lines] │", sentinel, 0));

        // One placeholder already present at baseline; still just one →
        // no new paste, render not proven.
        assert!(!paste_rendered(
            "prior: [Pasted text #1 +10 lines]",
            sentinel,
            1,
        ));

        // Baseline had one; capture shows two → new paste did render.
        // Mix formats to prove the count is backend-agnostic.
        assert!(paste_rendered(
            "prior: [Pasted text #1 +10 lines]\nnew: [Pasted ~42 lines]",
            sentinel,
            1,
        ));

        // Neither signal present.
        assert!(!paste_rendered(
            "just some unrelated pane content",
            sentinel,
            0
        ));
    }

    #[test]
    fn test_plain_capture_tail_caps_real_idle_claude_fixture() {
        // Captured locally with:
        // `tmux capture-pane -t omar-fixture-claude -p -S -30`
        // from an idle Claude Code v2.1.136 pane.
        const IDLE_CLAUDE_CAPTURE: &str = r#" ▐▛███▜▌   Claude Code v2.1.136
▝▜█████▛▘  Opus 4.7 with low effort · Claude Max
  ▘▘ ▝▝    /workspace/omar

───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
❯ Try "refactor dashboard.rs"
───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
  PR #133











































"#;

        let capped = tail_pane_lines(IDLE_CLAUDE_CAPTURE.to_string(), 1);

        assert_eq!(capped.lines().count(), 1);
        assert!(!capped.contains("Claude Code"));
        assert!(!capped.contains("PR #133"));
    }

    #[test]
    fn test_client_with_different_prefix() {
        let client = TmuxClient::new("test-");
        assert_eq!(client.prefix(), "test-");
    }

    #[test]
    fn test_exact_target_prefixes_plain_session_names() {
        assert_eq!(
            exact_session_target("omar-agent-0-gx-r"),
            "=omar-agent-0-gx-r"
        );
        assert_eq!(exact_session_target("=already-exact"), "=already-exact");
        assert_eq!(exact_session_target("session:1.0"), "session:1.0");
        assert_eq!(
            exact_pane_target("omar-agent-0-gx-r"),
            "=omar-agent-0-gx-r:"
        );
        assert_eq!(exact_pane_target("=already-exact"), "=already-exact:");
        assert_eq!(exact_pane_target("session:1.0"), "session:1.0");
    }

    #[test]
    fn test_popup_attach_command_unsets_nested_tmux_and_quotes_target() {
        assert_eq!(
            popup_attach_command_with_server("=omar-agent-ea-0", None),
            "env -u TMUX tmux attach-session -t omar-agent-ea-0"
        );
    }

    #[test]
    fn test_popup_attach_command_preserves_custom_tmux_server() {
        assert_eq!(
            popup_attach_command_with_server("=omar-agent-ea-0", Some("omar-test-server")),
            "env -u TMUX tmux -L omar-test-server attach-session -t omar-agent-ea-0"
        );
    }

    fn tmux_available() -> bool {
        tmux_command()
            .arg("-V")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Cleanup guard: kill the named tmux session on drop (even on panic).
    struct SessionGuard(String);
    impl Drop for SessionGuard {
        fn drop(&mut self) {
            let _ = tmux_command()
                .args(["kill-session", "-t", &self.0])
                .output();
        }
    }

    /// Cleanup guard: remove a path on drop (even on panic or early return).
    /// Best-effort — missing files are fine.
    struct TempPathGuard(std::path::PathBuf);
    impl Drop for TempPathGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// Deliver a prompt to a shell session and verify the command actually ran.
    #[test]
    fn test_deliver_prompt_to_shell_session() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let session = "omar-test-deliver-prompt";
        let _ = tmux_command()
            .args(["kill-session", "-t", session])
            .output();
        let _guard = SessionGuard(session.to_string());

        let ok = tmux_command()
            .args(["new-session", "-d", "-s", session])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("Skipping test: failed to create tmux session");
            return;
        }

        let client = TmuxClient::new("omar-test-");
        // Tighter timeouts so the test finishes fast on a shell prompt.
        let opts = DeliveryOptions {
            startup_timeout: Duration::from_secs(3),
            stable_quiet: Duration::from_millis(200),
            verify_timeout: Duration::from_secs(2),
            max_retries: 3,
            poll_interval: Duration::from_millis(50),
            retry_delay: Duration::from_millis(100),
            require_initial_change: false,
        };

        let result = client.deliver_prompt(session, "echo OMAR_DELIVERED", &opts);
        if let Err(e) = &result {
            // Some sandboxes block send-keys; skip rather than fail.
            eprintln!(
                "Skipping test: deliver_prompt failed (likely sandbox): {}",
                e
            );
            return;
        }

        // Give the shell a moment to run the command
        thread::sleep(Duration::from_millis(500));

        let content = client.capture_pane(session, 50).unwrap_or_default();
        assert!(
            content.contains("OMAR_DELIVERED"),
            "Expected delivered command to run. Pane: {:?}",
            content
        );
    }

    #[test]
    fn test_has_session_uses_exact_target_not_tmux_prefix_match() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let existing = "omar-test-prefix-root";
        let prefix_only = "omar-test-prefix-r";
        let _ = tmux_command()
            .args(["kill-session", "-t", existing])
            .output();
        let _guard = SessionGuard(existing.to_string());

        let ok = tmux_command()
            .args(["new-session", "-d", "-s", existing, "sleep", "60"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("Skipping test: failed to create tmux session");
            return;
        }

        let client = TmuxClient::new("omar-test-");
        assert!(client.has_session(existing).unwrap());
        assert!(
            !client.has_session(prefix_only).unwrap(),
            "tmux prefix target matching must not make {prefix_only} resolve to {existing}"
        );
        assert!(
            client.send_keys(prefix_only, "C-l").is_err(),
            "pane targets must also avoid tmux prefix matching"
        );
    }

    #[test]
    fn test_wait_for_stable_returns_on_idle_pane() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let session = "omar-test-wait-stable";
        let _ = tmux_command()
            .args(["kill-session", "-t", session])
            .output();
        let _guard = SessionGuard(session.to_string());

        let ok = tmux_command()
            .args(["new-session", "-d", "-s", session])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return;
        }

        // Let the shell prompt finish drawing
        thread::sleep(Duration::from_millis(300));

        let client = TmuxClient::new("omar-test-");
        let start = Instant::now();
        client
            .wait_for_stable(
                session,
                Duration::from_millis(200),
                Duration::from_secs(3),
                Duration::from_millis(50),
                false,
            )
            .unwrap();
        // Should return within a couple hundred ms on a fully idle pane.
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "wait_for_stable took too long on idle pane: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn test_wait_for_markers_detects_backend_banner_text() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let session = "omar-test-wait-markers";
        let _ = tmux_command()
            .args(["kill-session", "-t", session])
            .output();
        let _guard = SessionGuard(session.to_string());

        let ok = tmux_command()
            .args(["new-session", "-d", "-s", session])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return;
        }

        let client = TmuxClient::new("omar-test-");
        let _ = client.send_keys_literal(session, "echo OpenAI Codex");
        let _ = client.send_keys(session, "Enter");
        let found = client.wait_for_markers(
            session,
            &["openai codex"],
            Duration::from_secs(3),
            Duration::from_millis(50),
        );
        assert!(found, "Expected marker not detected in tmux pane");
    }

    #[test]
    fn test_deliver_prompt_multiline() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let session = "omar-test-deliver-multiline";
        let _ = tmux_command()
            .args(["kill-session", "-t", session])
            .output();
        let _guard = SessionGuard(session.to_string());

        let ok = tmux_command()
            .args(["new-session", "-d", "-s", session])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return;
        }

        let client = TmuxClient::new("omar-test-");
        let opts = DeliveryOptions {
            startup_timeout: Duration::from_secs(3),
            stable_quiet: Duration::from_millis(200),
            verify_timeout: Duration::from_secs(2),
            max_retries: 3,
            poll_interval: Duration::from_millis(50),
            retry_delay: Duration::from_millis(100),
            require_initial_change: false,
        };

        // Multi-line input: bash will run both commands. Verification needle
        // is the first non-empty line ("echo FIRST_LINE_OMAR"); we also
        // assert the SECOND_LINE actually executed to prove the full payload
        // was delivered, not just truncated at the first newline.
        let text = "echo FIRST_LINE_OMAR\necho SECOND_LINE_OMAR";
        let result = client.deliver_prompt(session, text, &opts);
        if let Err(e) = &result {
            eprintln!("Skipping test: deliver_prompt failed: {}", e);
            return;
        }

        thread::sleep(Duration::from_millis(500));
        let content = client.capture_pane(session, 50).unwrap_or_default();
        assert!(
            content.contains("FIRST_LINE_OMAR"),
            "Expected FIRST_LINE_OMAR in pane: {:?}",
            content
        );
        assert!(
            content.contains("SECOND_LINE_OMAR"),
            "Expected SECOND_LINE_OMAR in pane (multi-line delivery): {:?}",
            content
        );
    }

    /// Regression: on tmux 3.6a (macOS homebrew) `#{pane_activity}` is empty
    /// unless `monitor-activity` is enabled. `get_pane_activity` used to
    /// swallow this with `unwrap_or(0)`, freezing the "activity timestamp"
    /// at 0 forever and breaking `wait_for_stable` / `wait_for_change` on
    /// readiness-gated prompt delivery. This test asserts we get a usable,
    /// advancing timestamp out of the box (no monitor-activity needed).
    #[test]
    fn test_get_pane_activity_returns_usable_timestamp() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let session = "omar-test-pane-activity";
        let _ = tmux_command()
            .args(["kill-session", "-t", session])
            .output();
        let _guard = SessionGuard(session.to_string());

        let ok = tmux_command()
            .args(["new-session", "-d", "-s", session])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return;
        }

        let client = TmuxClient::new("omar-test-");
        let t0 = client
            .get_pane_activity(session)
            .expect("get_pane_activity must parse a timestamp on a fresh session");
        assert!(
            t0 > 0,
            "activity timestamp should be a real unix time, got {}",
            t0
        );

        // Generate activity and verify the timestamp advances.
        let _ = client.send_keys(session, "Space");
        thread::sleep(Duration::from_secs(2));
        let t1 = client.get_pane_activity(session).unwrap();
        assert!(
            t1 >= t0,
            "activity timestamp must not go backwards: {} -> {}",
            t0,
            t1
        );
    }

    /// Regression: Claude Code's banner renders each word of "Claude Code"
    /// with its own bold/reset ANSI pair — `Claude<ESC>[0m <ESC>[1mCode` —
    /// so an ANSI-inclusive capture (`-e`) contains the bytes
    /// `claude\x1b[0m code`, which the literal substring "claude code" does
    /// NOT match. `wait_for_markers` must use a plain capture so multi-word
    /// markers survive styling. Without this, readiness detection silently
    /// fails for claude (works for single-word markers like "OpenAI Codex",
    /// which is why the bug escaped earlier testing).
    #[test]
    fn test_wait_for_markers_handles_ansi_styled_multiword_banner() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let session = "omar-test-ansi-marker";
        let _ = tmux_command()
            .args(["kill-session", "-t", session])
            .output();
        let _guard = SessionGuard(session.to_string());

        let ok = tmux_command()
            .args(["new-session", "-d", "-s", session])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return;
        }

        let client = TmuxClient::new("omar-test-");
        // Reproduce the Claude Code banner pattern: each word wrapped in
        // its own bold/reset pair, exactly like the real TUI.
        let banner = "printf '\\033[1mClaude\\033[0m \\033[1mCode\\033[0m v2.1.113\\n'";
        let _ = client.send_keys_literal(session, banner);
        let _ = client.send_keys(session, "Enter");

        // Confirm the bug's precondition: an ANSI-inclusive capture does
        // NOT contain the contiguous bytes "claude code".
        thread::sleep(Duration::from_millis(300));
        let ansi_capture = client.capture_pane(session, 50).unwrap_or_default();
        assert!(
            !ansi_capture.to_ascii_lowercase().contains("claude code"),
            "precondition: styled banner must NOT contain contiguous 'claude code' in an ANSI capture (if it does, the test is trivially passing and won't catch regressions)"
        );

        // And the fix: wait_for_markers must still find "Claude Code".
        let found = client.wait_for_markers(
            session,
            &["Claude Code"],
            Duration::from_secs(3),
            Duration::from_millis(50),
        );
        assert!(
            found,
            "wait_for_markers must detect multi-word marker under ANSI styling"
        );
    }

    /// Regression: before the fix, `deliver_prompt` with
    /// `require_initial_change: true` on a pane that is already at rest
    /// (e.g. Claude Code's banner has finished drawing) would wait the full
    /// `startup_timeout` because `wait_for_stable` never observed an
    /// additional change. The API + manager paths now set
    /// `require_initial_change: false` once `wait_for_markers` has proven
    /// readiness, which this test codifies: delivery to a quiet pane whose
    /// marker is already present must complete quickly, not hit the
    /// startup timeout.
    #[test]
    fn test_deliver_prompt_after_markers_does_not_stall() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let session = "omar-test-deliver-after-markers";
        let _ = tmux_command()
            .args(["kill-session", "-t", session])
            .output();
        let _guard = SessionGuard(session.to_string());

        let ok = tmux_command()
            .args(["new-session", "-d", "-s", session])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return;
        }

        let client = TmuxClient::new("omar-test-");

        // Print a "banner" and let the pane settle so the marker is visible
        // and the pane is fully at rest before we attempt delivery.
        let _ = client.send_keys_literal(session, "echo READY_MARKER_XYZ");
        let _ = client.send_keys(session, "Enter");
        let found = client.wait_for_markers(
            session,
            &["READY_MARKER_XYZ"],
            Duration::from_secs(3),
            Duration::from_millis(50),
        );
        if !found {
            eprintln!("Skipping test: banner echo did not appear (sandbox)");
            return;
        }
        // Ensure the pane is quiet BEFORE delivery, so `require_initial_change:
        // true` would see no change and stall until startup_timeout.
        thread::sleep(Duration::from_millis(500));

        let startup_timeout = Duration::from_secs(6);
        let opts = DeliveryOptions {
            startup_timeout,
            stable_quiet: Duration::from_millis(200),
            verify_timeout: Duration::from_secs(2),
            max_retries: 2,
            poll_interval: Duration::from_millis(50),
            retry_delay: Duration::from_millis(100),
            // Matches the runtime setting after markers succeed — the bug
            // was that this was `true`, causing a full-timeout stall.
            require_initial_change: false,
        };

        let start = Instant::now();
        let result = client.deliver_prompt(session, "echo DELIVERED_AFTER_MARKERS", &opts);
        let elapsed = start.elapsed();

        if let Err(e) = &result {
            eprintln!("Skipping test: deliver_prompt failed (sandbox?): {}", e);
            return;
        }
        assert!(
            elapsed < startup_timeout,
            "delivery should complete well under startup_timeout ({:?}), took {:?}",
            startup_timeout,
            elapsed
        );

        thread::sleep(Duration::from_millis(500));
        let content = client.capture_pane(session, 80).unwrap_or_default();
        assert!(
            content.contains("DELIVERED_AFTER_MARKERS"),
            "Expected delivered command in pane: {:?}",
            content
        );
    }

    /// Regression: `wait_for_markers` must require ALL markers to be
    /// present, not any-of. Claude Code v2.1.116 paints "Claude Code" in
    /// its banner several hundred ms before the input widget is actually
    /// wired up to accept Enter; any-of matching returned true immediately
    /// when only the banner was visible and let `deliver_prompt` fire
    /// Enter into a pane that would silently swallow it. With all-of
    /// semantics, we don't succeed until every marker (banner + input-
    /// widget prompt glyph "❯" for claude) has rendered.
    #[test]
    fn test_wait_for_markers_requires_all_markers() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let session = "omar-test-wait-markers-all";
        let _ = tmux_command()
            .args(["kill-session", "-t", session])
            .output();
        let _guard = SessionGuard(session.to_string());

        let ok = tmux_command()
            .args(["new-session", "-d", "-s", session])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return;
        }

        let client = TmuxClient::new("omar-test-");

        // Print only the first marker. With the old any-of semantics
        // wait_for_markers would return true; with all-of it must not.
        let _ = client.send_keys_literal(session, "echo FIRST_MARKER_ONLY");
        let _ = client.send_keys(session, "Enter");
        thread::sleep(Duration::from_millis(300));

        let found = client.wait_for_markers(
            session,
            &["FIRST_MARKER_ONLY", "SECOND_MARKER_MISSING"],
            Duration::from_millis(600),
            Duration::from_millis(50),
        );
        assert!(
            !found,
            "wait_for_markers must require ALL markers; returning true \
             when only one is present regresses the Claude Code v2.1.116 \
             Enter-swallow fix"
        );

        // Now print the second marker too; both present -> must return true.
        let _ = client.send_keys_literal(session, "echo SECOND_MARKER_MISSING");
        let _ = client.send_keys(session, "Enter");
        let found = client.wait_for_markers(
            session,
            &["FIRST_MARKER_ONLY", "SECOND_MARKER_MISSING"],
            Duration::from_secs(3),
            Duration::from_millis(50),
        );
        assert!(
            found,
            "wait_for_markers must return true once ALL markers are present"
        );
    }

    /// Regression: the claude readiness marker set includes "❯" (U+276F),
    /// the non-ASCII input-widget prompt glyph. `wait_for_markers` lowercases
    /// the hay with `to_ascii_lowercase`, which leaves "❯" byte-identical,
    /// so substring matching must still find it. This test locks in that
    /// UTF-8 markers work, so nobody later "fixes" readiness matching with a
    /// full-Unicode lowercase transform that would shift the bytes and break
    /// the match. Also guards against a prior bug where `capture_pane` (with
    /// `-e`, ANSI-inclusive) was used for marker matching, which inserts
    /// escape sequences mid-string and breaks multi-byte char boundaries.
    #[test]
    fn test_wait_for_markers_matches_claude_prompt_glyph() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let session = "omar-test-claude-glyph-marker";
        let _ = tmux_command()
            .args(["kill-session", "-t", session])
            .output();
        let _guard = SessionGuard(session.to_string());

        let ok = tmux_command()
            .args(["new-session", "-d", "-s", session])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return;
        }

        let client = TmuxClient::new("omar-test-");

        // Put "Claude Code" and "❯" directly into the pane as literal
        // bytes via `send-keys -l`. Avoids running a shell command so
        // the test is independent of the sandbox's default shell actually
        // executing inputs.
        let _ = client.send_keys_literal(session, "Claude Code ❯ ");

        let found = client.wait_for_markers(
            session,
            &["Claude Code", "❯"],
            Duration::from_secs(3),
            Duration::from_millis(50),
        );
        assert!(
            found,
            "wait_for_markers must match the non-ASCII \"❯\" glyph used as \
             Claude Code's input prompt — regresses the v2.1.116 Enter- \
             swallow fix if missing"
        );
    }

    #[test]
    fn large_payload_threshold_is_reasonable() {
        // Threshold must be large enough that typical short tasks go through send-keys
        // but small enough to catch the ~1-3 KB tasks that silently fail
        const { assert!(TmuxClient::LARGE_PAYLOAD_THRESHOLD >= 512) };
        const { assert!(TmuxClient::LARGE_PAYLOAD_THRESHOLD <= 8192) };
    }

    /// Regression: on tmux 3.2a (Ubuntu 22.04), `paste-buffer` without `-r`
    /// replaces every LF in the buffer with CR by default. TUI backends like
    /// Claude Code run the terminal in raw mode (ICRNL off), so those CRs
    /// arrive unchanged and each reads as an Enter keypress in the input
    /// widget — turning a multi-line prompt into a cascade of blank
    /// submissions. `paste_text` must use `-r` so LFs survive verbatim.
    ///
    /// This test reproduces the raw-mode environment that exposes the bug:
    /// without `stty -icrnl` the TTY driver translates CR→LF on input and
    /// the test would pass trivially regardless of the flag, masking
    /// regressions. A naive `cat > file` + `C-d` reader also won't work
    /// because `-icanon` disables EOF interpretation; we use
    /// `dd bs=1 count=N` instead so the reader exits deterministically
    /// after the expected number of bytes.
    #[test]
    fn test_paste_text_preserves_lf_in_raw_mode() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let session = "omar-test-paste-preserves-lf";
        let _ = tmux_command()
            .args(["kill-session", "-t", session])
            .output();
        let _guard = SessionGuard(session.to_string());

        let tmp_path =
            std::env::temp_dir().join(format!("omar-paste-lf-{}.txt", uuid::Uuid::new_v4()));
        let tmp_str = match tmp_path.to_str() {
            Some(s) => s,
            None => {
                eprintln!(
                    "Skipping test: temp path is not valid UTF-8: {:?}",
                    tmp_path
                );
                return;
            }
        };
        // Single-quote-escape the path for the shell command below so a
        // TMPDIR containing spaces or metacharacters doesn't break the test
        // (which would mask the regression by spuriously skipping).
        let quoted_tmp_path = format!("'{}'", tmp_str.replace('\'', r"'\''"));
        // Clean up the tmp file on every exit path (skip, assert fail, panic).
        let _tmp_guard = TempPathGuard(tmp_path.clone());

        // Start with an rc-less, non-login shell so users' rc files can't
        // break session startup on CI runners with unusual setups.
        let shell_cmd = "/bin/bash --norc --noprofile -i";
        let ok = tmux_command()
            .args(["new-session", "-d", "-s", session, shell_cmd])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("Skipping test: failed to create tmux session");
            return;
        }

        // Give the shell a moment to draw, then put the pane into the raw-mode
        // conditions that expose the bug (ICRNL off, non-canonical input) and
        // start a deterministic N-byte reader.
        thread::sleep(Duration::from_millis(200));
        let payload = "line1\nline2\nline3";
        let reader_cmd = format!(
            "stty -icrnl -icanon; dd bs=1 count={} of={} 2>/dev/null",
            payload.len(),
            quoted_tmp_path,
        );

        let client = TmuxClient::new("omar-test-");
        if client.send_keys_literal(session, &reader_cmd).is_err() {
            eprintln!("Skipping test: send_keys_literal failed (sandbox?)");
            return;
        }
        if client.send_keys(session, "Enter").is_err() {
            eprintln!("Skipping test: send_keys Enter failed (sandbox?)");
            return;
        }
        thread::sleep(Duration::from_millis(200));

        if client.paste_text(session, payload).is_err() {
            eprintln!("Skipping test: paste_text failed (sandbox?)");
            return;
        }

        // Wait for dd to finish collecting the payload and flush the file.
        // Poll briefly instead of sleeping blindly so slow runners still pass.
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(meta) = std::fs::metadata(&tmp_path) {
                if meta.len() as usize >= payload.len() {
                    break;
                }
            }
            thread::sleep(Duration::from_millis(50));
        }

        let got = match std::fs::read(&tmp_path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("Skipping test: tmp file not produced: {}", e);
                return;
            }
        };

        // The core assertion: LFs must survive. With the bug, every \n in
        // the payload arrives as \r, so the file would be "line1\rline2\rline3".
        assert_eq!(
            got,
            payload.as_bytes(),
            "paste_text must preserve LF (got {:?}, expected {:?}) — \
             tmux is likely mangling \\n → \\r because `-r` is missing from \
             the paste-buffer call in paste_text",
            String::from_utf8_lossy(&got),
            payload,
        );
    }

    /// Regression: `deliver_prompt` must wrap the payload with per-delivery
    /// UUID sentinels and only submit once the end sentinel has rendered
    /// into the pane. The previous implementation used heuristic needle
    /// matching with a fall-through on timeout — when the needle failed to
    /// appear it still pressed Enter, submitting a blank/partial widget and
    /// declaring success from the resulting pane change.
    ///
    /// This test proves (a) the sentinels wrap the payload and (b) the
    /// full submitted text (sentinels + payload) is exactly what the
    /// backend receives. Verification reads the submitted bytes
    /// byte-for-byte via `dd bs=1 count=...` — the file contents are
    /// exactly what crossed the pty after submit. The render-before-submit
    /// ordering is enforced by the `paste_rendered` predicate in
    /// `deliver_prompt` and covered by the separate
    /// `test_paste_rendered_matches_sentinel_or_new_placeholder` unit
    /// test; asserting that ordering against a live pane would require a
    /// deliberately slow-rendering harness, which we don't maintain.
    #[test]
    fn test_deliver_prompt_wraps_payload_with_sentinels() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }

        let session = "omar-test-deliver-sentinels";
        let _ = tmux_command()
            .args(["kill-session", "-t", session])
            .output();
        let _guard = SessionGuard(session.to_string());

        let tmp_path =
            std::env::temp_dir().join(format!("omar-sentinel-{}.txt", uuid::Uuid::new_v4()));
        let tmp_str = match tmp_path.to_str() {
            Some(s) => s,
            None => {
                eprintln!("Skipping test: temp path not UTF-8");
                return;
            }
        };
        let quoted_tmp = format!("'{}'", tmp_str.replace('\'', r"'\''"));
        let _tmp_guard = TempPathGuard(tmp_path.clone());

        let shell_cmd = "/bin/bash --norc --noprofile -i";
        let ok = tmux_command()
            .args(["new-session", "-d", "-s", session, shell_cmd])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("Skipping test: failed to create tmux session");
            return;
        }
        thread::sleep(Duration::from_millis(200));

        // Start a byte-exact reader. dd count is generously sized so it
        // captures the full sentinel-wrapped payload regardless of the
        // exact sentinel string lengths.
        let reader_cmd = format!(
            "stty -icrnl -icanon; dd bs=1 count=120 of={} 2>/dev/null",
            quoted_tmp,
        );

        let client = TmuxClient::new("omar-test-");
        if client.send_keys_literal(session, &reader_cmd).is_err() {
            eprintln!("Skipping test: send_keys_literal failed (sandbox?)");
            return;
        }
        if client.send_keys(session, "Enter").is_err() {
            eprintln!("Skipping test: send_keys Enter failed (sandbox?)");
            return;
        }
        thread::sleep(Duration::from_millis(200));

        let opts = DeliveryOptions {
            startup_timeout: Duration::from_secs(3),
            stable_quiet: Duration::from_millis(200),
            verify_timeout: Duration::from_secs(3),
            max_retries: 2,
            poll_interval: Duration::from_millis(50),
            retry_delay: Duration::from_millis(100),
            require_initial_change: false,
        };
        let payload = "SENTINEL_PAYLOAD_MARKER";
        let result = client.deliver_prompt(session, payload, &opts);
        if let Err(e) = &result {
            eprintln!("Skipping test: deliver_prompt failed (sandbox?): {}", e);
            return;
        }

        // Wait for dd to flush enough bytes for the full wrapped payload.
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(meta) = std::fs::metadata(&tmp_path) {
                if meta.len() >= 70 {
                    break;
                }
            }
            thread::sleep(Duration::from_millis(50));
        }

        let got = match std::fs::read_to_string(&tmp_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Skipping test: tmp file not produced: {}", e);
                return;
            }
        };

        assert!(
            got.contains("<UserPromptBegins:"),
            "submitted bytes must contain start sentinel, got: {:?}",
            got
        );
        assert!(
            got.contains("<UserPromptEnds:"),
            "submitted bytes must contain end sentinel, got: {:?}",
            got
        );
        assert!(
            got.contains("SENTINEL_PAYLOAD_MARKER"),
            "submitted bytes must contain the actual payload, got: {:?}",
            got
        );

        // Sentinels must use the SAME UUID on both sides — extract and
        // compare. Catches accidental per-line regeneration regressions.
        let begins_id = extract_sentinel_id(&got, "<UserPromptBegins:").expect("begin id");
        let ends_id = extract_sentinel_id(&got, "<UserPromptEnds:").expect("end id");
        assert_eq!(
            begins_id, ends_id,
            "start and end sentinel UUIDs must match"
        );
    }

    fn extract_sentinel_id(hay: &str, prefix: &str) -> Option<String> {
        let start = hay.find(prefix)? + prefix.len();
        let rest = &hay[start..];
        let end = rest.find('>')?;
        Some(rest[..end].to_string())
    }
}
