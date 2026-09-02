// Generated from the Rust wire types by `cargo test protocol`. Do not edit.
//
// The runtime is the single definition of what the protocol is. Editing this
// file makes the client disagree with the daemon, which is the failure the
// generator exists to make impossible.

/** Every status `omar serve` can report a run in. */
export const RUN_STATUSES = ["starting", "running", "stopping", "completed", "stopped", "failed"] as const;
export type RunStatus = (typeof RUN_STATUSES)[number];

/** Where a drawing stands. `ready` is compiled but never run, which is what a proposal's preview is. */
export const DIAGRAM_STATUSES = ["ready", "running", "completed", "failed"] as const;
export type DiagramStatus = (typeof DIAGRAM_STATUSES)[number];

/** Where one reaction stands. */
export const REACTION_STATUSES = ["idle", "running", "completed"] as const;
export type ReactionStatus = (typeof REACTION_STATUSES)[number];

/** What an edge means, which decides how it is drawn. */
export const EDGE_KINDS = ["connection", "trigger", "effect"] as const;
export type EdgeKind = (typeof EDGE_KINDS)[number];

/** What a port is for. */
export const PORT_KINDS = ["input", "output", "action"] as const;
export type PortKind = (typeof PORT_KINDS)[number];

/** What happened, as the event stream names it. */
export const DIAGRAM_EVENT_KINDS = ["run_started", "tag_advanced", "reaction_started", "reaction_completed", "run_completed", "run_failed"] as const;
export type DiagramEventKind = (typeof DIAGRAM_EVENT_KINDS)[number];

/** Who spoke. */
export const CHAT_ROLES = ["operator", "assistant"] as const;
export type ChatRole = (typeof CHAT_ROLES)[number];

export type DiagramTag = { timestamp: number, microstep: number, };

export type DiagramInstance = { id: string, name: string, 
/**
 * The team it was instantiated from.
 */
team: string, 
/**
 * The container this one is drawn inside, or empty for a top-level one.
 *
 * A team can instantiate another team, so containers nest. The id is
 * given rather than the name so a client never has to re-derive one from
 * the other.
 */
parent: string, };

export type DiagramAgent = { id: string, name: string, backend: string, 
/**
 * The container it is drawn inside.
 */
instance: string, };

export type DiagramPort = { id: string, name: string, kind: PortKind, type: string, delay: number | null, value: unknown | null, last_tag: DiagramTag | null, instance: string, };

export type DiagramTimer = { id: string, name: string, offset: number, 
/**
 * `0` fires once; anything else re-arms forever.
 */
period: number, last_tag: DiagramTag | null, instance: string, };

export type DiagramReaction = { id: string, name: string, agent: string, order: number, triggers: Array<string>, effects: Array<string>, contract: string, status: ReactionStatus, invocation_id: string | null, instance: string, 
/**
 * Nanoseconds this reaction gave itself, or `None` for one bounded only by
 * the run. Carried so a client can draw the bound rather than leaving the
 * operator to read it out of the source.
 */
within: number | null, };

export type DiagramEdge = { id: string, kind: EdgeKind, source: string, target: string, 
/**
 * Null when the hop costs nothing. `0` is `after 0`, which costs a
 * microstep; larger values are nanoseconds. Trigger and effect edges
 * carry no delay of their own and are always null.
 */
delay: number | null, };

export type DiagramSnapshot = { protocol_version: number, team: string, sequence: number, status: DiagramStatus, current_tag: DiagramTag | null, 
/**
 * Nanoseconds physical time had run past `current_tag` when it executed.
 * `None` before the first tag. A sequence number says how much has been
 * published; this says whether the run is keeping its promises.
 */
lag: number | null, 
/**
 * The containers to draw. Empty for a program compiled before instances
 * were carried through, which is how a client tells the difference.
 */
instances: Array<DiagramInstance>, agents: Array<DiagramAgent>, ports: Array<DiagramPort>, 
/**
 * Empty for a program with no timer, and for bytecode that predates them.
 */
timers: Array<DiagramTimer>, reactions: Array<DiagramReaction>, edges: Array<DiagramEdge>, };

export type DiagramEvent = { protocol_version: number, sequence: number, team: string, tag: DiagramTag | null, kind: DiagramEventKind, payload: Record<string, unknown>, };

export type ProposedDesign = { program: string, inputs: Record<string, unknown>, 
/**
 * The compiled topology, so the operator sees what they are approving
 * before any run exists.
 */
preview: DiagramSnapshot, };

export type ChatMessage = { sequence: number, role: ChatRole, text: string, 
/**
 * Commentary while working, rather than something awaiting an answer.
 */
progress: boolean, design: ProposedDesign | null, 
/**
 * Diagram components the operator had selected when they sent this.
 * "this one" is unresolvable in text; a selection says which.
 */
selection: Array<string>, };

export type RunRecord = { run_id: string, team: string, status: RunStatus, diagram_address: string | null, started_at: number, finished_at: number | null, error: string | null, };

