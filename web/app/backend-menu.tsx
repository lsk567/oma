"use client";

import { useEffect, useRef, useState } from "react";
import { fetchBackends, switchBackend } from "./lib/runtime-client";

/** How each backend is written for a human. */
const LABELS: Record<string, string> = {
  claude: "Claude Code",
  codex: "Codex",
  cursor: "Cursor",
  opencode: "opencode",
  agy: "agy",
};

function label(backend: string): string {
  return LABELS[backend] ?? backend;
}

/**
 * Which backend the assistant runs on, and a way to change it.
 *
 * It opens upward because it lives at the bottom of the composer, and choosing
 * a different backend restarts the assistant — a backend is picked when the
 * process starts — so the change is confirmed rather than applied on a click.
 */
export function BackendMenu({
  serveUrl,
  live,
}: {
  serveUrl: string;
  live: boolean;
}) {
  const [current, setCurrent] = useState<string | null>(null);
  const [available, setAvailable] = useState<string[]>([]);
  const [open, setOpen] = useState(false);
  const [pending, setPending] = useState<string | null>(null);
  const [error, setError] = useState("");
  const rootRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    if (!live) return;
    let cancelled = false;
    fetchBackends(serveUrl)
      .then((backends) => {
        if (cancelled) return;
        setCurrent(backends.backend);
        setAvailable(backends.available);
      })
      // A runtime that predates this endpoint simply offers no choice.
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [serveUrl, live]);

  useEffect(() => {
    if (!open) return;
    const dismiss = (event: MouseEvent) => {
      if (!rootRef.current?.contains(event.target as Node)) {
        setOpen(false);
        setPending(null);
      }
    };
    window.addEventListener("mousedown", dismiss);
    return () => window.removeEventListener("mousedown", dismiss);
  }, [open]);

  async function choose(backend: string) {
    setError("");
    try {
      await switchBackend(serveUrl, backend);
      setCurrent(backend);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    }
    setPending(null);
    setOpen(false);
  }

  if (!live || available.length === 0) {
    return <span className="composer-status">Draft mode</span>;
  }

  return (
    <div className="backend-menu" ref={rootRef}>
      <button
        type="button"
        className="backend-trigger"
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => setOpen((was) => !was)}
      >
        {current ? label(current) : "Assistant"}
        <span aria-hidden="true">{open ? "▾" : "▴"}</span>
      </button>

      {open ? (
        <div className="backend-options" role="menu">
          {pending ? (
            <div className="backend-confirm">
              {/* Worth saying plainly: the running assistant is discarded. */}
              <p>
                Restart the assistant on {label(pending)}? Its current session
                is lost.
              </p>
              <span>
                <button type="button" onClick={() => void choose(pending)}>
                  Restart
                </button>
                <button type="button" onClick={() => setPending(null)}>
                  Cancel
                </button>
              </span>
            </div>
          ) : (
            available.map((backend) => (
              <button
                key={backend}
                type="button"
                role="menuitem"
                className={backend === current ? "current" : undefined}
                onClick={() =>
                  backend === current ? setOpen(false) : setPending(backend)
                }
              >
                {label(backend)}
                {backend === current ? <span aria-hidden="true">✓</span> : null}
              </button>
            ))
          )}
        </div>
      ) : null}

      {error ? <span className="backend-error">{error}</span> : null}
    </div>
  );
}
