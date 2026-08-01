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

use crate::tmux::tmux_command;

/// A window smaller than this is not worth attaching to, and a size that large
/// is a tmux answer we did not understand.
const MIN_DIMENSION: u16 = 2;
const MAX_DIMENSION: u16 = 1000;

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
            session,
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
    /// Attach to a session by its exact name.
    ///
    /// A session, not a prefix and an agent: the assistant's is
    /// `<base>ea-<id>` and is built from no agent name at all, so the caller
    /// names the one it means rather than passing an empty prefix to say so.
    pub fn open_session(session: &str) -> Result<Self> {
        let session = session.to_string();
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
        command.args(["attach-session", "-t", &format!("={session}")]);
        // A nested tmux would refuse to attach.
        command.env_remove("TMUX");

        let child = pty
            .slave
            .spawn_command(command)
            .context("failed to start tmux attach")?;
        // The slave is held open by the child; keeping our copy would stop the
        // reader ever seeing EOF once the child exits.
        drop(pty.slave);

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
                .args(["kill-session", "-t", &self.0])
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
            .args(["kill-session", "-t", session])
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
            let mut attachment = Attachment::open_session(session).expect("attach");
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
