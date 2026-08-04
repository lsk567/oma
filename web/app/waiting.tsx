"use client";

import { useEffect, useState } from "react";

/**
 * The assistant can take a while, and a static line gives no sign anything is
 * still happening. The elapsed clock carries that; the verb is chosen once per
 * wait and held, because a word changing under the operator reads as churn
 * rather than progress. It advances between turns instead, so a new wait is
 * visibly a new wait.
 */
const VERBS = [
  "Drafting",
  "Considering",
  "Sketching",
  "Weighing",
  "Composing",
  "Refining",
  "Reasoning",
  "Working",
];

let cursor = 0;

/** Rotates rather than randomising, so consecutive waits never repeat. */
function takeVerb(): string {
  const verb = VERBS[cursor % VERBS.length];
  cursor += 1;
  return verb;
}

/** `12s`, `3m 04s`, `1h 05m 03s` — seconds always, larger units as needed. */
export function formatElapsed(totalSeconds: number): string {
  const seconds = Math.max(0, Math.floor(totalSeconds));
  const pad = (value: number) => String(value).padStart(2, "0");
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ${pad(seconds % 60)}s`;
  const hours = Math.floor(minutes / 60);
  return `${hours}h ${pad(minutes % 60)}m ${pad(seconds % 60)}s`;
}

export function Waiting({ label }: { label?: string }) {
  const [elapsed, setElapsed] = useState(0);
  // Chosen on mount and held for this wait; a new wait mounts a new component.
  const [verb] = useState(() => label ?? takeVerb());

  useEffect(() => {
    const started = Date.now();
    const timer = setInterval(
      () => setElapsed(Math.floor((Date.now() - started) / 1000)),
      250,
    );
    return () => clearInterval(timer);
  }, []);

  return (
    <p className="waiting" role="status" aria-live="polite">
      <span className="waiting-verb">{verb}</span>
      <span className="waiting-dots" aria-hidden="true">
        <i />
        <i />
        <i />
      </span>
      <span className="waiting-elapsed">{formatElapsed(elapsed)}</span>
    </p>
  );
}
