#![allow(dead_code)]

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::backend_probe;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub dashboard: DashboardConfig,

    #[serde(default)]
    pub health: HealthConfig,

    #[serde(default)]
    pub agent: AgentConfig,

    #[serde(default)]
    pub metrics: MetricsConfig,

    #[serde(default)]
    pub slack_bridge: SlackBridgeConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardConfig {
    /// Refresh interval in seconds
    #[serde(default = "default_refresh_interval")]
    pub refresh_interval: u64,

    /// Session name prefix for agent sessions
    #[serde(default = "default_session_prefix")]
    pub session_prefix: String,

    /// Show event queue in sidebar
    #[serde(default = "default_true")]
    pub show_event_queue: bool,

    /// Sidebar on right side
    #[serde(default = "default_true")]
    pub sidebar_right: bool,

    /// Show inspirational quotes in the status bar
    #[serde(default)]
    pub show_quotes: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthConfig {
    /// Seconds of inactivity before warning (yellow)
    #[serde(default = "default_idle_warning")]
    pub idle_warning: i64,

    /// Seconds of inactivity before critical (red/stuck)
    #[serde(default = "default_idle_critical")]
    pub idle_critical: i64,

    /// Patterns in output that indicate an error
    #[serde(default = "default_error_patterns")]
    pub error_patterns: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Default command to run for new agents
    #[serde(default = "default_command")]
    pub default_command: String,

    /// Default working directory
    #[serde(default = "default_workdir")]
    pub default_workdir: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetricsConfig {
    /// Enable global spawn metrics sink at ~/.omar/metrics/spawn_metrics.jsonl
    #[serde(default)]
    pub spawn_metrics_enabled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlackBridgeConfig {
    /// EA name the slack bridge targets. The bridge resolves this against
    /// the EA registry at startup; if unset or unresolvable it falls back
    /// to the first registered EA. Set via the `/ea <name>` Slack command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_ea: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_refresh_interval() -> u64 {
    1
}

fn default_session_prefix() -> String {
    "omar-agent-".to_string()
}

fn default_idle_warning() -> i64 {
    15
}

fn default_idle_critical() -> i64 {
    300
}

fn default_error_patterns() -> Vec<String> {
    vec![
        "error".to_string(),
        "failed".to_string(),
        "rate limit".to_string(),
        "exception".to_string(),
    ]
}

/// Detect which agent command is available on the system.
/// Checks PATH for the supported first-class backends, falling back to `claude`.
fn detect_agent_command() -> String {
    detect_agent_command_from(&[
        ("claude", "claude --dangerously-skip-permissions"),
        (
            "codex",
            "codex --no-alt-screen --dangerously-bypass-approvals-and-sandbox",
        ),
        ("cursor", "cursor agent --yolo"),
        ("opencode", "opencode"),
        ("agy", "agy --dangerously-skip-permissions"),
    ])
    .unwrap_or_else(|| "claude --dangerously-skip-permissions".to_string())
}

fn detect_agent_command_from(candidates: &[(&str, &str)]) -> Option<String> {
    candidates
        .iter()
        .find(|(binary, _)| backend_probe::backend_version_probe_succeeds(binary))
        .map(|(_, command)| (*command).to_string())
}

fn default_command() -> String {
    detect_agent_command()
}

/// Map shorthand agent names to full commands.
///
/// - `"claude"` → `"claude --dangerously-skip-permissions"`
/// - `"codex"` → `"codex --no-alt-screen --dangerously-bypass-approvals-and-sandbox"`
/// - `"cursor"` → `"cursor agent --yolo"`
/// - `"opencode"` → `"opencode"` (opencode has no permission-skip flag)
/// - `"agy"` → `"agy --dangerously-skip-permissions"`
/// - `"stub"` → the model-free test agent
/// - anything else → error
pub fn resolve_backend(name: &str) -> Result<String, String> {
    match name {
        "claude" => Ok("claude --dangerously-skip-permissions".to_string()),
        "codex" => {
            Ok("codex --no-alt-screen --dangerously-bypass-approvals-and-sandbox".to_string())
        }
        "cursor" => Ok("cursor agent --yolo".to_string()),
        "opencode" => Ok("opencode".to_string()),
        "agy" => Ok("agy --dangerously-skip-permissions".to_string()),
        // Answers invocations without a model, so a run can be exercised end to
        // end in a test. Resolved to this binary in `build_agent_command`.
        "stub" => Ok("omar stub-agent".to_string()),
        other => Err(format!(
            "Unknown backend '{}'. Supported: claude, codex, cursor, opencode, agy, stub",
            other
        )),
    }
}

fn default_workdir() -> String {
    ".".to_string()
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            refresh_interval: default_refresh_interval(),
            session_prefix: default_session_prefix(),
            show_event_queue: true,
            sidebar_right: true,
            show_quotes: false,
        }
    }
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            idle_warning: default_idle_warning(),
            idle_critical: default_idle_critical(),
            error_patterns: default_error_patterns(),
        }
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            default_command: default_command(),
            default_workdir: default_workdir(),
        }
    }
}

