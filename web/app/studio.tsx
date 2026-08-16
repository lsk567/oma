"use client";

import { FormEvent, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ChatMessage as ChatMessageView } from "./chat-message";
import { AgentTerminal } from "./agent-terminal";
import { BackendMenu } from "./backend-menu";
import { DiagramCanvas } from "./diagram/diagram-canvas";
import { OmarEditor } from "./omar-source";
import { PortPanel } from "./port-panel";
import { Resizer } from "./resizer";
import { Waiting } from "./waiting";
import {
  eaDesignAgent,
  scriptedDesignAgent,
  type DesignAgent,
} from "./lib/design-agent";
import { reviewProgram, reviewWorkflow } from "./lib/fixtures";
import {
  applyDiagramEvent,
  formatDuration,
  isRunFinished,
  type ChatMessage,
  type DiagramEvent,
  type DiagramSnapshot,
  type PendingInvocation,
  type ProposedDesign,
  type RunRecord,
} from "./lib/protocol";
import {
  answerPanel,
  ASSISTANT,
  checkProgram,
  checkServeHealth,
  diagramUrlFor,
  fetchDiagram,
  fetchPanel,
  fetchRun,
  startRun,
  subscribeToDiagram,
} from "./lib/runtime-client";

/**
 * The operator's position in the flow. A design is never executed without
 * passing through `review`, which is the confirmation gate.
 */
type Phase =
  | "idle"
  | "drafting"
  | "review"
  | "spawning"
  | "observing"
  | "finished"
  | "failed";

/** Whether `omar serve` is reachable, tracked separately from the flow phase. */
type Daemon =
  | { state: "demo" }
  | { state: "checking" }
  | { state: "live" }
  | { state: "offline"; reason: string };

const HEALTH_POLL_MS = 5000;
/**
 * Column bounds. A panel dragged below its minimum collapses to nothing rather
 * than lingering unusably narrow; its divider becomes the control to bring it
 * back, which is why there are no separate show/hide buttons.
 */
const MIN_BUILDER = 300;
const MIN_INSPECTOR = 260;
const MIN_DIAGRAM = 320;
const DEFAULT_BUILDER = 380;
const DEFAULT_INSPECTOR = 400;
/** Drag past this fraction of a panel's minimum and it collapses. */
const COLLAPSE_AT = 0.6;

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

