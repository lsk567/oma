import {
  assertChatMessage,
  assertDiagramSnapshot,
  assertRunRecord,
  type ChatMessage,
  type DiagramEvent,
  type DiagramSnapshot,
  type PendingInvocation,
  SERVE_PROTOCOL_VERSION,
  type RunRecord,
  type RunRequest,
} from "./protocol";

export function normalizeRuntimeUrl(value: string): string {
  const parsed = new URL(value.trim());
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    throw new Error("Runtime URL must use HTTP or HTTPS.");
  }
  return parsed.toString().replace(/\/$/, "");
}

/**
 * The WebSocket a terminal attaches over.
 *
 * The scheme follows the daemon's: an https page cannot open a ws:// socket,
 * the same rule that already governs the diagram's plain-HTTP calls.
 */
export function terminalUrlFor(serveUrl: string, agent: string): string {
  const base = new URL(normalizeRuntimeUrl(serveUrl));
  base.protocol = base.protocol === "https:" ? "wss:" : "ws:";
  const root = base.toString().replace(/\/$/, "");
  // The assistant is not one of the agents — its tmux session is named
  // differently — so it has a route of its own.
  if (agent === ASSISTANT) return `${root}/v1/agent/terminal`;
  // An agent name is instance-qualified, so it has to survive the path.
  return `${root}/v1/agents/${encodeURIComponent(agent)}/terminal`;
}

/** Stands for the assistant wherever an agent name is expected. */
export const ASSISTANT = "\u0000assistant";

/** What the assistant runs on, and what else it could run on. */
export type AgentBackends = { backend: string | null; available: string[] };

export async function fetchBackends(serveUrl: string): Promise<AgentBackends> {
  const base = normalizeRuntimeUrl(serveUrl);
  const response = await fetch(`${base}/v1/agent`);
  if (!response.ok) throw new Error(await readError(response));
  const body = (await response.json()) as Partial<AgentBackends>;
  return {
    backend: typeof body.backend === "string" ? body.backend : null,
    available: Array.isArray(body.available) ? body.available : [],
  };
}

/**
 * Move the assistant to another backend.
 *
 * A backend is chosen when the process starts, so this restarts it and the
 * assistant's current session does not survive.
 */
export async function switchBackend(
  serveUrl: string,
  backend: string,
): Promise<void> {
  const base = normalizeRuntimeUrl(serveUrl);
  const response = await fetch(`${base}/v1/agent/backend`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ backend }),
  });
  if (!response.ok) throw new Error(await readError(response));
}

async function readError(response: Response): Promise<string> {
  // `omar serve` reports compile and validation failures as {"error": "..."};
  // surfacing that beats a bare status code for a rejected design.
  try {
    const body = (await response.json()) as { error?: unknown };
    if (typeof body.error === "string" && body.error.length > 0) {
      return body.error;
    }
  } catch {
    // fall through to the status code
  }
  return `Runtime returned HTTP ${response.status}.`;
}

/** Send an operator message to the EA. Replies arrive on the chat stream. */
export async function sendChat(
  serveUrl: string,
  text: string,
  selection: string[] = [],
  signal?: AbortSignal,
): Promise<void> {
  const base = normalizeRuntimeUrl(serveUrl);
  const response = await fetch(`${base}/v1/chat`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ text, selection }),
    signal,
  });
  if (!response.ok) {
    throw new Error(await readError(response));
  }
}

/**
 * Subscribe to the operator/EA conversation. The stream replays history on
 * connect, so a reload does not lose the thread.
 */
export function subscribeToChat(
  serveUrl: string,
  onMessage: (message: ChatMessage) => void,
  onConnectionChange: (connected: boolean) => void,
): () => void {
  const stream = new EventSource(`${normalizeRuntimeUrl(serveUrl)}/v1/chat/events`);
  stream.onopen = () => onConnectionChange(true);
  stream.onerror = () => onConnectionChange(false);
  for (const kind of ["message", "design_proposed"]) {
    stream.addEventListener(kind, (raw) => {
      onMessage(assertChatMessage(JSON.parse((raw as MessageEvent<string>).data)));
    });
  }
  return () => stream.close();
}

/**
 * Ask `omar serve` whether it is up and speaking a protocol we understand.
 * A version mismatch is reported rather than discovered later on a failed run.
 */
export async function checkServeHealth(
  serveUrl: string,
  signal?: AbortSignal,
): Promise<{ ok: true } | { ok: false; reason: string }> {
  try {
    const base = normalizeRuntimeUrl(serveUrl);
    const response = await fetch(`${base}/health`, { signal });
    if (!response.ok) {
      return { ok: false, reason: `HTTP ${response.status}` };
    }
    const body = (await response.json()) as { protocol_version?: unknown };
    if (body.protocol_version !== SERVE_PROTOCOL_VERSION) {
      return {
        ok: false,
        reason: `serve speaks protocol ${String(body.protocol_version)}, this client speaks ${SERVE_PROTOCOL_VERSION}`,
      };
    }
    return { ok: true };
  } catch (cause) {
    return { ok: false, reason: cause instanceof Error ? cause.message : String(cause) };
  }
}

/**
 * Hand a confirmed design to `omar serve`, which compiles it and starts the
 * run. The returned `diagram_address` is where that run's live topology lives.
 */
