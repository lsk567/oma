//! Attaching to an agent's tmux session over a pseudo-terminal.
//!
//! `tmux attach-session` refuses to run without a controlling terminal, so the
//! session is opened behind a PTY and its bytes are relayed verbatim. Nothing
//! interprets them: the agent draws a TUI, and the viewer's job is to carry the
//! escape sequences through unchanged.

use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;

use anyhow::{bail, Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, PtySize};

use crate::tmux::{flatten_agent_name, tmux_command};

/// A window smaller than this is not worth attaching to, and a size that large
/// is a tmux answer we did not understand.
const MIN_DIMENSION: u16 = 2;
const MAX_DIMENSION: u16 = 1000;

/// A session target that matches this session and nothing else.
///
/// tmux resolves `-t name` by prefix when nothing matches it exactly, so with
/// sessions like `…-w` and `…-w2` a lookup for one that has since gone lands
/// silently on its neighbour — reading the wrong agent's size, or turning the
/// wrong agent's mouse on.
fn exact_session(session: &str) -> String {
    format!("={session}")
}

/// The same, for the commands that want a target inside the session.
///
/// `display-message`, `set-option` and `show-options` reject a bare `=name`
/// ("no such session"); the trailing colon is what makes it the session's
/// current window rather than a window called `=name`.
fn exact_target(session: &str) -> String {
    format!("={session}:")
}

/// The size tmux is currently drawing a session at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowSize {
    pub cols: u16,
    pub rows: u16,
    /// Rows tmux spends on its status line, which belong to the client rather
    /// than the window.
    pub status_lines: u16,
}

impl WindowSize {
    /// The terminal height a client must have for the window to keep `rows`.
    ///
    /// tmux gives the window whatever is left after its status line, so a
    /// client exactly `rows` tall silently costs the agent a row — and tmux
    /// does not give it back on detach.
    pub fn client_rows(&self) -> u16 {
        self.rows.saturating_add(self.status_lines)
    }
}

