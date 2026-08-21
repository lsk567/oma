//! Deployment lifecycle integration tests: the real binary, a stub-backed
//! never-ending program, an isolated tmux server per test.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use uuid::Uuid;

const TEAM: &str = "Pulse";

/// Bytecode the fake compiler passes through. A 300ms timer keeps the run
/// alive forever; the stub answers each tag.
const PROGRAM: &str = r#"{
  "version": 1,
  "team": "Pulse",
  "instructions": [
    {"op":"begin_plan","team":"Pulse"},
    {"op":"spawn_agent","name":"w","backend":"stub"},
    {"op":"declare_timer","name":"t","offset":0,"period":300000000},
    {"op":"define_port","kind":"output","name":"beat","type":"string"},
    {"op":"install_reaction","id":"reaction.0","agent":"w","triggers":["t"],"effects":["beat"],"contract":"beat","prompt":"Beat"},
    {"op":"commit_plan"}
  ]
}"#;

struct Harness {
    home: tempfile::TempDir,
    program: PathBuf,
    tmux_server: String,
    omarc: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("tempdir");
        let program = home.path().join("Pulse.omar");
        std::fs::write(&program, PROGRAM).expect("write program");
        // omarc is the Lean compiler, which a cargo test run does not build.
        // The program above is already bytecode, so a copy stands in.
        let omarc = home.path().join("omarc");
        std::fs::write(&omarc, "#!/bin/sh\ncp \"$1\" \"$2\"\n").expect("write omarc");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&omarc).expect("stat omarc").permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&omarc, permissions).expect("chmod omarc");
        }
        Self {
            home,
            program,
            tmux_server: format!("omar-deploy-test-{}", Uuid::new_v4()),
            omarc,
        }
    }

    fn omar(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_omar"));
        cmd.env("HOME", self.home.path())
            .env("OMAR_TMUX_SERVER", &self.tmux_server)
            .env("OMARC_BIN", &self.omarc);
        cmd
    }

    fn start_run(&self) -> Child {
        self.omar()
            .args(["run", self.program.to_str().unwrap()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn omar run")
    }

    /// stdout of `omar status <TEAM>`, tolerating a record not written yet.
    fn status(&self) -> String {
        let output = self
            .omar()
            .args(["status", TEAM])
            .output()
            .expect("run omar status");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn wait_for_state(&self, state: &str, timeout: Duration) {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if self.status().contains(&format!("state: {state}")) {
                return;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        panic!(
            "deployment never reached {state}; last status:\n{}",
            self.status()
        );
    }

    fn sessions(&self) -> Vec<String> {
        let output = Command::new("tmux")
            .args([
                "-L",
                &self.tmux_server,
                "list-sessions",
                "-F",
                "#{session_name}",
            ])
            .output()
            .expect("run tmux");
        // No server left is the cleanest possible answer.
        if !output.status.success() {
            return Vec::new();
        }
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn deployment_dir(&self) -> PathBuf {
        // The default EA is created on first use with id 0.
        self.home
            .path()
            .join(".omar")
            .join("ea")
            .join("0")
            .join("topologies")
            .join(TEAM)
    }

    fn record(&self) -> String {
        std::fs::read_to_string(self.deployment_dir().join("deployment.json"))
            .expect("read deployment record")
    }

    fn kill_server(&self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.tmux_server, "kill-server"])
            .output();
    }
}

fn agent_sessions(harness: &Harness) -> Vec<String> {
    harness
        .sessions()
        .into_iter()
        .filter(|name| name.contains("-w"))
        .collect()
}

fn assert_dir_has(dir: &Path, name: &str) {
    assert!(
        dir.join(name).exists(),
        "{name} missing in {}",
        dir.display()
    );
}

#[test]
fn stop_terminates_gracefully_and_leaves_no_orphans() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux not installed; skipping");
        return;
    }
    let harness = Harness::new();
    let mut run = harness.start_run();
    harness.wait_for_state("RUNNING", Duration::from_secs(30));
    assert!(
        !agent_sessions(&harness).is_empty(),
        "agent session never appeared"
    );

    // A second run of the same team is refused while the first is alive.
    let duplicate = harness
        .omar()
        .args(["run", harness.program.to_str().unwrap()])
        .output()
        .expect("run duplicate");
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("stop it first"));

    let stop = harness
        .omar()
        .args(["stop", TEAM])
        .output()
        .expect("run omar stop");
    assert!(
        stop.status.success(),
        "stop failed: {}",
        String::from_utf8_lossy(&stop.stderr)
    );
    assert!(String::from_utf8_lossy(&stop.stdout).contains("TERMINATED"));

    let exit = run.wait().expect("wait for run");
    assert!(exit.success(), "run should exit cleanly after a stop");
    assert!(harness.record().contains("TERMINATED"));
    assert!(agent_sessions(&harness).is_empty(), "orphan agent session");

    let dir = harness.deployment_dir();
    assert_dir_has(&dir, "outputs.json");
    assert_dir_has(&dir, "state.json");
    assert_dir_has(&dir.join("logs"), "w.txt");
    harness.kill_server();
}

#[test]
fn kill_cancels_and_sweeps_sessions() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux not installed; skipping");
        return;
    }
    let harness = Harness::new();
    let mut run = harness.start_run();
    harness.wait_for_state("RUNNING", Duration::from_secs(30));

    let kill = harness
        .omar()
        .args(["kill", TEAM])
        .output()
        .expect("run omar kill");
    assert!(
        kill.status.success(),
        "kill failed: {}",
        String::from_utf8_lossy(&kill.stderr)
    );
    assert!(String::from_utf8_lossy(&kill.stdout).contains("CANCELLED"));

    run.wait().expect("wait for run");
    assert!(harness.record().contains("CANCELLED"));
    assert!(agent_sessions(&harness).is_empty(), "orphan agent session");
    harness.kill_server();
}

#[test]
fn a_crashed_runner_is_reported_and_cleaned_up() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux not installed; skipping");
        return;
    }
    let harness = Harness::new();
    let mut run = harness.start_run();
    harness.wait_for_state("RUNNING", Duration::from_secs(30));

    // A hard kill of the runner is a crash: nothing writes an ending.
    run.kill().expect("kill runner");
    run.wait().expect("wait for runner");

    let status = harness.status();
    assert!(status.contains("state: FAILED"), "status was:\n{status}");
    assert!(status.contains("runner process died"));
    assert!(
        !agent_sessions(&harness).is_empty(),
        "a crash leaves sessions behind; that is what kill sweeps"
    );

    let kill = harness
        .omar()
        .args(["kill", TEAM])
        .output()
        .expect("run omar kill");
    assert!(kill.status.success());
    assert!(agent_sessions(&harness).is_empty(), "orphan agent session");
    harness.kill_server();
}
