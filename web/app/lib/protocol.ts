/**
 * What the client does with the protocol. What the protocol *is* now lives in
 * `protocol-generated.ts`, emitted from the Rust wire types by
 * `cargo test protocol`.
 *
 * They used to be one hand-written file per language with nothing holding them
 * together, and they drifted: `stopped` is a status the daemon has answered for
 * as long as `RunEnd::Stopped` has existed, and it appeared in neither the
 * union here nor the check below.
 *
 * Re-exported rather than replaced, so every existing import still resolves
 * from here and the split is invisible to callers.
 */
export type {
  ChatMessage,
  ChatRole,
  DiagramAgent,
  DiagramEdge,
  DiagramEvent,
  DiagramEventKind,
  DiagramInstance,
  DiagramPort,
  DiagramReaction,
  DiagramSnapshot,
  DiagramStatus,
  DiagramTag,
  DiagramTimer,
  EdgeKind,
  PortKind,
  ProposedDesign,
  ReactionStatus,
  RunRecord,
  RunStatus,
} from "./protocol-generated";

export {
  CHAT_ROLES,
  DIAGRAM_EVENT_KINDS,
  DIAGRAM_STATUSES,
  EDGE_KINDS,
  PORT_KINDS,
  REACTION_STATUSES,
  RUN_STATUSES,
} from "./protocol-generated";

import { CHAT_ROLES, RUN_STATUSES } from "./protocol-generated";
import type {
  ChatMessage,
  DiagramEvent,
  DiagramPort,
  DiagramSnapshot,
  RunRecord,
  RunStatus,
} from "./protocol-generated";

export const DIAGRAM_PROTOCOL_VERSION = 1;

/** Version of the `omar serve` run-admission API, distinct from the diagram protocol. */
export const SERVE_PROTOCOL_VERSION = 1;

/** Largest unit that still divides a whole number of nanoseconds. */
const DURATION_UNITS: [number, string][] = [
  [3_600_000_000_000, "h"],
  [60_000_000_000, "min"],
  [1_000_000_000, "s"],
  [1_000_000, "ms"],
  [1_000, "us"],
  [1, "ns"],
];

/**
 * A span of nanoseconds, in the largest unit that divides it exactly.
 *
 * `30000000000` is a number nobody can read at a glance and everybody has to
 * count the digits of. `30s` is the same fact. Exact division rather than
 * rounding, because `1500ms` is true where `1.5s` invites a reader to wonder
 * what was dropped — and a diagram that rounds a delay is lying about the
 * program.
 */
export function formatDuration(nanos: number): string {
  if (!Number.isFinite(nanos)) return "—";
  if (nanos === 0) return "0";
  if (nanos < 0) return `-${formatDuration(-nanos)}`;
  const [scale, unit] =
    DURATION_UNITS.find(([size]) => nanos % size === 0) ??
    DURATION_UNITS[DURATION_UNITS.length - 1];
  return `${nanos / scale}${unit}`;
}

/** One invocation a `Web` agent is waiting on, as the daemon reports it. */
export type PendingInvocation = {
  invocation_id: string;
  agent: string;
  reaction: string;
  contract: string;
  /** Already interpolated, so this is the instruction an agent would read. */
  prompt: string;
  trigger_values: Record<string, unknown>;
  /** Port name to declared type. Exactly what this invocation may write. */
  allowed_effects: Record<string, string>;
};

/**
 * Inputs nothing inside the topology writes to.
 *
 * A diagnostic, matching the daemon's own. A program that closes its loop has
 * none; one that has some has a port nothing will ever drive. The projection
 * treats them as arriving, because the question it answers is what the program
 * would do if they did.
 */
export function openInputs(snapshot: DiagramSnapshot): DiagramPort[] {
  const fed = new Set(
    snapshot.edges.filter((edge) => edge.kind === "connection").map((edge) => edge.target),
  );
  return snapshot.ports.filter((port) => port.kind === "input" && !fed.has(port.id));
}

