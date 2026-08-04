export const DIAGRAM_PROTOCOL_VERSION = 1;

/** Version of the `omar serve` run-admission API, distinct from the diagram protocol. */
export const SERVE_PROTOCOL_VERSION = 1;

export type DiagramTag = {
  timestamp: number;
  microstep: number;
};

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
  delay: number;
};

export type DiagramSnapshot = {
  protocol_version: number;
  team: string;
  sequence: number;
  status: "ready" | "running" | "completed" | "failed";
  current_tag: DiagramTag | null;
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

/** A run as `omar serve` reports it. `diagram_address` is host:port, not a URL. */
export type RunRecord = {
  run_id: string;
  team: string;
  status: "starting" | "running" | "completed" | "failed";
  diagram_address: string | null;
  started_at: number;
  finished_at: number | null;
  error: string | null;
};

export type RunRequest = {
  program: string;
  inputs: Record<string, unknown>;
};

export function isRunFinished(status: RunRecord["status"]): boolean {
  return status === "completed" || status === "failed";
}

export function assertRunRecord(value: unknown): RunRecord {
  if (!value || typeof value !== "object") {
    throw new Error("Run response is not an object.");
  }
  const record = value as Partial<RunRecord>;
  if (typeof record.run_id !== "string" || typeof record.team !== "string") {
    throw new Error("Run response is missing required run fields.");
  }
  if (
    record.status !== "starting" &&
    record.status !== "running" &&
    record.status !== "completed" &&
    record.status !== "failed"
  ) {
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
      return {
        ...snapshot,
        current_tag: event.tag ?? snapshot.current_tag,
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
