mod client;
mod health;
mod session;

pub use client::{flatten_agent_name, tmux_command, DeliveryOptions, TmuxClient};
pub use health::{HealthChecker, HealthState};
pub use session::Session;

/// Readiness markers for each supported backend — strings that must ALL
/// appear in a backend's rendered TUI before the pane is considered ready
/// to accept input. Single source of truth used by both the API's
/// `spawn_agent` path and the CLI `manager::spawn_worker` path.
///
/// For Claude Code specifically we require both the product banner
/// ("Claude Code") AND the input-widget prompt glyph ("❯"): on v2.1.116
/// the banner renders several hundred ms before the input widget is
/// actually wired up to accept keystrokes, so matching only on "Claude
/// Code" lets `deliver_prompt` fire Enter into a pane that silently
/// swallows it. "❯" is drawn by Claude Code as the leading character of
/// the input line and only appears once the widget has been laid out,
/// making it a reliable readiness signal that survives tmux config
/// differences (unlike the "? for shortcuts" hint, which Claude Code
/// replaces with a `tmux focus-events off` warning on stock Ubuntu tmux
/// configs — rendering that marker absent on Linux and stalling
/// `wait_for_markers` for its full timeout).
pub fn backend_readiness_markers(backend: &str) -> &'static [&'static str] {
    match backend {
        "codex" => &["OpenAI Codex"],
        "cursor" => &["Cursor Agent"],
        // `agy` is not available in CI/local discovery here, so avoid
        // overfitting to an unverified banner and let delivery use the
        // existing stable-pane fallback.
        "agy" => &[],
        "claude" => &["Claude Code", "❯"],
        "opencode" => &["tab agents", "ctrl+p commands"],
        _ => &[],
    }
}
