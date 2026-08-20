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

/// Loopback HTTP is either immediate or wedged; nothing in between.
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to keep waiting for a freshly launched backend to open its port.
/// opencode takes several seconds to boot; past this it is not coming up.
const PROVISION_TIMEOUT: Duration = Duration::from_secs(90);

/// A side channel into a running agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Channel {
    /// Claude Code's cross-session peer socket. The message is appended to the
    /// session's command queue, which is a separate structure from the input
    /// buffer, so a draft in the composer is untouched.
    ClaudePeer { socket: PathBuf, token: String },
    /// opencode's HTTP server. A `synthetic` message part is shown to the
    /// model but not rendered in the transcript, and `noReply` seats it
    /// without starting a turn.
    OpencodeHttp { port: u16, session: String },
}

impl Channel {
    /// Find the side channel for a pane, if its backend offers one.
    ///
    /// `pane_pid` is the pane's own process. The backend may be a child of it
    /// when the pane runs a shell, so its children are checked too.
    pub fn resolve(backend: &str, pane_pid: u32, stamp: Option<&str>) -> Option<Channel> {
        // A stamp is written when the backend needed provisioning at launch;
        // it names the port and session an event must be addressed to.
        if let Some(channel) = stamp.and_then(Channel::from_stamp) {
            return Some(channel);
        }
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

    /// Parse a stamp written at launch, e.g. `opencode:47455:ses_abc`.
    fn from_stamp(stamp: &str) -> Option<Channel> {
        let (kind, rest) = stamp.split_once(':')?;
        match kind {
            "opencode" => {
                let (port, session) = rest.split_once(':')?;
                (!session.is_empty()).then_some(Channel::OpencodeHttp {
                    port: port.parse().ok()?,
                    session: session.to_string(),
                })
            }
            _ => None,
        }
    }

    /// Hand the event to the agent. Errors are the caller's cue to fall back.
    pub fn deliver(&self, text: &str) -> Result<()> {
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
            Channel::OpencodeHttp { port, session } => {
                let body = serde_json::json!({
                    "noReply": true,
                    "parts": [{ "type": "text", "text": text, "synthetic": true }],
                })
                .to_string();
                let (status, _) = http_json(
                    *port,
                    "POST",
                    &format!("/session/{}/message", session),
                    Some(&body),
                )
                .context("post message to opencode")?;
                if status != 200 {
                    anyhow::bail!("opencode answered {}", status);
                }
                Ok(())
            }
        }
    }

    pub fn describe(&self) -> &'static str {
        match self {
            Channel::ClaudePeer { .. } => "claude peer socket",
            Channel::OpencodeHttp { .. } => "opencode http api",
        }
    }
}

/// Claim a free loopback port for a backend that must be told one at launch.
///
/// The socket is closed immediately, so this reserves nothing — it only picks a
/// number the OS was willing to hand out. Losing the race means the backend
/// fails to bind and the pane falls back to the input box.
pub fn free_port() -> Option<u16> {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .ok()?
        .local_addr()
        .ok()
        .map(|addr| addr.port())
}

/// Give a freshly launched pane a side channel, once its backend is listening.
///
/// Runs in the background: opencode takes seconds to boot, and a launch must
/// not block on it. Until the stamp lands, deliveries fall back to the input
/// box, so being slow is safe and failing is safe.
pub fn provision_in_background(session: String, command: String) {
    let Some(port) = opencode_port(&command) else {
        return;
    };
    std::thread::spawn(move || {
        if let Some(stamp) = provision_opencode(port) {
            let _ = crate::tmux::TmuxClient::new("").set_session_delivery(&session, &stamp);
        }
    });
}

/// `--port N` as it appears in a launch command.
fn opencode_port(command: &str) -> Option<u16> {
    let mut tokens = command.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == "--port" {
            return tokens.next()?.parse().ok();
        }
        if let Some(value) = token.strip_prefix("--port=") {
            return value.parse().ok();
        }
    }
    None
}