/**
 * Turn what an operator typed into a value of the port's type.
 *
 * The runtime checks a value against its port before it reaches the run, and it
 * wants JSON: a number for `int`, a boolean for `bool`, null for a signal.
 * Sending the raw text would fail every port that is not a string, with an
 * error about a type the operator never chose.
 *
 * Returns `undefined` when the text cannot be read as the type, which the panel
 * shows as a problem with that field rather than sending and being refused.
 */
export function parseInputValue(type: string, text: string): unknown | undefined {
  const trimmed = text.trim();
  switch (type) {
    case "string":
    case "path":
    case "bytes":
      // Not trimmed: whitespace can be part of what was meant.
      return text;
    case "signal":
      return null;
    case "bool":
      if (trimmed === "true") return true;
      if (trimmed === "false") return false;
      return undefined;
    case "int":
      return /^-?\d+$/.test(trimmed) ? Number(trimmed) : undefined;
    case "float": {
      const value = Number(trimmed);
      return trimmed !== "" && Number.isFinite(value) ? value : undefined;
    }
    default:
      // `list<int>`, `option<string>` and friends are given as JSON.
      try {
        return JSON.parse(trimmed);
      } catch {
        return undefined;
      }
  }
}

/**
 * The agents a client answers for: those the program put on the `web` backend.
 *
 * Read off the snapshot rather than asked for, so a client knows a panel exists
 * from the same drawing everyone else sees.
 */
export function webAgents(snapshot: DiagramSnapshot): Set<string> {
  return new Set(
    snapshot.agents
      .filter((agent) => agent.backend.toLowerCase() === "web")
      .map((agent) => agent.id),
  );
}

export function assertChatMessage(value: unknown): ChatMessage {
  if (!value || typeof value !== "object") {
    throw new Error("Chat message is not an object.");
  }
  const message = value as Partial<ChatMessage>;
  if (typeof message.sequence !== "number" || typeof message.text !== "string") {
    throw new Error("Chat message is missing required fields.");
  }
  if (!CHAT_ROLES.includes(message.role as (typeof CHAT_ROLES)[number])) {
    throw new Error(`Unsupported chat role ${String(message.role)}.`);
  }
  const progress = message.progress === true;
  // Older daemons predate selection; an absent one is simply none.
  const selection = Array.isArray(message.selection)
    ? message.selection.filter((name): name is string => typeof name === "string")
    : [];
  // Keep what the assertion normalises. Calling it only for its throw left the
  // preview without the defaults every other snapshot gets, so a design from a
  // runtime that predates a field reached the renderer missing it.
  let design = message.design ?? null;
  if (design !== null) {
    if (typeof design.program !== "string") {
      throw new Error("Proposed design is missing its program.");
    }
    design = { ...design, preview: assertDiagramSnapshot(design.preview) };
  }
  return { ...message, design, progress, selection } as ChatMessage;
}

export type RunRequest = {
  program: string;
  inputs: Record<string, unknown>;
};

/**
 * Whether the run is over, however it ended.
 *
 * `stopped` is an ending like any other — it is what a stop asked for, not a
 * failure. It was missing here for as long as the daemon could answer it.
 */
export function isRunFinished(status: RunStatus): boolean {
  return status === "completed" || status === "stopped" || status === "failed";
}

export function assertRunRecord(value: unknown): RunRecord {
  if (!value || typeof value !== "object") {
    throw new Error("Run response is not an object.");
  }
  const record = value as Partial<RunRecord>;
  if (typeof record.run_id !== "string" || typeof record.team !== "string") {
    throw new Error("Run response is missing required run fields.");
  }
  // Checked against the generated list, so a status the daemon adds cannot be
  // one the client rejects. This is why the generator emits a runtime array and
  // not only a type: a type would have been erased before this ran.
  if (!RUN_STATUSES.includes(record.status as RunStatus)) {
    throw new Error(`Unsupported run status ${String(record.status)}.`);
  }
  return record as RunRecord;
}