impl Config {
    /// Default config path: ~/.omar/config.toml
    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".omar")
            .join("config.toml")
    }

    /// Resolve a config path from CLI input, expanding `~/`.
    pub fn resolve_path(path: Option<&str>) -> PathBuf {
        match path {
            Some(p) => expand_tilde(p),
            None => Self::default_path(),
        }
    }

    /// Load config from file. Creates default config at ~/.omar/config.toml on first run.
    pub fn load(path: Option<&str>) -> Result<Self> {
        let expanded_path = Self::resolve_path(path);

        if !expanded_path.exists() {
            let config = Self::default();
            config.save_to_path(&expanded_path);
            return Ok(config);
        }

        let contents =
            std::fs::read_to_string(&expanded_path).context("Failed to read config file")?;

        let mut config: Self = toml::from_str(&contents).context("Failed to parse config file")?;
        config.dashboard.session_prefix =
            normalize_session_prefix(&config.dashboard.session_prefix);
        Ok(config)
    }

    /// Save config to its default path (~/.omar/config.toml)
    pub fn save(&self) {
        self.save_to_path(&Self::default_path());
    }

    /// Save config to a specific path.
    pub fn save_to_path(&self, path: &std::path::Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        if let Ok(contents) = toml::to_string_pretty(self) {
            std::fs::write(path, contents).ok();
        }
    }

    /// Number of settings exposed in the dashboard panel.
    pub fn settings_count(&self) -> usize {
        4
    }

    /// Settings panel entry: a labelled toggle or a labelled text field.
    pub fn settings_item(&self, index: usize) -> Option<SettingItem<'_>> {
        match index {
            0 => Some(SettingItem::Toggle {
                label: "Show event queue in sidebar",
                value: self.dashboard.show_event_queue,
            }),
            1 => Some(SettingItem::Toggle {
                label: "Sidebar on right side",
                value: self.dashboard.sidebar_right,
            }),
            2 => Some(SettingItem::Toggle {
                label: "Show inspirational quotes",
                value: self.dashboard.show_quotes,
            }),
            3 => Some(SettingItem::Text {
                label: "Slack bridge target EA (name)",
                value: self.slack_bridge.active_ea.as_deref().unwrap_or(""),
            }),
            _ => None,
        }
    }

    /// Toggle a boolean setting and save. No-op for text-typed settings.
    pub fn toggle_setting(&mut self, index: usize) {
        match index {
            0 => self.dashboard.show_event_queue = !self.dashboard.show_event_queue,
            1 => self.dashboard.sidebar_right = !self.dashboard.sidebar_right,
            2 => self.dashboard.show_quotes = !self.dashboard.show_quotes,
            _ => return,
        }
        self.save();
    }

    /// Set a text-typed setting by index and save. An empty string clears
    /// the field (serializes as a missing key thanks to
    /// `skip_serializing_if = "Option::is_none"`). Returns `true` if the
    /// index targets a text setting.
    pub fn set_text_setting(&mut self, index: usize, value: &str) -> bool {
        let trimmed = value.trim();
        match index {
            3 => {
                self.slack_bridge.active_ea = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                };
            }
            _ => return false,
        }
        self.save();
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingItem<'a> {
    Toggle { label: &'a str, value: bool },
    Text { label: &'a str, value: &'a str },
}

