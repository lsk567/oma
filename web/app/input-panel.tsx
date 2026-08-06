"use client";

import { FormEvent, useEffect, useRef, useState } from "react";

import { parseInputValue, type DiagramPort } from "./lib/protocol";

/**
 * The ports the operator sets, and the one act of setting them.
 *
 * Deploying a program and deciding what to feed it are separate: the run comes
 * up, spawns its agents, and then sits at its first tag with nothing to do. This
 * is where that changes. Everything typed here is sent in one message and lands
 * at one tag, so a reaction reading several of these ports sees them together
 * rather than firing once per value — which is why there is a Send button
 * rather than a field that commits on blur.
 *
 * It slides in over the diagram rather than replacing it: the operator picked
 * these ports by clicking them, and losing sight of where they are would make
 * a long list of qualified names hard to place.
 */
export function InputPanel({
  ports,
  focused,
  pending,
  onClose,
  onSend,
}: {
  ports: DiagramPort[];
  /** The id of the port the operator clicked, scrolled to and highlighted. */
  focused: string | null;
  pending: boolean;
  onClose: () => void;
  onSend: (values: Record<string, unknown>) => void;
}) {
  const [draft, setDraft] = useState<Record<string, string>>({});
  const focusedRef = useRef<HTMLInputElement | null>(null);

  // Clicking a port on the diagram while the panel is open moves the focus
  // here rather than opening a second panel, so a long list stays navigable.
  useEffect(() => {
    if (!focused) return;
    focusedRef.current?.scrollIntoView({ block: "nearest" });
    focusedRef.current?.focus();
  }, [focused]);

  useEffect(() => {
    const dismiss = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", dismiss);
    return () => window.removeEventListener("keydown", dismiss);
  }, [onClose]);

  /**
   * What is typed, read as the type each port declares.
   *
   * The runtime checks a value against its port before it reaches the run, and
   * wants JSON — a number for an `int`, not the characters `3`. Doing that here
   * means a value that cannot be read is a field the operator can see is wrong,
   * rather than a batch the run refuses.
   */
  const typed = ports.flatMap((port) => {
    const text = draft[port.name] ?? "";
    if (text.trim() === "") return [];
    return [{ port, value: parseInputValue(port.type, text) }];
  });
  const bad = typed.filter((entry) => entry.value === undefined).map((entry) => entry.port.name);
  const ready = typed.filter((entry) => entry.value !== undefined);

  function submit(event: FormEvent) {
    event.preventDefault();
    // Only what was typed. An untouched port stays unset, and the run keeps
    // waiting for it, which is what an open input means.
    if (ready.length === 0 || bad.length > 0) return;
    onSend(Object.fromEntries(ready.map((entry) => [entry.port.name, entry.value])));
    setDraft({});
  }

  return (
    <aside className="input-panel" role="dialog" aria-label="Set input ports">
      <div className="input-panel-head">
        <div>
          <span className="eyebrow">INPUT PORTS</span>
          <h2>Set values</h2>
        </div>
        <button type="button" className="input-panel-close" onClick={onClose}>
          Close
        </button>
      </div>

      <p className="input-panel-note">
        Nothing runs until these arrive. Everything you send goes in at the same
        tag.
      </p>

      <form className="input-panel-body" onSubmit={submit}>
        {ports.map((port) => (
          <label
            key={port.id}
            className={[
              "input-port",
              port.id === focused ? "focused" : "",
              bad.includes(port.name) ? "invalid" : "",
            ]
              .filter(Boolean)
              .join(" ")}
          >
            <span className="input-port-name">{port.name}</span>
            <span className="input-port-type">{port.type}</span>
            <input
              ref={port.id === focused ? focusedRef : undefined}
              value={draft[port.name] ?? ""}
              onChange={(event) =>
                setDraft((current) => ({ ...current, [port.name]: event.target.value }))
              }
              // The value already carried is what the run has, so a port that
              // has been set once reads as set rather than empty.
              placeholder={port.value === null ? port.type : String(port.value)}
              aria-label={`${port.name} (${port.type})`}
              aria-invalid={bad.includes(port.name) || undefined}
            />
            {bad.includes(port.name) ? (
              <span className="input-port-problem">not a {port.type}</span>
            ) : null}
          </label>
        ))}
        {ports.length === 0 ? (
          <p className="input-panel-empty">
            This program has no open inputs. It starts on its own, or on a timer.
          </p>
        ) : null}
        <button
          type="submit"
          className="primary-button"
          disabled={pending || ready.length === 0 || bad.length > 0}
        >
          {pending ? "Sending…" : `Send ${ready.length || ""}`.trim()}
        </button>
      </form>
    </aside>
  );
}