export function assertDiagramSnapshot(value: unknown): DiagramSnapshot {
  if (!value || typeof value !== "object") {
    throw new Error("Diagram response is not an object.");
  }
  const snapshot = value as Partial<DiagramSnapshot>;
  if (snapshot.protocol_version !== DIAGRAM_PROTOCOL_VERSION) {
    throw new Error(
      `Unsupported diagram protocol ${String(snapshot.protocol_version)}.`,
    );
  }
  if (
    typeof snapshot.team !== "string" ||
    !Array.isArray(snapshot.ports) ||
    !Array.isArray(snapshot.reactions) ||
    !Array.isArray(snapshot.edges)
  ) {
    throw new Error("Diagram response is missing required topology fields.");
  }
  // Older runtimes send no instances; those programs draw as one container
  // named after the program, which is what they were before.
  return {
    ...snapshot,
    instances: Array.isArray(snapshot.instances) ? snapshot.instances : [],
    // Likewise for timers: a runtime without them sends no field at all.
    timers: Array.isArray(snapshot.timers) ? snapshot.timers : [],
    // A runtime that does not measure lag is not a runtime with no lag, so
    // this stays null and the readout says so rather than claiming zero.
    lag: typeof snapshot.lag === "number" ? snapshot.lag : null,
    // Likewise `within`: a reaction that declares no deadline, and one from a
    // runtime that predates them, both arrive without the field. The type says
    // `number | null`, so make that true rather than leaving `undefined` for
    // every reader to guard against.
    reactions: (snapshot.reactions ?? []).map((reaction) => ({
      ...reaction,
      within: typeof reaction.within === "number" ? reaction.within : null,
    })),
  } as DiagramSnapshot;
}

/**
 * Fold a live event into the snapshot it belongs to.
 *
 * The per-run diagram server dies with the run, so the refetch that follows the
 * last events can lose the race and leave a reaction painted as running for
 * good. Applying the event directly means the picture is right whether or not
 * the server is still there to ask.
 */
export function applyDiagramEvent(
  snapshot: DiagramSnapshot,
  event: DiagramEvent,
): DiagramSnapshot {
  const reactionId = (event.payload as { reaction?: unknown }).reaction;
  const withReaction = (
    id: unknown,
    change: Partial<DiagramSnapshot["reactions"][number]>,
  ) =>
    snapshot.reactions.map((reaction) =>
      reaction.id === id ? { ...reaction, ...change } : reaction,
    );

  switch (event.kind) {
    case "reaction_started":
      return {
        ...snapshot,
        current_tag: event.tag ?? snapshot.current_tag,
        reactions: withReaction(reactionId, { status: "running" }),
      };
    case "reaction_completed":
      return {
        ...snapshot,
        current_tag: event.tag ?? snapshot.current_tag,
        reactions: withReaction(reactionId, {
          status: "completed",
          invocation_id: null,
        }),
      };
    case "tag_advanced": {
      // A timer arrives in the same map as the ports; it is the one that fired
      // at this tag. Reading it from the event rather than refetching is what
      // keeps the clock moving after the run's server has gone.
      const fired = (event.payload?.ports ?? {}) as Record<string, unknown>;
      const lag = (event.payload as { lag?: unknown }).lag;
      return {
        ...snapshot,
        current_tag: event.tag ?? snapshot.current_tag,
        // Carried on the event as well as the snapshot, so the reading stays
        // right after the run's server has gone and there is nothing to refetch.
        lag: typeof lag === "number" ? lag : snapshot.lag,
        timers: snapshot.timers.map((timer) =>
          timer.name in fired ? { ...timer, last_tag: event.tag } : timer,
        ),
      };
    }
    case "run_completed":
      // The run cannot have completed with a reaction still in flight, so
      // anything left running did finish, whatever the last fetch saw.
      return {
        ...snapshot,
        status: "completed",
        reactions: snapshot.reactions.map((reaction) =>
          reaction.status === "running"
            ? { ...reaction, status: "completed", invocation_id: null }
            : reaction,
        ),
      };
    case "run_failed":
      // A reaction interrupted by the failure did not complete, so it goes back
      // to idle rather than claiming a result it never produced.
      return {
        ...snapshot,
        status: "failed",
        reactions: snapshot.reactions.map((reaction) =>
          reaction.status === "running"
            ? { ...reaction, status: "idle", invocation_id: null }
            : reaction,
        ),
      };
    default:
      return snapshot;
  }
}