/** One logical tag of a projected run. */
export type ProjectedStep = {
  timestamp: number;
  microstep: number;
  /** Ports and timers carrying a value at this tag. */
  events: string[];
  /** Reactions that fire, in the order they would run. */
  reactions: string[];
};

export type Projection = {
  ok: boolean;
  team?: string;
  open_inputs?: string[];
  steps?: ProjectedStep[];
  /** The projection stopped early; the program has not. */
  truncated?: boolean;
  /** The topology as compiled, so the drawing can follow the text. */
  preview?: DiagramSnapshot;
  errors?: string[];
};

/**
 * What a program would do, worked out without running it.
 *
 * Computed by the runtime rather than here: the rules that decide a tag —
 * fixed port delays, connection delays, superdense microsteps — belong to the
 * event loop, and a second implementation in this language would drift from it
 * silently.
 */
export async function projectProgram(
  serveUrl: string,
  program: string,
  filename: string,
  present: string[],
  signal?: AbortSignal,
): Promise<Projection> {
  const base = normalizeRuntimeUrl(serveUrl);
  const response = await fetch(`${base}/v1/programs/project`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ program, filename, present }),
    signal,
  });
  if (!response.ok) {
    throw new Error(await readError(response));
  }
  return (await response.json()) as Projection;
}

export async function startRun(
  serveUrl: string,
  request: RunRequest,
  signal?: AbortSignal,
): Promise<RunRecord> {
  const base = normalizeRuntimeUrl(serveUrl);
  const response = await fetch(`${base}/v1/runs`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(request),
    signal,
  });
  if (!response.ok) {
    throw new Error(await readError(response));
  }
  return assertRunRecord(await response.json());
}

/**
 * What the run's web agents are waiting on.
 *
 * The client learns *that* something is waiting from the diagram's
 * `reaction_started`; this is where it learns what. 404 means the program has
 * no `Web` agent, or the run is over — neither is an error worth showing, so
 * callers read an empty list.
 */
export async function fetchPanel(
  serveUrl: string,
  runId: string,
  signal?: AbortSignal,
): Promise<PendingInvocation[]> {
  const base = normalizeRuntimeUrl(serveUrl);
  const response = await fetch(
    `${base}/v1/runs/${encodeURIComponent(runId)}/panel`,
    { signal },
  );
  if (response.status === 404) return [];
  if (!response.ok) throw new Error(await readError(response));
  const body = (await response.json()) as { pending?: unknown };
  return Array.isArray(body.pending) ? (body.pending as PendingInvocation[]) : [];
}

/**
 * Answer one invocation. Every value lands as one completion, so a reaction
 * reading several of these ports sees them together rather than firing once
 * per value — which is why the panel has a Send button rather than committing
 * a field at a time.
 */
export async function answerPanel(
  serveUrl: string,
  runId: string,
  answer: { invocation_id: string; agent: string; values: Record<string, unknown> },
  signal?: AbortSignal,
): Promise<void> {
  const base = normalizeRuntimeUrl(serveUrl);
  const response = await fetch(
    `${base}/v1/runs/${encodeURIComponent(runId)}/panel`,
    {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(answer),
      signal,
    },
  );
  if (!response.ok) throw new Error(await readError(response));
}

export async function fetchRun(
  serveUrl: string,
  runId: string,
  signal?: AbortSignal,
): Promise<RunRecord> {
  const base = normalizeRuntimeUrl(serveUrl);
  const response = await fetch(`${base}/v1/runs/${encodeURIComponent(runId)}`, {
    signal,
  });
  if (!response.ok) {
    throw new Error(await readError(response));
  }
  return assertRunRecord(await response.json());
}

/** `omar serve` reports `host:port`; the diagram API is plain HTTP on loopback. */
export function diagramUrlFor(record: RunRecord): string {
  if (!record.diagram_address) {
    throw new Error(`Run ${record.run_id} has no diagram address yet.`);
  }
  return `http://${record.diagram_address}`;
}

export async function fetchDiagram(
  runtimeUrl: string,
  signal?: AbortSignal,
): Promise<DiagramSnapshot> {
  const base = normalizeRuntimeUrl(runtimeUrl);
  const response = await fetch(`${base}/v1/diagram`, { signal });
  if (!response.ok) {
    throw new Error(`Runtime returned HTTP ${response.status}.`);
  }
  return assertDiagramSnapshot(await response.json());
}

export function subscribeToDiagram(
  runtimeUrl: string,
  onEvent: (event: DiagramEvent) => void,
  onConnectionChange: (connected: boolean) => void,
): () => void {
  const stream = new EventSource(
    `${normalizeRuntimeUrl(runtimeUrl)}/v1/events`,
  );
  stream.onopen = () => onConnectionChange(true);
  stream.onerror = () => onConnectionChange(false);

  const kinds: DiagramEvent["kind"][] = [
    "run_started",
    "tag_advanced",
    "reaction_started",
    "reaction_completed",
    "run_completed",
    "run_failed",
  ];
  for (const kind of kinds) {
    stream.addEventListener(kind, (raw) => {
      onEvent(JSON.parse((raw as MessageEvent<string>).data) as DiagramEvent);
    });
  }
  return () => stream.close();
}