/// Ask tmux how large the session's window is.
///
/// This is the size the viewer must adopt. A client that attaches at a
/// different size resizes the window to its own, and tmux does not put it back
/// on detach — the agent would be left redrawing its TUI at the viewer's
/// dimensions long after the viewer had gone.
pub fn window_size(session: &str) -> Result<WindowSize> {
    let output = tmux_command()
        .args([
            "display-message",
            "-p",
            "-t",
            &exact_target(session),
            "#{window_width}x#{window_height}x#{status}",
        ])
        .output()
        .context("failed to ask tmux for the window size")?;
    if !output.status.success() {
        bail!(
            "tmux could not describe session '{session}': {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_window_size(String::from_utf8_lossy(&output.stdout).trim())
}

/// Let the viewer scroll the pane.
///
/// A tmux client only forwards wheel events when the session asks for mouse
/// tracking, and an agent session is created without it — so a viewer had no
/// way to scroll back through what the agent had already drawn. Best effort:
/// failing to set it costs scrolling, not the attachment.
fn enable_mouse(session: &str) {
    let _ = tmux_command()
        .args(["set-option", "-t", &exact_target(session), "mouse", "on"])
        .output();
}

fn parse_window_size(reported: &str) -> Result<WindowSize> {
    let mut fields = reported.split('x');
    let (Some(cols), Some(rows), Some(status), None) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        bail!("tmux reported an unreadable window size '{reported}'");
    };
    let size = WindowSize {
        cols: cols.parse().context("window width is not a number")?,
        rows: rows.parse().context("window height is not a number")?,
        // The status option is `off`, `on`, or a count of lines.
        status_lines: match status {
            "off" => 0,
            "on" => 1,
            count => count
                .parse()
                .with_context(|| format!("unreadable status option '{count}'"))?,
        },
    };
    if !(MIN_DIMENSION..=MAX_DIMENSION).contains(&size.cols)
        || !(MIN_DIMENSION..=MAX_DIMENSION).contains(&size.rows)
    {
        bail!("tmux reported an implausible window size '{reported}'");
    }
    Ok(size)
}

/// A live attachment to one agent's session.
///
/// Dropping it detaches: the `tmux attach` process is killed, which is what
/// closing the viewer means. The agent's own session is untouched.
pub struct Attachment {
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: mpsc::Receiver<Vec<u8>>,
    pub size: WindowSize,
}

impl Attachment {
    /// Attach to `agent`'s session through `prefix`.
    pub fn open(prefix: &str, agent: &str) -> Result<Self> {
        let session = format!("{prefix}{}", flatten_agent_name(agent));
        let size = window_size(&session)?;

        let pty = native_pty_system()
            .openpty(PtySize {
                rows: size.client_rows(),
                cols: size.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to open a pseudo-terminal")?;

        // `-t =name` pins the target to a session of exactly that name, so a
        // prefix match cannot attach the viewer to somebody else's agent.
        let mut command = CommandBuilder::new("tmux");
        if let Ok(server) = std::env::var("OMAR_TMUX_SERVER") {
            let server = server.trim().to_string();
            if !server.is_empty() {
                command.args(["-L", &server]);
            }
        }
        command.args(["attach-session", "-t", exact_session(&session).as_str()]);
        // A nested tmux would refuse to attach.
        command.env_remove("TMUX");

        let child = pty
            .slave
            .spawn_command(command)
            .context("failed to start tmux attach")?;
        // The slave is held open by the child; keeping our copy would stop the
        // reader ever seeing EOF once the child exits.
        drop(pty.slave);
        // Only now that a viewer really is attached: everything above can fail,
        // and a session left with mouse tracking on by an attempt that never
        // arrived is a change nobody asked for.
        enable_mouse(&session);

        let mut reader = pty
            .master
            .try_clone_reader()
            .context("failed to read from the pseudo-terminal")?;
        let writer = pty
            .master
            .take_writer()
            .context("failed to write to the pseudo-terminal")?;

        // The master must outlive the reader thread but is not itself Send in
        // every implementation, so the thread owns only the reader.
        let (sender, output) = mpsc::channel();
        thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        if sender.send(buffer[..read].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Ok(Self {
            child,
            writer,
            output,
            size,
        })
    }

    /// Bytes the agent has drawn since the last call, if any.
    pub fn read(&self, timeout: std::time::Duration) -> Option<Vec<u8>> {
        self.output.recv_timeout(timeout).ok()
    }

    /// Send keystrokes to the agent.
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        // Killing the attach client detaches it. tmux keeps the session, so the
        // agent carries on exactly as it was.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Requires tmux, so it is skipped where the binary is absent.
    fn tmux_available() -> bool {
        tmux_command()
            .arg("-V")
            .output()
            .is_ok_and(|output| output.status.success())
    }

    /// Kills only its own session, so it cannot disturb the operator's server.
    struct SessionGuard(String);

    impl Drop for SessionGuard {
        fn drop(&mut self) {
            let _ = tmux_command()
                .args(["kill-session", "-t", &exact_session(&self.0)])
                .output();
        }
    }

    #[test]
    fn attaching_relays_both_ways_and_leaves_the_window_alone() {
        if !tmux_available() {
            eprintln!("skipping: tmux is not installed");
            return;
        }
        // Sessions are shared with whatever server the operator is running, so
        // the name is distinctive and only it gets cleaned up. Setting
        // OMAR_TMUX_SERVER instead would be process-global and would race the
        // other tmux tests.
        let session = "omar-terminal-relay-probe";
        let _ = tmux_command()
            .args(["kill-session", "-t", &exact_session(session)])
            .output();
        let _guard = SessionGuard(session.to_string());

        // A deliberately unusual size: if attaching resizes it, this changes.
        tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                session,
                "-x",
                "173",
                "-y",
                "47",
                "sh",
            ])
            .output()
            .expect("tmux runs");

        let before = window_size(session).expect("size before");
        assert_eq!((before.cols, before.rows), (173, 47));

        {
            let mut attachment = Attachment::open("", session).expect("attach");
            assert_eq!(
                attachment.size, before,
                "the viewer adopts the agent's size"
            );

            attachment.write(b"echo omar-relay-ok\n").expect("write");
            let mut seen = String::new();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            while !seen.contains("omar-relay-ok") && std::time::Instant::now() < deadline {
                if let Some(chunk) = attachment.read(std::time::Duration::from_millis(200)) {
                    seen.push_str(&String::from_utf8_lossy(&chunk));
                }
            }
            assert!(seen.contains("omar-relay-ok"), "got: {seen}");

            assert_eq!(
                window_size(session).expect("size during"),
                before,
                "attaching must not resize the agent's window"
            );
        }

        // Dropping detaches. The status line is why this is not simply `rows`:
        // a client exactly as tall as the window costs the agent a row.
        std::thread::sleep(std::time::Duration::from_millis(400));
        assert_eq!(
            window_size(session).expect("size after"),
            before,
            "detaching must leave the window as it was"
        );
    }

    #[test]
    fn a_failed_attach_leaves_the_session_as_it_found_it() {
        // Mouse tracking is turned on for the viewer's benefit. An attempt that
        // never attaches has no viewer, so it has no business changing anything.
        if !tmux_available() {
            eprintln!("skipping: tmux is not installed");
            return;
        }
        let session = "omar-terminal-failed-attach";
        let _ = tmux_command()
            .args(["kill-session", "-t", &exact_session(session)])
            .output();
        let _guard = SessionGuard(session.to_string());
        tmux_command()
            .args(["new-session", "-d", "-s", session, "sh"])
            .output()
            .expect("tmux runs");
        tmux_command()
            .args(["set-option", "-t", &exact_target(session), "mouse", "off"])
            .output()
            .expect("tmux runs");

        // A name that does not resolve: the attach fails before any viewer.
        assert!(Attachment::open("", "omar-terminal-does-not-exist").is_err());

        let mouse = tmux_command()
            .args(["show-options", "-t", &exact_target(session), "-v", "mouse"])
            .output()
            .expect("tmux answers");
        assert_eq!(String::from_utf8_lossy(&mouse.stdout).trim(), "off");
    }

    #[test]
    fn attaching_lets_the_viewer_scroll() {
        // Without mouse tracking a tmux client never sees a wheel event, so the
        // viewer cannot scroll back through what the agent already drew.
        if !tmux_available() {
            eprintln!("skipping: tmux is not installed");
            return;
        }
        let session = "omar-terminal-mouse-probe";
        let _ = tmux_command()
            .args(["kill-session", "-t", &exact_session(session)])
            .output();
        let _guard = SessionGuard(session.to_string());
        tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                session,
                "-x",
                "80",
                "-y",
                "24",
                "sh",
            ])
            .output()
            .expect("tmux runs");
        tmux_command()
            .args(["set-option", "-t", &exact_target(session), "mouse", "off"])
            .output()
            .expect("tmux runs");

        let attachment = Attachment::open("", session).expect("attach");

        let reported = tmux_command()
            .args(["show-options", "-t", &exact_target(session), "-v", "mouse"])
            .output()
            .expect("tmux answers");
        drop(attachment);
        assert_eq!(String::from_utf8_lossy(&reported.stdout).trim(), "on");
    }

    #[test]
    fn a_missing_session_is_not_confused_with_its_neighbour() {
        // tmux resolves `-t name` by prefix when nothing matches exactly, so a
        // lookup for an agent that has gone would land on one whose name it is
        // a prefix of — reporting that agent's size, or attaching to it.
        if !tmux_available() {
            eprintln!("skipping: tmux is not installed");
            return;
        }
        let gone = "omar-terminal-prefix";
        let neighbour = "omar-terminal-prefix2";
        for name in [gone, neighbour] {
            let _ = tmux_command()
                .args(["kill-session", "-t", &exact_session(name)])
                .output();
        }
        let _guard = SessionGuard(neighbour.to_string());
        tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                neighbour,
                "-x",
                "111",
                "-y",
                "31",
                "sh",
            ])
            .output()
            .expect("tmux runs");

        // The neighbour is fine to read; the one that does not exist must not
        // resolve to it.
        assert_eq!(window_size(neighbour).expect("neighbour").cols, 111);
        assert!(
            window_size(gone).is_err(),
            "a session that does not exist resolved to '{neighbour}'"
        );
        assert!(
            Attachment::open("", gone).is_err(),
            "attached to the wrong session"
        );
    }

    #[test]
    fn window_sizes_are_read_and_bounded() {
        let size = parse_window_size("200x50xon").unwrap();
        assert_eq!((size.cols, size.rows, size.status_lines), (200, 50, 1));
        // A client must be a row taller than the window to leave it intact.
        assert_eq!(size.client_rows(), 51);
        assert_eq!(parse_window_size("200x50xoff").unwrap().client_rows(), 50);
        assert_eq!(parse_window_size("200x50x2").unwrap().client_rows(), 52);
        // tmux answers on stdout, so anything unparseable is a protocol change
        // rather than something to guess at.
        for bad in [
            "",
            "200",
            "200x50",
            "200x50xmaybe",
            "0x50xon",
            "200x9999xon",
        ] {
            assert!(parse_window_size(bad).is_err(), "{bad} should be rejected");
        }
    }
}
