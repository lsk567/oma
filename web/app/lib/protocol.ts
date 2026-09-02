export const DIAGRAM_PROTOCOL_VERSION = 1;

/** Version of the `omar serve` run-admission API, distinct from the diagram protocol. */
export const SERVE_PROTOCOL_VERSION = 1;

export type DiagramTag = {
  /** Nanoseconds of logical time since the run began. */
  timestamp: number;
  microstep: number;
};

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

export type DiagramAgent = {
  id: string;
  name: string;
  backend: string;
  instance: string;
};

export type DiagramPort = {
  id: string;
  name: string;
  kind: "input" | "output" | "action";
  type: string;
  delay: number | null;
  value: unknown | null;
  last_tag: DiagramTag | null;
  /** The container it is drawn in. Empty for programs compiled before
      instances were carried through the protocol. */
  instance: string;
};

export type DiagramReaction = {
  id: string;
  name: string;
  agent: string;
  order: number;
  triggers: string[];
  effects: string[];
  contract: string;
  status: "idle" | "running" | "completed";
  invocation_id: string | null;
  instance: string;
  /**
   * Nanoseconds this reaction gave itself with `within`, or null for one
   * bounded only by the run. Null also for a runtime that predates the field.
   */
  within: number | null;
};

/**
 * A trigger the runtime fires from its own logical clock.
 *
 * Not a port: nothing feeds it, so it is drawn as a clock rather than as an
 * inlet with no source. `last_tag` is when it last fired — the schedule as the
 * runtime actually ran it, rather than one the client extrapolates.
 */
export type DiagramTimer = {
  id: string;
  name: string;
  offset: number;
  /** 0 fires once; anything else re-arms forever. */
  period: number;
  last_tag: DiagramTag | null;
  instance: string;
};

/** A team `main` instantiated: one container in the drawing. */
export type DiagramInstance = {
  id: string;
  name: string;
  /** The team it came from. */
  team: string;
  /**
   * The container this one is drawn inside, as an id, or empty at the top
   * level. A team can instantiate another team, so containers nest.
   */
  parent: string;
};

export type DiagramEdge = {
  id: string;
  kind: "connection" | "trigger" | "effect";
  source: string;
  target: string;
  /**
   * What the hop costs. Null for a plain connection, whose value is readable
   * at the tag it was written; 0 for `after 0`, which costs a microstep; and
   * nanoseconds above that. Trigger and effect edges are always null.
   */
  delay: number | null;
};

export type DiagramSnapshot = {
  protocol_version: number;
  team: string;
  sequence: number;
  status: "ready" | "running" | "completed" | "failed";
  current_tag: DiagramTag | null;
  /**
   * Nanoseconds physical time had run past `current_tag` when it executed.
   * Null before the first tag, and from a runtime that predates the field.
   *
   * Zero means the run is on or ahead of its schedule. It grows only when work
   * outlasts the gap the program gave it, which is the question an operator
   * actually has: is this keeping up?
   */
  lag: number | null;
  /** Containers to draw. Empty means the runtime predates instances, and the
      program is drawn as one box named after itself. */
  instances: DiagramInstance[];
  agents: DiagramAgent[];
  ports: DiagramPort[];
  /** Empty for a program with no timer, and for runtimes that predate them. */
  timers: DiagramTimer[];
  reactions: DiagramReaction[];
  edges: DiagramEdge[];
};

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

export type DiagramEvent = {
  protocol_version: number;
  sequence: number;
  team: string;
  tag: DiagramTag | null;
  kind:
    | "run_started"
    | "tag_advanced"
    | "reaction_started"
    | "reaction_completed"
    | "run_completed"
    | "run_failed";
  payload: Record<string, unknown>;
};

/** A program the EA has proposed. Nothing runs until the operator approves it. */
export type ProposedDesign = {
  program: string;
  inputs: Record<string, unknown>;
  /** Compiled by the runtime, so the operator sees what they are approving. */
  preview: DiagramSnapshot;
};

/** One turn of the operator/EA conversation, as `omar serve` relays it. */
export type ChatMessage = {
  sequence: number;
  role: "operator" | "assistant";
  text: string;
  /** Commentary while the assistant works, rather than a reply awaiting you. */
  progress: boolean;
  design: ProposedDesign | null;
  /** Diagram components the operator had selected when they sent this. */
  selection: string[];
};

export function assertChatMessage(value: unknown): ChatMessage {
  if (!value || typeof value !== "object") {
    throw new Error("Chat message is not an object.");
  }
  const message = value as Partial<ChatMessage>;
  if (typeof message.sequence !== "number" || typeof message.text !== "string") {
    throw new Error("Chat message is missing required fields.");
  }
  if (message.role !== "operator" && message.role !== "assistant") {
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

/**
 * Every status `omar serve` can report, as one list the type and the runtime
 * check are both built from.
 *
 * They used to be two hand-written copies, which is how `stopped` came to be a
 * status the daemon sends and the client rejects: the union was updated and the
 * check below was not. One list cannot disagree with itself.
 */
export const RUN_STATUSES = [
  "starting",
  "running",
  "stopping",
  "completed",
  "stopped",
  "failed",
] as const;

export type RunStatus = (typeof RUN_STATUSES)[number];

/** A run as `omar serve` reports it. `diagram_address` is host:port, not a URL. */
export type RunRecord = {
  run_id: string;
  team: string;
  status: RunStatus;
  diagram_address: string | null;
  started_at: number;
  finished_at: number | null;
  error: string | null;
};

export type RunRequest = {
  program: string;
  inputs: Record<string, unknown>;
};

/**
 * Whether the run is over, however it ended. `stopped` is an ending like any
 * other -- it is what a stop asked for, not a failure to be reported.
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
