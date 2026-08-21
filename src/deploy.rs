use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::tmux::TmuxClient;
use crate::topology::write_json_atomic;

/// TERMINATED covers natural drain and graceful stop; CANCELLED is a force
/// kill; FAILED is any error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DeploymentState {
    Created,
    Deploying,
    Running,
    Stopping,
    Terminated,
    Failed,
    Cancelled,
}

impl DeploymentState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Terminated | Self::Failed | Self::Cancelled)
    }

    /// Whether `next` is a legal successor. Forward-only; any non-terminal
    /// state may fail or be cancelled; a terminal state is final.
    pub fn may_become(self, next: Self) -> bool {
        use DeploymentState::*;
        if self.is_terminal() {
            return false;
        }
        match next {
            Deploying => self == Created,
            Running => self == Deploying,
            Stopping => self == Running,
            Terminated => matches!(self, Running | Stopping),
            Failed | Cancelled => true,
            Created => false,
        }
    }
}

impl fmt::Display for DeploymentState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Created => "CREATED",
            Self::Deploying => "DEPLOYING",
            Self::Running => "RUNNING",
            Self::Stopping => "STOPPING",
            Self::Terminated => "TERMINATED",
            Self::Failed => "FAILED",
            Self::Cancelled => "CANCELLED",
        };
        f.write_str(name)
    }
}

/// One transition, kept so the record carries its own event history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitionEvent {
    pub state: DeploymentState,
    pub at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentRecord {
    pub deployment_id: String,
    pub team: String,
    pub state: DeploymentState,
    /// Runner pid; its liveness separates a run in flight from a crash.
    pub pid: u32,
    pub started_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Agent name to tmux session, so teardown needs no re-verify.
    pub sessions: BTreeMap<String, String>,
    /// Per-invocation timeout, which bounds a graceful stop.
    pub timeout_seconds: u64,
    pub history: Vec<TransitionEvent>,
}

impl DeploymentRecord {
    pub fn create(team: &str, sessions: BTreeMap<String, String>, timeout_seconds: u64) -> Self {
        let now = now_unix();
        Self {
            deployment_id: uuid::Uuid::new_v4().to_string(),
            team: team.to_string(),
            state: DeploymentState::Created,
            pid: std::process::id(),
            started_at: now,
            finished_at: None,
            error: None,
            sessions,
            timeout_seconds,
            history: vec![TransitionEvent {
                state: DeploymentState::Created,
                at: now,
                detail: None,
            }],
        }
    }

    /// Move to `next`, refusing an illegal transition.
    pub fn advance(&mut self, next: DeploymentState, detail: Option<&str>) -> Result<()> {
        if !self.state.may_become(next) {
            bail!("deployment cannot go {} -> {}", self.state, next);
        }
        let now = now_unix();
        self.state = next;
        if next.is_terminal() {
            self.finished_at = Some(now);
        }
        self.history.push(TransitionEvent {
            state: next,
            at: now,
            detail: detail.map(str::to_string),
        });
        Ok(())
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        write_json_atomic(&record_path(dir), self)
    }

    pub fn load(dir: &Path) -> Result<Option<Self>> {
        let path = record_path(dir);
        if !path.exists() {
            return Ok(None);
        }
        let bytes =
            std::fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
        let record = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid deployment record {}", path.display()))?;
        Ok(Some(record))
    }

    pub fn is_active(&self) -> bool {
        !self.state.is_terminal()
    }

    pub fn runner_alive(&self) -> bool {
        process_alive(self.pid)
    }
}

pub fn record_path(dir: &Path) -> PathBuf {
    dir.join("deployment.json")
}

fn control_path(dir: &Path) -> PathBuf {
    dir.join("control.json")
}

pub fn logs_dir(dir: &Path) -> PathBuf {
    dir.join("logs")
}

pub fn outputs_path(dir: &Path) -> PathBuf {
    dir.join("outputs.json")
}

#[derive(Debug, Serialize, Deserialize)]
struct StopRequest {
    requested_at: u64,
}

/// Ask the runner to stop at the next tag boundary.
pub fn request_stop(dir: &Path) -> Result<()> {
    write_json_atomic(
        &control_path(dir),
        &StopRequest {
            requested_at: now_unix(),
        },
    )
}

pub fn stop_requested(dir: &Path) -> bool {
    control_path(dir).exists()
}

/// Remove any stop request, so a finished stop cannot end the next run.
pub fn clear_stop(dir: &Path) -> Result<()> {
    let path = control_path(dir);
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("failed to remove {}", path.display()))?;
    }
    Ok(())
}