impl SettingItem<'_> {
    pub fn is_text(&self) -> bool {
        matches!(self, SettingItem::Text { .. })
    }
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }
    PathBuf::from(path)
}

fn normalize_session_prefix(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return String::new();
    }
    let raw = raw.trim_end_matches('-');
    if raw.is_empty() {
        return String::new();
    }
    format!("{}-", raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.dashboard.refresh_interval, 1);
        assert_eq!(config.dashboard.session_prefix, "omar-agent-");
        assert!(config.dashboard.show_event_queue);
        assert!(config.dashboard.sidebar_right);
        assert_eq!(config.health.idle_warning, 15);
        assert_eq!(config.health.idle_critical, 300);
        assert!(!config.metrics.spawn_metrics_enabled);
    }

    #[test]
    fn test_settings_toggle() {
        let mut config = Config::default();
        assert!(config.dashboard.show_event_queue);
        assert!(!config.dashboard.show_quotes);
        assert_eq!(config.settings_count(), 4);
        assert!(matches!(
            config.settings_item(0),
            Some(SettingItem::Toggle {
                label: "Show event queue in sidebar",
                value: true,
            })
        ));
        // Toggle without saving to disk (just test the in-memory toggle)
        config.dashboard.show_event_queue = !config.dashboard.show_event_queue;
        assert!(!config.dashboard.show_event_queue);
    }

    #[test]
    fn slack_bridge_text_setting_round_trips() {
        let _env_lock = crate::test_env_lock();
        let mut config = Config::default();
        // The default state is no persisted EA — exposed as the empty string.
        assert!(matches!(
            config.settings_item(3),
            Some(SettingItem::Text {
                label: "Slack bridge target EA (name)",
                value: "",
            })
        ));
        // Use a tempdir so save() doesn't write to ~/.omar in tests.
        let dir = tempfile::tempdir().unwrap();
        let original = std::env::var("HOME").ok();
        std::env::set_var("HOME", dir.path());

        assert!(config.set_text_setting(3, "Research"));
        assert_eq!(config.slack_bridge.active_ea.as_deref(), Some("Research"));

        // Whitespace-only input clears the field rather than storing blanks.
        assert!(config.set_text_setting(3, "   "));
        assert_eq!(config.slack_bridge.active_ea, None);

        // Toggle indices return false (rejected).
        assert!(!config.set_text_setting(0, "anything"));

        match original {
            Some(prev) => std::env::set_var("HOME", prev),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn test_parse_config_with_settings() {
        let toml = r#"
[dashboard]
show_event_queue = false
sidebar_right = false
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(!config.dashboard.show_event_queue);
        assert!(!config.dashboard.sidebar_right);
    }

    #[test]
    fn test_default_command_detects_agent() {
        let cmd = detect_agent_command();
        // Should return a non-empty command regardless of what's installed
        assert!(!cmd.is_empty());
        // Should be one of the known agent commands
        assert!(
            cmd.contains("claude")
                || cmd.contains("codex")
                || cmd.contains("cursor")
                || cmd.contains("opencode")
                || cmd.contains("agy"),
            "Unexpected default command: {}",
            cmd
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_detect_agent_command_skips_hanging_probe() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};

        let temp = tempfile::tempdir().unwrap();
        let slow = temp.path().join("slow-agent");
        let fast = temp.path().join("fast-agent");
        fs::write(&slow, "#!/bin/sh\nsleep 5\n").unwrap();
        fs::write(&fast, "#!/bin/sh\nexit 0\n").unwrap();
        for path in [&slow, &fast] {
            let mut perms = fs::metadata(path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(path, perms).unwrap();
        }

        let start = Instant::now();
        let cmd = detect_agent_command_from(&[
            (slow.to_str().unwrap(), "slow command"),
            (fast.to_str().unwrap(), "fast command"),
        ]);

        assert_eq!(cmd.as_deref(), Some("fast command"));
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "hanging probe should be skipped after the bounded timeout"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_detect_agent_command_can_select_agy() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let agy = temp.path().join("agy");
        fs::write(&agy, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = fs::metadata(&agy).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&agy, perms).unwrap();

        let cmd = detect_agent_command_from(&[(
            agy.to_str().unwrap(),
            "agy --dangerously-skip-permissions",
        )]);

        assert_eq!(cmd.as_deref(), Some("agy --dangerously-skip-permissions"));
    }

    #[test]
    fn test_parse_config() {
        let toml = r#"
[dashboard]
refresh_interval = 5
session_prefix = "test-"
show_event_queue = false
sidebar_right = false

[health]
idle_warning = 30
idle_critical = 120
error_patterns = ["error", "panic"]

[agent]
default_command = "bash"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.dashboard.refresh_interval, 5);
        assert_eq!(config.dashboard.session_prefix, "test-");
        assert!(!config.dashboard.show_event_queue);
        assert!(!config.dashboard.sidebar_right);
        assert_eq!(config.health.idle_warning, 30);
        assert_eq!(config.health.error_patterns, vec!["error", "panic"]);
    }

    #[test]
    fn test_load_normalizes_session_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
[dashboard]
session_prefix = "omar-agent"
"#,
        )
        .unwrap();

        let config = Config::load(Some(config_path.to_str().unwrap())).unwrap();
        assert_eq!(config.dashboard.session_prefix, "omar-agent-");
    }

    #[test]
    fn test_resolve_backend_known_names() {
        assert_eq!(
            resolve_backend("claude").unwrap(),
            "claude --dangerously-skip-permissions"
        );
        assert_eq!(
            resolve_backend("codex").unwrap(),
            "codex --no-alt-screen --dangerously-bypass-approvals-and-sandbox"
        );
        assert_eq!(resolve_backend("cursor").unwrap(), "cursor agent --yolo");
        assert_eq!(resolve_backend("opencode").unwrap(), "opencode");
        assert_eq!(
            resolve_backend("agy").unwrap(),
            "agy --dangerously-skip-permissions"
        );
    }

    #[test]
    fn test_resolve_backend_unknown_errors() {
        assert!(resolve_backend("aider --yes").is_err());
        assert!(resolve_backend("custom-agent").is_err());
    }

    #[test]
    fn test_parse_config_opencode_backend() {
        let toml = r#"
[agent]
default_command = "opencode"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.agent.default_command, "opencode");
    }

    #[test]
    fn test_parse_config_codex_backend() {
        let toml = r#"
[agent]
default_command = "codex"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.agent.default_command, "codex");
    }

    #[test]
    fn test_parse_config_custom_backend() {
        let toml = r#"
[agent]
default_command = "aider --yes"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.agent.default_command, "aider --yes");
    }

    #[test]
    fn test_parse_metrics_config() {
        let toml = r#"
[metrics]
spawn_metrics_enabled = true
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.metrics.spawn_metrics_enabled);
    }

    #[test]
    fn test_load_missing_custom_path_writes_custom_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("custom.toml");

        let _config = Config::load(Some(path.to_str().unwrap())).unwrap();

        assert!(
            path.exists(),
            "missing explicit config path should be created"
        );
    }
}