export function Studio({
  serveUrl = "",
  designAgent,
}: {
  serveUrl?: string;
  designAgent?: DesignAgent;
}) {
  const isDemo = serveUrl.trim().length === 0;
  const [snapshot, setSnapshot] = useState<DiagramSnapshot | null>(
    isDemo ? reviewWorkflow : null,
  );
  const [source, setSource] = useState(isDemo ? reviewProgram : "");
  /** What the program is called. Named for the team it declares until the
      operator says otherwise. */
  const [filename, setFilename] = useState(
    isDemo ? `${reviewWorkflow.team}.omar` : "program.omar",
  );
  /** What the compiler said about the source as it stands. */
  const [sourceErrors, setSourceErrors] = useState<string[]>([]);
  const [checking, setChecking] = useState(false);
  // Chat gets the whole width until there is a design to look at. The operator
  // can flip back and forth once there is.
  const [tab, setTab] = useState<"source" | "events" | "ports">("source");
  /** What the run's web agents are waiting to be given. */
  const [pending, setPending] = useState<PendingInvocation[]>([]);
  const [answering, setAnswering] = useState(false);
  const [builderWidth, setBuilderWidth] = useState(DEFAULT_BUILDER);
  const [inspectorWidth, setInspectorWidth] = useState(DEFAULT_INSPECTOR);
  // The first design splits the window down the middle. After that the widths
  // are the operator's, so this only ever fires once.
  const arrangedRef = useRef(isDemo);
  const workspaceRef = useRef<HTMLElement | null>(null);
  const dragOriginRef = useRef({
    builder: DEFAULT_BUILDER,
    inspector: DEFAULT_INSPECTOR,
  });
  // Deploying starts real agents, so the button arms a second, explicit step.
  const [confirming, setConfirming] = useState(false);
  const [daemon, setDaemon] = useState<Daemon>(
    isDemo ? { state: "demo" } : { state: "checking" },
  );
  const [phase, setPhase] = useState<Phase>("idle");
  const [design, setDesign] = useState<ProposedDesign | null>(null);
  const [run, setRun] = useState<RunRecord | null>(null);
  const [error, setError] = useState("");
  const [prompt, setPrompt] = useState("");
  /** Diagram components the operator has highlighted for the next message. */
  const [selection, setSelection] = useState<string[]>([]);
  /** The agent whose terminal is open, if any. */
  const [terminalAgent, setTerminalAgent] = useState<string | null>(null);
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [events, setEvents] = useState<DiagramEvent[]>([]);
  const disconnectRef = useRef<null | (() => void)>(null);
  const threadRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => () => disconnectRef.current?.(), []);

  // Poll the daemon so the indicator reflects reality rather than whatever was
  // true when the page loaded.
  useEffect(() => {
    if (isDemo) return;
    let cancelled = false;
    const probe = async () => {
      const health = await checkServeHealth(serveUrl);
      if (cancelled) return;
      setDaemon(health.ok ? { state: "live" } : { state: "offline", reason: health.reason });
    };
    void probe();
    const timer = setInterval(() => void probe(), HEALTH_POLL_MS);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, [isDemo, serveUrl]);

  // One agent for the lifetime of a mode. Demo mode never reaches the network.
  const agent = useMemo(
    () => designAgent ?? (isDemo ? scriptedDesignAgent() : eaDesignAgent(serveUrl)),
    [designAgent, isDemo, serveUrl],
  );

  // The conversation is owned by the runtime, not this component: the stream
  // replays history on connect, so a reload rejoins rather than starting over.
  useEffect(() => {
    const unsubscribe = agent.subscribe(
      (message) => {
        setMessages((current) =>
          current.some((seen) => seen.sequence === message.sequence)
            ? current
            : [...current, message],
        );
        if (!message.design) {
          // Only an assistant reply ends the wait — the operator's own message
          // echoes back off the same stream, and commentary while it works is
          // the opposite of finishing. And a reply must not withdraw a pending
          // proposal: assistants routinely comment straight after proposing
          // ("…is in your queue"), which was retracting the gate.
          if (message.role === "assistant" && !message.progress) {
            setPhase((current) => (current === "drafting" ? "idle" : current));
          }
          return;
        }
        setConfirming(false);
        setDesign(message.design);
        setSource(message.design.program);
        setSourceErrors([]);
        setFilename(`${message.design.preview.team}.omar`);
        // Show the proposed topology, not whatever was on screen before.
        setSnapshot(message.design.preview);
        if (!arrangedRef.current) {
          arrangedRef.current = true;
          const available = workspaceRef.current?.clientWidth ?? 0;
          // Conversation and diagram side by side; the source pane starts
          // collapsed behind its handle rather than crowding the first look.
          if (available) setBuilderWidth(Math.round(available / 2));
          setInspectorWidth(0);
        }
        setPhase("review");
      },
      () => {
        /* daemon health is polled separately */
      },
    );
    // Clear on teardown rather than on subscribe: transcripts belong to an
    // agent, and setting state synchronously in an effect cascades renders.
    return () => {
      unsubscribe();
      setMessages([]);
    };
  }, [agent]);

  useEffect(() => {
    const thread = threadRef.current;
    if (thread) thread.scrollTop = thread.scrollHeight;
  }, [messages, phase]);

  async function submitPrompt(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const request = prompt.trim();
    if (!request || phase === "drafting" || phase === "spawning") return;
    const selected = selection;
    setPrompt("");
    setError("");
    // The selection belonged to the message just sent. Keeping it would
    // silently attach it to the next one too.
    setSelection([]);
    setPhase("drafting");
    try {
      await agent.send(request, selected);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
      setPhase("idle");
    }
  }

  /** What the run's web agents are waiting on, as of now. */
  const loadPanel = useCallback(
    async (runId: string) => {
      try {
        setPending(await fetchPanel(serveUrl, runId));
      } catch {
        // A panel that cannot be read is not a run that has failed. The next
        // event asks again.
      }
    },
    [serveUrl],
  );

  /** Answer one invocation, as one completion. */
  async function answer(
    invocation: PendingInvocation,
    values: Record<string, unknown>,
  ) {
    if (!run) return;
    setAnswering(true);
    setError("");
    try {
      await answerPanel(serveUrl, run.run_id, {
        invocation_id: invocation.invocation_id,
        agent: invocation.agent,
        values,
      });
      await loadPanel(run.run_id);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setAnswering(false);
    }
  }

  /** Watch a live run until it reaches a terminal status. */
  const observe = useCallback(
    (record: RunRecord) => {
      const diagramUrl = diagramUrlFor(record);
      disconnectRef.current?.();

      const refresh = async () => {
        setSnapshot(await fetchDiagram(diagramUrl));
      };
      void refresh().catch(() => {
        /* the SSE stream reports connection loss */
      });

      /**
       * The stream is authoritative for *that* the run ended; serve is
       * authoritative for the recorded status, which it writes only once
       * `run_topology` returns. Poll briefly so the two can't disagree.
       */
      const settle = async () => {
        for (let attempt = 0; attempt < 10; attempt += 1) {
          const latest = await fetchRun(serveUrl, record.run_id).catch(() => null);
          if (latest) {
            setRun(latest);
            if (isRunFinished(latest.status)) {
              setPhase(latest.status === "completed" ? "finished" : "failed");
              if (latest.error) setError(latest.error);
              return;
            }
          }
          await new Promise((resolve) => setTimeout(resolve, 300));
        }
      };

      disconnectRef.current = subscribeToDiagram(
        diagramUrl,
        (event) => {
          setEvents((current) => [event, ...current].slice(0, 8));
          // Apply first, then refetch. The diagram server shuts down with the
          // run, so the fetch that follows the closing events often loses and
          // the picture would otherwise keep a reaction painted as running.
          setSnapshot((current) =>
            current ? applyDiagramEvent(current, event) : current,
          );
          void refresh().catch(() => {});
          // A reaction starting is how a client learns a web agent is waiting:
          // the event carries an id, so the detail is fetched rather than
          // pushed. Completion is when to look again -- an answered invocation
          // leaves the queue the same way.
          if (event.kind === "reaction_started" || event.kind === "reaction_completed") {
            void loadPanel(record.run_id);
          }
          if (event.kind === "run_completed") {
            setPhase("finished");
            // The run's invocation service goes with it, so nothing is owed
            // any more whatever the last fetch saw.
            setPending([]);
            void settle();
          }
          if (event.kind === "run_failed") {
            setPhase("failed");
            const message = (event.payload as { message?: unknown }).message;
            setError(typeof message === "string" ? message : "The run failed.");
            void settle();
          }
        },
        // The per-run diagram server dies with the run, so a dropped stream is
        // expected at the end rather than an error. Ask serve what happened.
        () => void settle(),
      );
    },
    [serveUrl, loadPanel],
  );

  async function confirmDesign() {
    if (!design || phase !== "review") return;
    setConfirming(false);
    setPhase("spawning");
    setError("");
    try {
      // The source, not the design: the operator may have edited it, and what
      // they are looking at is what they are deploying.
      const record = await startRun(serveUrl, {
        program: source,
        inputs: design.inputs,
      });
      setSnapshot((current) => (current ? { ...current, team: record.team } : current));
      setRun(record);
      setPhase("observing");
      setTab("events");

      observe(record);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
      setPhase("review");
    }
  }

  /** Click a component to include it in the next message, click it again to drop it. */
  function toggleComponent(component: string) {
    setSelection((current) =>
      current.includes(component)
        ? current.filter((name) => name !== component)
        : [...current, component],
    );
  }

  function discardDesign() {
    setTab("source");
    setConfirming(false);
    setDesign(null);
    // The diagram those names pointed at is going away with the design.
    setSelection([]);
    // Discarding puts the studio back where it was before the proposal —
    // leaving the topology on screen implies a design is still in play.
    setSnapshot(isDemo ? reviewWorkflow : null);
    setSource(isDemo ? reviewProgram : "");
    setFilename(isDemo ? `${reviewWorkflow.team}.omar` : "program.omar");
    setSourceErrors([]);
    arrangedRef.current = isDemo;
    setPhase("idle");
    setError("");
  }

  // A tag is a time and a microstep, so the time reads as one: `30s:0`, not
  // thirty thousand million and a colon.
  const tag = snapshot?.current_tag
    ? `${formatDuration(snapshot.current_tag.timestamp)}:${snapshot.current_tag.microstep}`
    : "—";
  // How far behind its own schedule the run is. A sequence number counted
  // messages, which told an operator nothing they wanted to know.
  const lag = typeof snapshot?.lag === "number" ? formatDuration(snapshot.lag) : "—";
  const canRun = daemon.state === "live";
  // The compiler answers on every pause in typing. Debounced because a check
  // compiles a real program on disk, and aborted on the next keystroke so a
  // stale answer cannot land after a newer one.
  useEffect(() => {
    const abort = new AbortController();
    const timer = setTimeout(() => {
      // Nothing to check against, or nothing to check: clear rather than leave
      // an error standing for text that is gone.
      if (!canRun || source.trim() === "") {
        setSourceErrors([]);
        return;
      }
      setChecking(true);
      checkProgram(serveUrl, source, filename, abort.signal)
        .then((result) => setSourceErrors(result.ok ? [] : (result.errors ?? [])))
        .catch((cause) => {
          if (abort.signal.aborted) return;
          setSourceErrors([cause instanceof Error ? cause.message : String(cause)]);
        })
        .finally(() => {
          if (!abort.signal.aborted) setChecking(false);
        });
    }, 400);
    return () => {
      clearTimeout(timer);
      abort.abort();
    };
  }, [source, filename, serveUrl, canRun]);

  const isDeployed = run !== null;

  // Widths are clamped against the workspace so the diagram always keeps a
  // usable column, whichever divider is being dragged. Below a panel's minimum
  // the drag collapses it rather than leaving a sliver.
  const workspaceWidth = () => workspaceRef.current?.clientWidth ?? 0;
  const resolve = (raw: number, min: number, ceiling: number) =>
    raw < min * COLLAPSE_AT ? 0 : clamp(raw, min, Math.max(min, ceiling));

  const setBuilder = (width: number) => {
    const available = workspaceWidth();
    setBuilderWidth(
      resolve(
        width,
        MIN_BUILDER,
        available ? available - MIN_DIAGRAM - inspectorWidth : width,
      ),
    );
  };
  const setInspector = (width: number) => {
    const available = workspaceWidth();
    setInspectorWidth(
      resolve(
        width,
        MIN_INSPECTOR,
        available ? available - MIN_DIAGRAM - builderWidth : width,
      ),
    );
  };

  // Without a topology there is nothing to divide, so the conversation has the
  // window to itself.
  const columns = snapshot
    ? `${builderWidth}px auto minmax(0, 1fr) auto ${inspectorWidth}px`
    : "minmax(0, 1fr)";

  return (
    <main className="studio-shell">
      <header className="topbar">
        <div className="brand">
          {/* Fixed-size brand mark served straight from /public: next/image
              would only add a loader round-trip on Workers. */}
          {/* eslint-disable-next-line @next/next/no-img-element */}
          <img className="brand-mark" src="/omar-logo.png" alt="" width={40} height={40} />
          <span>OMAR <b>Mission Control</b></span>
        </div>
        <div className="runtime-controls">
          <span
            className={`daemon ${daemon.state}`}
            aria-label="Runtime mode"
            title={daemon.state === "offline" ? daemon.reason : undefined}
          >
            <i />
            {daemon.state === "demo" ? "demo topology" : serveUrl}
            {daemon.state === "offline" ? " · unreachable" : null}
          </span>
          <span className="connection" data-phase={phase}>
            {phase}
          </span>
        </div>
      </header>

      <section
        ref={workspaceRef}
        className="workspace"
        style={{ gridTemplateColumns: columns }}
      >
        <aside
          className={[
            "builder-panel",
            builderWidth === 0 ? "collapsed" : "",
            // Alone in the window, so the thread is read as a column rather
            // than stretched across it.
            snapshot ? "" : "solo",
            !snapshot && messages.length === 0 ? "opening" : "",
          ]
            .filter(Boolean)
            .join(" ")}
        >
          <div className="panel-heading">
            <div>
              <span className="eyebrow">WORKFLOW BUILDER</span>
              <h1>What should the team do?</h1>
            </div>
          </div>
          <div className="messages" ref={threadRef}>
            {messages.length === 0 && snapshot ? (
              <p className="builder-status">
                Describe a workflow. The assistant drafts an OMAR program for
                you to confirm before anything runs.
              </p>
            ) : null}
            {messages.map((message) => (
              <ChatMessageView key={message.sequence} message={message} />
            ))}
            {phase === "drafting" ? (
              // The assistant is not on the diagram, so the double click that
              // opens an agent's terminal cannot reach it. Offered beside the
              // wait it belongs to, rather than inside a menu that has to be
              // opened first.
              <div className="waiting-row">
                <Waiting />
                {daemon.state === "live" ? (
                  <button
                    type="button"
                    className="waiting-inspect"
                    onClick={() => setTerminalAgent(ASSISTANT)}
                  >
                    Inspect on terminal
                  </button>
                ) : null}
              </div>
            ) : null}
            {phase === "spawning" ? <Waiting label="Starting the run" /> : null}
          </div>

          {error ? <div className="connection-error">{error}</div> : null}

          {selection.length > 0 ? (
            <div className="selection-bar">
              <span className="selection-label">
                [{selection.join(", ")}] selected
              </span>
              <button
                type="button"
                className="selection-clear"
                onClick={() => setSelection([])}
              >
                Clear
              </button>
            </div>
          ) : null}

          <form className="prompt-box" onSubmit={(event) => void submitPrompt(event)}>
            <textarea
              value={prompt}
              onChange={(event) => setPrompt(event.target.value)}
              onKeyDown={(event) => {
                if (event.key !== "Enter") return;
                // Shift/Ctrl/Cmd+Enter is a newline; plain Enter sends.
                if (event.shiftKey || event.ctrlKey || event.metaKey) return;
                if (event.nativeEvent.isComposing) return;
                event.preventDefault();
                event.currentTarget.form?.requestSubmit();
              }}
              placeholder="Describe a workflow…  Enter to send, Shift+Enter for a new line"
              aria-label="Describe a workflow"
            />
            <div>
              {phase === "observing" || (phase === "review" && !canRun) ? (
                // A run to report on, or a reason deploying is unavailable.
                // Holding a design is neither: the deploy button already says
                // it has not run, so the corner keeps naming the assistant.
                <span className="composer-status">
                  {phase === "observing"
                    ? "Run in progress"
                    : daemon.state === "demo"
                      ? "Demo topology — relaunch with OMAR_SERVE_URL to deploy"
                      : `Cannot reach omar serve at ${serveUrl}`}
                </span>
              ) : (
                <BackendMenu
                  serveUrl={serveUrl}
                  live={daemon.state === "live"}
                />
              )}
              <div className="composer-actions">
                {phase === "review" && design ? (
                  <span role="group" aria-label="Deploy design">
                    {confirming ? (
                      <>
                        <button
                          className="secondary-button"
                          onClick={() => setConfirming(false)}
                          type="button"
                        >
                          Cancel
                        </button>
                        <button
                          className="primary-button"
                          onClick={() => void confirmDesign()}
                          type="button"
                          disabled={!canRun}
                          title="This starts real agents"
                        >
                          Confirm deploy
                        </button>
                      </>
                    ) : (
                      <>
                        <button
                          className="secondary-button"
                          onClick={discardDesign}
                          type="button"
                        >
                          Discard
                        </button>
                        <button
                          className="primary-button"
                          onClick={() => setConfirming(true)}
                          type="button"
                          disabled={!canRun}
                        >
                          Deploy
                        </button>
                      </>
                    )}
                  </span>
                ) : null}
                <button
                  className="send-button"
                  type="submit"
                  aria-label="Draft workflow"
                  disabled={phase === "drafting" || phase === "spawning"}
                >
                  ↑
                </button>
              </div>
            </div>
          </form>
        </aside>

        {snapshot ? (
          <Resizer
            label="the conversation"
            collapsed={builderWidth === 0}
            toward="right"
            onExpand={() => setBuilder(DEFAULT_BUILDER)}
            onDragStart={() => {
              dragOriginRef.current.builder = builderWidth;
            }}
            onDelta={(dx) => setBuilder(dragOriginRef.current.builder + dx)}
            onStep={(dx) => setBuilder(builderWidth + dx)}
          />
        ) : null}

        {snapshot ? (
        <section className="diagram-panel">
          <div className="diagram-heading">
            <div>
              <span className="eyebrow">LIVE TOPOLOGY</span>
              <h2>{snapshot?.team}</h2>
            </div>
            <div className="run-stats">
              <span><small>STATUS</small>{snapshot?.status}</span>
              <span><small>TAG</small>{tag}</span>
              <span><small>LAG</small>{lag}</span>
            </div>
          </div>
          <DiagramCanvas
            snapshot={snapshot}
            selection={selection}
            onToggleComponent={toggleComponent}
            // Agents outlive the run that spawned them, so a finished run can
            // still be opened; before a run there is nothing behind the node.
            onOpenTerminal={canRun && run ? setTerminalAgent : undefined}
          />
        </section>
        ) : null}

        {snapshot ? (
          <Resizer
            label="the source pane"
            collapsed={inspectorWidth === 0}
            toward="left"
            onExpand={() => setInspector(DEFAULT_INSPECTOR)}
            onDragStart={() => {
              dragOriginRef.current.inspector = inspectorWidth;
            }}
            onDelta={(dx) => setInspector(dragOriginRef.current.inspector - dx)}
            onStep={(dx) => setInspector(inspectorWidth - dx)}
          />
        ) : null}

        {snapshot ? (
        <aside className={`inspector-panel${inspectorWidth === 0 ? " collapsed" : ""}`}>
          <div className="tabs" role="tablist">
            <button
              role="tab"
              aria-selected={tab === "source"}
              className={tab === "source" ? "active" : ""}
              onClick={() => setTab("source")}
            >
              Source
            </button>
            {/* Events only exist once something has been deployed. */}
            {isDeployed ? (
              <button
                role="tab"
                aria-selected={tab === "events"}
                className={tab === "events" ? "active" : ""}
                onClick={() => setTab("events")}
              >
                Events
              </button>
            ) : null}
            {/* Like events, only once something is running: before a deploy
                every value is null and nothing can be set, so the tab would
                offer a list of dashes. */}
            {isDeployed ? (
            <button
              role="tab"
              aria-selected={tab === "ports"}
              className={tab === "ports" ? "active" : ""}
              onClick={() => setTab("ports")}
            >
              {pending.length > 0 ? `Ports (${pending.length})` : "Ports"}
            </button>
            ) : null}
          </div>

          {tab === "ports" ? (
            <PortPanel
              snapshot={snapshot}
              pending={pending}
              sending={answering}
              onAnswer={(invocation, values) => void answer(invocation, values)}
            />
          ) : tab === "source" ? (
            <OmarEditor
              source={source}
              filename={filename}
              status={run ? run.status : "draft"}
              errors={sourceErrors}
              checking={checking}
              onSourceChange={setSource}
              onFilenameChange={setFilename}
            />
          ) : (
            <div className="event-strip" role="tabpanel">
              {events.length ? (
                events.map((event) => (
                  <div key={`${event.sequence}-${event.kind}`}>
                    <span>#{event.sequence}</span>
                    <b>{event.kind.replaceAll("_", " ")}</b>
                  </div>
                ))
              ) : (
                <p>Waiting for the first runtime event…</p>
              )}
            </div>
          )}
        </aside>
        ) : null}
      </section>

      {terminalAgent ? (
        <AgentTerminal
          serveUrl={serveUrl}
          agent={terminalAgent}
          onClose={() => setTerminalAgent(null)}
        />
      ) : null}
    </main>
  );
}
