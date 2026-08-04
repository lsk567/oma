"use client";

import { useEffect, useRef, useState } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import { ASSISTANT, terminalUrlFor } from "./lib/runtime-client";

/**
 * A live terminal attached to one agent's tmux session.
 *
 * The terminal fits the panel and the session reflows to match, which is what
 * a terminal emulator does and why the text is always crisp: nothing is scaled,
 * so nothing can be clipped or blurred.
 *
 * That resizes the agent's tmux window while the viewer is open. The daemon
 * restores it on detach, which is what makes this safe to do at all.
 */
export function AgentTerminal({
  serveUrl,
  agent,
  onClose,
}: {
  serveUrl: string;
  agent: string;
  onClose: () => void;
}) {
  const screenRef = useRef<HTMLDivElement | null>(null);
  const frameRef = useRef<HTMLDivElement | null>(null);
  const [error, setError] = useState("");
  const [size, setSize] = useState<{ cols: number; rows: number } | null>(null);

  useEffect(() => {
    const screen = screenRef.current;
    if (!screen) return;

    const terminal = new Terminal({
      convertEol: false,
      cursorBlink: true,
      fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
      fontSize: 13,
      theme: { background: "#0b0b0e", foreground: "#d8d5e0" },
    });
    const fit = new FitAddon();
    terminal.loadAddon(fit);
    terminal.open(screen);

    const socket = new WebSocket(terminalUrlFor(serveUrl, agent));
    socket.binaryType = "arraybuffer";

    // Ask the session for the shape this panel can hold. The daemon reflows
    // it and answers with what it settled on.
    const claim = () => {
      fit.fit();
      if (socket.readyState !== WebSocket.OPEN) return;
      socket.send(
        JSON.stringify({ cols: terminal.cols, rows: terminal.rows }),
      );
    };

    socket.onopen = claim;
    socket.onmessage = (event) => {
      // Text is the geometry the session settled on; everything else is raw
      // terminal bytes.
      if (typeof event.data === "string") {
        setSize(JSON.parse(event.data) as { cols: number; rows: number });
        return;
      }
      terminal.write(new Uint8Array(event.data as ArrayBuffer));
    };

    // The panel can change under it — the window resizes, a divider moves.
    const observer = new ResizeObserver(claim);
    if (frameRef.current) observer.observe(frameRef.current);
    socket.onerror = () =>
      setError(`Could not attach to ${agent}. Is the run still going?`);
    socket.onclose = (event) => {
      // 1000 and 1005 are an ordinary close, including the one we ask for.
      if (event.code !== 1000 && event.code !== 1005) {
        setError(`The terminal closed (${event.code}).`);
      }
    };

    // Keystrokes go up the same socket, which is why this is a socket at all.
    const typing = terminal.onData((data) => {
      if (socket.readyState === WebSocket.OPEN) {
        socket.send(new TextEncoder().encode(data));
      }
    });
    terminal.focus();

    return () => {
      observer.disconnect();
      typing.dispose();
      // Closing the socket is the detach; the agent's session carries on, at
      // the size it had before this viewer arrived.
      socket.close();
      terminal.dispose();
    };
  }, [serveUrl, agent]);

  useEffect(() => {
    const close = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", close);
    return () => window.removeEventListener("keydown", close);
  }, [onClose]);

  return (
    <div
      className="terminal-overlay"
      role="dialog"
      aria-label={`Terminal for ${agent === ASSISTANT ? "the assistant" : agent}`}
      // Clicking the backdrop closes, but a click inside must not.
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <div className="terminal-panel">
        <header className="terminal-heading">
          <h2>{agent === ASSISTANT ? "assistant" : agent}</h2>
          <span>
            {error
              ? error
              : size
                ? `${size.cols}×${size.rows} · attached`
                : "attaching…"}
          </span>
          <button type="button" onClick={onClose} aria-label="Close terminal">
            Close
          </button>
        </header>
        <div className="terminal-frame" ref={frameRef}>
          <div className="terminal-screen" ref={screenRef} />
        </div>
        <p className="terminal-note">
          {agent === ASSISTANT
            ? // Worth saying: the chat and this terminal drive one pane, so a
              // half-typed line here collides with the next reply delivered.
              "This is the same session the chat talks to. Typing here shares the pane with it. Closing detaches; the assistant keeps running."
            : "Typing here goes straight to the agent. Closing detaches; the agent keeps running."}
        </p>
      </div>
    </div>
  );
}