/// Create a session on a running opencode server and point its TUI at it.
///
/// opencode's API cannot say which session a given pane is showing, and a pane
/// only creates one once the user speaks. So OMAR makes the session itself:
/// the id it gets back is then unambiguously this pane's, even when several
/// agents share a directory.
fn provision_opencode(port: u16) -> Option<String> {
    let deadline = std::time::Instant::now() + PROVISION_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if let Ok((200, body)) = http_json(port, "POST", "/session", Some("{}")) {
            let session = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|value| value.get("id")?.as_str().map(str::to_string))?;
            let select = serde_json::json!({ "sessionID": session }).to_string();
            // Without this the pane keeps showing a different session and the
            // user never sees what the agent was told.
            http_json(port, "POST", "/tui/select-session", Some(&select)).ok()?;
            return Some(format!("opencode:{}:{}", port, session));
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    None
}

/// POST JSON to opencode's loopback server and return the status code.
///
/// Hand-rolled rather than pulling in an HTTP stack: the server is on
/// 127.0.0.1, the request shape is fixed, and the response is discarded.
fn http_json(port: u16, method: &str, path: &str, body: Option<&str>) -> Result<(u16, String)> {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port))
        .with_context(|| format!("connect to 127.0.0.1:{}", port))?;
    stream.set_read_timeout(Some(HTTP_TIMEOUT))?;
    stream.set_write_timeout(Some(HTTP_TIMEOUT))?;

    let body = body.unwrap_or("");
    let request = format!(
        "{} {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        method,
        path,
        port,
        body.len(),
        body
    );
    stream.write_all(request.as_bytes())?;
    stream.flush()?;

    let mut response = Vec::new();
    std::io::Read::read_to_end(&mut stream, &mut response)?;
    let response = String::from_utf8_lossy(&response).into_owned();
    let (status, body) = split_response(&response).context("parse opencode response")?;
    Ok((status, body))
}

/// Split an HTTP/1.x response into its status code and body.
fn split_response(response: &str) -> Option<(u16, String)> {
    let status = response.split_whitespace().nth(1)?.parse().ok()?;
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or_default();
    Some((status, body))
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
    fn a_launch_stamp_names_the_port_and_session_to_address() {
        assert_eq!(
            Channel::from_stamp("opencode:47455:ses_abc"),
            Some(Channel::OpencodeHttp {
                port: 47455,
                session: "ses_abc".to_string()
            })
        );
        // A stamp wins over backend sniffing, so a malformed one must not be
        // silently treated as a working channel.
        for bad in [
            "opencode:47455:",
            "opencode:notaport:ses_abc",
            "opencode:47455",
            "something-else:1:2",
            "",
        ] {
            assert_eq!(
                Channel::from_stamp(bad),
                None,
                "stamp {bad:?} must not parse"
            );
        }
    }

    #[test]
    fn a_port_is_read_back_out_of_a_launch_command() {
        assert_eq!(opencode_port("opencode --port 47455"), Some(47455));
        assert_eq!(opencode_port("FOO=1 opencode --port=47455"), Some(47455));
        assert_eq!(opencode_port("opencode"), None);
        assert_eq!(opencode_port("opencode --port bogus"), None);
    }

    #[test]
    fn an_http_response_yields_its_status_and_body() {
        assert_eq!(
            split_response("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}"),
            Some((200, "{}".to_string()))
        );
        assert_eq!(
            split_response("HTTP/1.1 404 Not Found\r\n\r\n").map(|(status, _)| status),
            Some(404)
        );
    }

    #[test]
    fn a_backend_without_a_side_channel_resolves_to_nothing() {
        // The fallback is the input box, so "no channel" must be an ordinary
        // answer rather than an error.
        for backend in ["codex", "opencode", "cursor", "agy", "stub"] {
            assert_eq!(Channel::resolve(backend, std::process::id(), None), None);
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
            match claude_peer(&sessions, pid).expect("resolved above") {
                Channel::ClaudePeer { socket, token } => {
                    assert!(socket.exists(), "socket for {pid} must exist");
                    assert!(!token.is_empty(), "peer token for {pid} must be non-empty");
                }
                other => panic!("claude must resolve to a peer socket, got {other:?}"),
            }
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
