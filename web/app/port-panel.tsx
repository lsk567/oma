"use client";

import { FormEvent, useEffect, useMemo, useState } from "react";

import {
  formatDuration,
  parseInputValue,
  type DiagramSnapshot,
  type PendingInvocation,
} from "./lib/protocol";

/**
 * One web component's panel: what it can see, and what it is waiting to be told.
 *
 * Opened by double-clicking the component, not docked beside the events. A
 * program may declare several `Web` agents and each has its own interface —
 * different ports, different prompts, different deadlines — so one shared panel
 * would have to invent a way to merge them, and there is no honest way to do
 * that.
 *
 * Scoped to this agent's wiring, which is the same rule the runtime enforces:
 * the ports its reactions trigger on are what it may see, and the ports they
 * declare as effects are what it may set. Neither is a setting of the panel.
 */
export function PortPanel({
  agent,
  snapshot,
  pending,
  sending,
  onAnswer,
  onClose,
}: {
  /** Qualified agent name, as the diagram and the daemon both spell it. */
  agent: string;
  snapshot: DiagramSnapshot;
  pending: PendingInvocation[];
  sending: boolean;
  onAnswer: (invocation: PendingInvocation, values: Record<string, unknown>) => void;
  onClose: () => void;
}) {
  useEffect(() => {
    const dismiss = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", dismiss);
    return () => window.removeEventListener("keydown", dismiss);
  }, [onClose]);

  // The ports this component is wired to, in the order a reader wants them:
  // what it reads, then what it writes.
  const wiring = useMemo(() => {
    const agentId = snapshot.agents.find((a) => a.name === agent)?.id;
    const mine = snapshot.reactions.filter((reaction) => reaction.agent === agentId);
    const reads = new Set(mine.flatMap((reaction) => reaction.triggers));
    const writes = new Set(mine.flatMap((reaction) => reaction.effects));
    const port = (id: string) => snapshot.ports.find((p) => p.id === id);
    return {
      reads: [...reads].map(port).filter((p) => p !== undefined),
      writes: [...writes].map(port).filter((p) => p !== undefined),
    };
  }, [agent, snapshot]);

  return (
    <div className="port-overlay" role="dialog" aria-label={`Port panel for ${agent}`}>
      <div className="port-panel">
        <header className="port-panel-head">
          <div>
            <span className="eyebrow">PORT PANEL</span>
            <h2>{agent}</h2>
          </div>
          <button type="button" className="port-panel-close" onClick={onClose}>
            Close
          </button>
        </header>

        <div className="port-panel-body">
          {pending.map((invocation) => (
            <Waiting
              key={invocation.invocation_id}
              invocation={invocation}
              sending={sending}
              onAnswer={onAnswer}
            />
          ))}
          {pending.length === 0 ? (
            <p className="port-panel-idle">
              Nothing is waiting on you. This panel opens the moment one of this
              component&rsquo;s reactions is invoked.
            </p>
          ) : null}

          <PortList label="SEES" ports={wiring.reads} />
          <PortList label="SETS" ports={wiring.writes} />
        </div>
      </div>
    </div>
  );
}

function PortList({
  label,
  ports,
}: {
  label: string;
  ports: DiagramSnapshot["ports"];
}) {
  if (ports.length === 0) return null;
  return (
    <div className="port-list">
      <span className="eyebrow">{label}</span>
      {ports.map((port) => (
        <div key={port.id} className={`port-row${port.value === null ? "" : " filled"}`}>
          <span className="port-row-name">{port.name}</span>
          <span className="port-row-type">{port.type}</span>
          <span className="port-row-value">
            {port.value === null ? "—" : JSON.stringify(port.value)}
          </span>
          <span className="port-row-tag">
            {port.last_tag === null
              ? ""
              : `${formatDuration(port.last_tag.timestamp)}:${port.last_tag.microstep}`}
          </span>
        </div>
      ))}
    </div>
  );
}

/** One invocation, and the fields it is owed. */
function Waiting({
  invocation,
  sending,
  onAnswer,
}: {
  invocation: PendingInvocation;
  sending: boolean;
  onAnswer: (invocation: PendingInvocation, values: Record<string, unknown>) => void;
}) {
  const [draft, setDraft] = useState<Record<string, string>>({});
  const effects = Object.entries(invocation.allowed_effects);

  // What is typed, read as the type each port declares. Doing it here means a
  // value that cannot be read is a field the operator can see is wrong, rather
  // than a batch the run refuses.
  const typed = effects.flatMap(([port, type]) => {
    const text = draft[port] ?? "";
    if (text.trim() === "" && type !== "signal") return [];
    return [{ port, value: parseInputValue(type, text) }];
  });
  const bad = typed.filter((entry) => entry.value === undefined).map((entry) => entry.port);
  const ready = typed.filter((entry) => entry.value !== undefined);

  function submit(event: FormEvent) {
    event.preventDefault();
    if (bad.length > 0) return;
    onAnswer(invocation, Object.fromEntries(ready.map((entry) => [entry.port, entry.value])));
    setDraft({});
  }

  return (
    <form className="port-waiting" onSubmit={submit}>
      <span className="eyebrow">WAITING ON YOU</span>
      <p className="port-waiting-prompt">{invocation.prompt}</p>

      {effects.map(([port, type]) => (
        <label key={port} className={`port-field${bad.includes(port) ? " invalid" : ""}`}>
          <span className="port-field-name">{port}</span>
          <span className="port-field-type">{type}</span>
          <input
            value={draft[port] ?? ""}
            onChange={(event) =>
              setDraft((current) => ({ ...current, [port]: event.target.value }))
            }
            placeholder={type}
            aria-label={`${port} (${type})`}
            aria-invalid={bad.includes(port) || undefined}
          />
          {bad.includes(port) ? (
            <span className="port-field-problem">not a {type}</span>
          ) : null}
        </label>
      ))}

      {/* The contract decides whether leaving a field empty is allowed, and the
          runtime is the one that decides it — so this sends and reports what
          came back rather than second-guessing here. */}
      <p className="port-waiting-contract">contract: {invocation.contract}</p>
      <button type="submit" className="primary-button" disabled={sending || bad.length > 0}>
        {sending ? "Sending…" : `Send ${ready.length || ""}`.trim()}
      </button>
    </form>
  );
}
