"use client";

import { FormEvent, useState } from "react";

import {
  formatDuration,
  parseInputValue,
  type DiagramSnapshot,
  type PendingInvocation,
} from "./lib/protocol";

/**
 * Every port's value, and the ports a web agent is waiting to be given.
 *
 * Two halves of one question. Reading is free: the diagram stream already
 * carries every port's value and the tag it arrived at, so this is a list of
 * what the run already told us. Writing goes back through the reaction that
 * asked, so an operator's answer is recorded dataflow rather than a side effect
 * on the run.
 *
 * What can be set is not a setting of this panel. It is whatever the program
 * wired to a `Web` agent, which is why an operator cannot reach a port the
 * topology never offered them.
 */
export function PortPanel({
  snapshot,
  pending,
  sending,
  onAnswer,
}: {
  snapshot: DiagramSnapshot;
  pending: PendingInvocation[];
  sending: boolean;
  onAnswer: (invocation: PendingInvocation, values: Record<string, unknown>) => void;
}) {
  return (
    <div className="port-panel" role="tabpanel">
      {pending.map((invocation) => (
        <Waiting
          key={invocation.invocation_id}
          invocation={invocation}
          sending={sending}
          onAnswer={onAnswer}
        />
      ))}

      <div className="port-list">
        <span className="eyebrow">PORTS</span>
        {snapshot.ports.map((port) => (
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
      <div className="port-waiting-head">
        <span className="eyebrow">WAITING ON YOU</span>
        <h3>{invocation.agent}</h3>
      </div>
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
