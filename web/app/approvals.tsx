"use client";

import { useEffect, useRef, useState } from "react";
import type { ApprovalSnapshot, PendingApproval } from "./lib/protocol";
import { normalizeRuntimeUrl } from "./lib/runtime-client";

const EMPTY: ApprovalSnapshot = { sequence: 0, requests: [], monitors: [], recent: [] };

export function useApprovals(serveUrl: string) {
  const [feed, setFeed] = useState({ snapshot: EMPTY, connected: false });
  useEffect(() => {
    if (!serveUrl.trim()) return;
    const events = new EventSource(`${normalizeRuntimeUrl(serveUrl)}/v1/approvals/events`);
    function update(event: MessageEvent) {
      try {
        const value = JSON.parse(event.data) as ApprovalSnapshot;
        if (!Number.isSafeInteger(value.sequence) || !Array.isArray(value.requests) || !Array.isArray(value.monitors) || !Array.isArray(value.recent)) throw new Error("Invalid approval snapshot");
        if (value.requests.some((r) => !r || typeof r.request_id !== "string" || typeof r.agent_id !== "string" || typeof r.summary !== "string" || !Number.isFinite(r.requested_at) || r.resolution_mode !== "terminal")) throw new Error("Invalid approval request");
        setFeed({ snapshot: value, connected: true });
      } catch {
        setFeed((old) => ({ ...old, connected: false }));
      }
    }
    events.addEventListener("approvals", update as EventListener);
    events.onerror = () => setFeed((old) => ({ ...old, connected: false }));
    return () => events.close();
  }, [serveUrl]);
  return feed;
}

function elapsed(start: number, current: number) {
  const seconds = Math.max(0, Math.floor((current - start) / 1000));
  return `${Math.floor(seconds / 60)}m ${String(seconds % 60).padStart(2, "0")}s`;
}

function WaitingDuration({ start }: { start: number }) {
  const [current, setCurrent] = useState(start);
  useEffect(() => {
    const initial = setTimeout(() => setCurrent(Date.now()), 0);
    const timer = setInterval(() => setCurrent(Date.now()), 1000);
    return () => { clearTimeout(initial); clearInterval(timer); };
  }, [start]);
  return <span aria-label="Time waiting">{elapsed(start, current)}</span>;
}

export function ApprovalBadge({ request, lost, onReview }: { request: PendingApproval; lost: boolean; onReview: () => void }) {
  return (
    <button type="button" className="approval-badge" onClick={onReview}>
      <i aria-hidden="true" />
      <span>Waiting for approval · <WaitingDuration start={request.requested_at} />{lost ? " · Connection lost" : ""}</span>
    </button>
  );
}

export function ApprovalPanel({ request, feed, onClose, onTerminal, onReview }: {
  request: PendingApproval;
  feed: ReturnType<typeof useApprovals>;
  onClose: () => void;
  onTerminal: () => void;
  onReview: (request: PendingApproval) => void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const pending = feed.snapshot.requests.some((r) => r.request_id === request.request_id);
  const result = feed.snapshot.recent.find((r) => r.request.request_id === request.request_id);
  const monitor = feed.snapshot.monitors.find((m) => m.agent_id === request.agent_id && m.run_id === request.run_id);
  const lost = !feed.connected || monitor?.state === "disconnected" || monitor?.state === "unsupported";
  useEffect(() => { dialog.current?.showModal(); }, []);
  return (
    <dialog ref={dialog} className="approval-panel" aria-labelledby="approval-title" onCancel={onClose} onClose={onClose}>
      <header>
        <div><span className="eyebrow">{request.agent_name}</span><h2 id="approval-title">{pending ? "Approval required" : result?.outcome === "denied" ? "Request denied" : result?.outcome === "cancelled" ? "Request cancelled" : "Request resolved"}</h2></div>
        <button type="button" className="approval-close" aria-label="Close approval request" onClick={onClose}>×</button>
      </header>
      {pending ? <p className="approval-waiting"><i aria-hidden="true" />Waiting for approval · <WaitingDuration start={request.requested_at} /></p> : <p role="status">{result?.outcome === "denied" ? "The requested action was denied. Review the agent's result in its terminal." : result?.outcome === "cancelled" ? "The request was cancelled." : "The backend resolved this request. Check the agent's result for what happened next."}</p>}
      {lost ? <p className="approval-connection" role="status">Connection lost. Approval status may be out of date; no response has been confirmed by this view.</p> : null}
      <p className="approval-summary">{request.summary}</p>
      <dl>
        <dt>Tool</dt><dd>{request.tool_name}</dd>
        <dt>Scope</dt><dd>{request.scope}</dd>
        {request.command ? <><dt>Command</dt><dd><pre>{request.command}</pre></dd></> : null}
        {request.cwd ? <><dt>Working directory</dt><dd>{request.cwd}</dd></> : null}
      </dl>
      {feed.snapshot.requests.length > 1 ? <nav aria-label="Other approval requests">{feed.snapshot.requests.filter((r) => r.request_id !== request.request_id).map((r) => <button className="approval-badge" key={r.request_id} onClick={() => onReview(r)}>Review request: {r.agent_name} · {r.tool_name}</button>)}</nav> : null}
      <p className="approval-help">Review and respond in this agent’s terminal. Opening it does not approve or dismiss the request.</p>
      <button type="button" className="primary" onClick={onTerminal}>Open agent terminal</button>
    </dialog>
  );
}