pub fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

pub fn kill_process(pid: u32) {
    let _ = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Terminal sessions beneath a deployment. tmux is the only implementation
/// today; this seam lets another process host slot in.
pub trait SessionHost {
    fn exists(&self, session: &str) -> Result<bool>;
    fn capture(&self, session: &str) -> Result<String>;
    fn kill(&self, session: &str) -> Result<()>;
}

/// Pane history to keep as an agent's log.
const CAPTURE_LINES: i32 = 10_000;

impl SessionHost for TmuxClient {
    fn exists(&self, session: &str) -> Result<bool> {
        self.has_session(session)
    }

    fn capture(&self, session: &str) -> Result<String> {
        self.capture_pane_plain(session, CAPTURE_LINES)
    }

    fn kill(&self, session: &str) -> Result<()> {
        self.kill_session(session)
    }
}

/// Capture each session's pane as a log, then kill it. Best effort per
/// session; failures come back for the caller to report.
pub fn teardown_sessions(
    host: &dyn SessionHost,
    sessions: &BTreeMap<String, String>,
    logs: &Path,
) -> Vec<String> {
    let mut failures = Vec::new();
    for (agent, session) in sessions {
        match host.exists(session) {
            Ok(false) => continue,
            Ok(true) => {}
            Err(error) => {
                failures.push(format!("{agent}: {error:#}"));
                continue;
            }
        }
        if let Ok(log) = host.capture(session) {
            let _ = std::fs::create_dir_all(logs);
            let _ = std::fs::write(
                logs.join(format!("{}.txt", crate::tmux::flatten_agent_name(agent))),
                log,
            );
        }
        if let Err(error) = host.kill(session) {
            failures.push(format!("{agent}: {error:#}"));
        }
    }
    failures
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitions_follow_the_lifecycle() {
        use DeploymentState::*;
        assert!(Created.may_become(Deploying));
        assert!(Deploying.may_become(Running));
        assert!(Running.may_become(Stopping));
        assert!(Running.may_become(Terminated));
        assert!(Stopping.may_become(Terminated));
        for state in [Created, Deploying, Running, Stopping] {
            assert!(state.may_become(Failed));
            assert!(state.may_become(Cancelled));
        }

        assert!(!Created.may_become(Running));
        assert!(!Deploying.may_become(Terminated));
        assert!(!Stopping.may_become(Running));
        for terminal in [Terminated, Failed, Cancelled] {
            for next in [
                Created, Deploying, Running, Stopping, Terminated, Failed, Cancelled,
            ] {
                assert!(!terminal.may_become(next));
            }
        }
    }

    #[test]
    fn advance_refuses_illegal_and_records_history() {
        let mut record = DeploymentRecord::create("Demo", BTreeMap::new(), 300);
        assert!(record.advance(DeploymentState::Running, None).is_err());
        record.advance(DeploymentState::Deploying, None).unwrap();
        record.advance(DeploymentState::Running, None).unwrap();
        record
            .advance(DeploymentState::Terminated, Some("run completed"))
            .unwrap();
        assert!(record.finished_at.is_some());
        assert!(record.advance(DeploymentState::Failed, None).is_err());
        let states: Vec<_> = record.history.iter().map(|event| event.state).collect();
        use DeploymentState::*;
        assert_eq!(states, vec![Created, Deploying, Running, Terminated]);
    }

    #[test]
    fn record_round_trips_and_stop_control_toggles() {
        let dir = tempfile::tempdir().unwrap();
        let mut record = DeploymentRecord::create("Demo", BTreeMap::new(), 42);
        record.advance(DeploymentState::Deploying, None).unwrap();
        record.save(dir.path()).unwrap();

        let loaded = DeploymentRecord::load(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.state, DeploymentState::Deploying);
        assert_eq!(loaded.team, "Demo");
        assert_eq!(loaded.timeout_seconds, 42);
        assert!(loaded.is_active());

        assert!(!stop_requested(dir.path()));
        request_stop(dir.path()).unwrap();
        assert!(stop_requested(dir.path()));
        clear_stop(dir.path()).unwrap();
        assert!(!stop_requested(dir.path()));
    }

    #[test]
    fn dead_pid_is_not_alive() {
        let mut record = DeploymentRecord::create("Demo", BTreeMap::new(), 300);
        assert!(record.runner_alive());
        record.pid = u32::MAX - 1;
        assert!(!record.runner_alive());
    }
}
