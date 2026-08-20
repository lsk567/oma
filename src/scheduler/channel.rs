//! Deliver an event without typing it into the agent's input box.
//!
//! Typing an event into the composer is the worst way to reach an agent: it
//! collides with whatever the user is drafting, and the event then reads as
//! something the user said. Several backends expose a side channel that takes
//! a message directly, leaving the input box alone.
//!
//! Resolution is deliberately dynamic — nothing is cached at launch. A pane
//! whose backend has restarted, or whose socket has gone, simply resolves to
//! `None` and delivery falls back to the input box.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};

/// How long to wait on a socket that has accepted the connection but is not
/// reading. Short: the fallback is a working delivery path, not an error.
const WRITE_TIMEOUT: Duration = Duration::from_secs(3);

/// A side channel into a running agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Channel {
    /// Claude Code's cross-session peer socket. The message is appended to the
    /// session's command queue, which is a separate structure from the input
    /// buffer, so a draft in the composer is untouched.
    ClaudePeer { socket: PathBuf, token: String },
}

impl Channel {
    /// Find the side channel for a pane, if its backend offers one.
    ///
    /// `pane_pid` is the pane's own process. The backend may be a child of it
    /// when the pane runs a shell, so its children are checked too.
    pub(crate) fn resolve(backend: &str, pane_pid: u32) -> Option<Channel> {
        match backend {
            "claude" => {
                let sessions = claude_sessions_dir()?;
                std::iter::once(pane_pid)
                    .chain(child_pids(pane_pid))
                    .find_map(|pid| claude_peer(&sessions, pid))
            }
            _ => None,
        }
    }

    /// Hand the event to the agent. Errors are the caller's cue to fall back.
    pub(crate) fn deliver(&self, text: &str) -> Result<()> {
        match self {
            Channel::ClaudePeer { socket, token } => {
                let mut stream = UnixStream::connect(socket)
                    .with_context(|| format!("connect to {}", socket.display()))?;
                stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
                stream
                    .write_all(claude_peer_frames(token, text).as_bytes())
                    .context("write peer message")?;
                stream.flush().context("flush peer message")?;
                Ok(())
            }
        }
    }

    pub(crate) fn describe(&self) -> &'static str {
        match self {
            Channel::ClaudePeer { .. } => "claude peer socket",
        }
    }
}

/// The two newline-delimited JSON frames a peer sends: authenticate, then the
/// message itself.
///
/// `priority: "next"` queues the event behind any turn already running instead
/// of preempting it — an event is news, not an interrupt.
fn claude_peer_frames(token: &str, text: &str) -> String {
    let auth = serde_json::json!({ "type": "auth", "token": token });
    let message = serde_json::json!({
        "type": "user",
        "priority": "next",
        "message": { "role": "user", "content": text },
    });
    format!("{}\n{}\n", auth, message)
}

fn claude_sessions_dir() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".claude").join("sessions"))
}

/// Read one session's registry entry and its peer token.
///
/// Claude Code writes `<pid>.json` describing the session and a sibling
/// `<pid>.<hash>.key` holding the token a peer must present.
fn claude_peer(sessions: &Path, pid: u32) -> Option<Channel> {
    let registry = std::fs::read_to_string(sessions.join(format!("{}.json", pid))).ok()?;
    let registry: serde_json::Value = serde_json::from_str(&registry).ok()?;
    let socket = PathBuf::from(registry.get("messagingSocketPath")?.as_str()?);
    if !socket.exists() {
        return None;
    }

    let prefix = format!("{}.", pid);
    let token = std::fs::read_dir(sessions)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| {
            path.extension().is_some_and(|ext| ext == "key")
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&prefix))
        })
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok())
        .and_then(|key| key.get("peerToken")?.as_str().map(str::to_string))?;

    Some(Channel::ClaudePeer { socket, token })
}

fn child_pids(parent: u32) -> Vec<u32> {
    Command::new("pgrep")
        .args(["-P", &parent.to_string()])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.trim().parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixListener;

    #[test]
    fn a_peer_message_is_two_json_frames_and_never_touches_the_composer() {
        let frames = claude_peer_frames("deadbeef", "standup in 5 minutes");
        let mut lines = frames.lines();

        let auth: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(auth["type"], "auth");
        assert_eq!(auth["token"], "deadbeef");

        let message: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(message["type"], "user");
        assert_eq!(message["message"]["role"], "user");
        assert_eq!(message["message"]["content"], "standup in 5 minutes");
        // Queued behind a running turn rather than preempting it.
        assert_eq!(message["priority"], "next");

        assert!(lines.next().is_none());
        assert!(frames.ends_with('\n'), "frames are newline-delimited");
    }

    #[test]
    fn delivery_writes_both_frames_to_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("peer.sock");
        let listener = UnixListener::bind(&socket).unwrap();

        let reader = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut received = String::new();
            let _ = stream.read_to_string(&mut received);
            received
        });

        Channel::ClaudePeer {
            socket: socket.clone(),
            token: "t0ken".to_string(),
        }
        .deliver("ship it")
        .expect("deliver over the peer socket");

        let received = reader.join().unwrap();
        assert_eq!(received, claude_peer_frames("t0ken", "ship it"));
    }

    #[test]
    fn a_backend_without_a_side_channel_resolves_to_nothing() {
        // The fallback is the input box, so "no channel" must be an ordinary
        // answer rather than an error.
        for backend in ["codex", "opencode", "cursor", "agy", "stub"] {
            assert_eq!(Channel::resolve(backend, std::process::id()), None);
        }
    }

    #[test]
    fn a_session_whose_socket_is_gone_is_not_offered_as_a_channel() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("4242.json"),
            r#"{"pid":4242,"messagingSocketPath":"/tmp/does-not-exist-omar.sock"}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("4242.abc.key"), r#"{"peerToken":"unused"}"#).unwrap();

        assert_eq!(claude_peer(dir.path(), 4242), None);
    }

    /// Guards against the real registry drifting from the shape we parse.
    /// Skips when no Claude Code session is running, so CI stays green.
    #[test]
    fn a_live_claude_session_resolves_to_a_channel() {
        let Some(sessions) = claude_sessions_dir().filter(|dir| dir.is_dir()) else {
            eprintln!("Skipping test: no Claude Code sessions directory");
            return;
        };

        let live: Vec<u32> = std::fs::read_dir(&sessions)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                name.strip_suffix(".json")?.parse::<u32>().ok()
            })
            .filter(|pid| claude_peer(&sessions, *pid).is_some())
            .collect();

        if live.is_empty() {
            eprintln!("Skipping test: no running Claude Code session to resolve");
            return;
        }

        for pid in live {
            let channel = claude_peer(&sessions, pid).expect("resolved above");
            let Channel::ClaudePeer { socket, token } = channel;
            assert!(socket.exists(), "socket for {pid} must exist");
            assert!(!token.is_empty(), "peer token for {pid} must be non-empty");
        }
    }

    #[test]
    fn a_registry_entry_and_its_key_resolve_to_a_channel() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("live.sock");
        let _listener = UnixListener::bind(&socket).unwrap();

        std::fs::write(
            dir.path().join("77.json"),
            serde_json::json!({ "pid": 77, "messagingSocketPath": socket }).to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("77.9f3.key"),
            r#"{"peerToken":"s3cret","procStart":"now"}"#,
        )
        .unwrap();

        assert_eq!(
            claude_peer(dir.path(), 77),
            Some(Channel::ClaudePeer {
                socket,
                token: "s3cret".to_string()
            })
        );
    }
}
