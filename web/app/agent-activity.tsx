"use client";
import { useCallback, useEffect, useRef, useState, useSyncExternalStore } from "react";
import type { ActivitySnapshot, DiagramSnapshot, InvocationActivity, RoleSettingsSnapshot, RoleSelection } from "./lib/protocol-generated";
import { normalizeRuntimeUrl } from "./lib/runtime-client";
import { acceptActivity, activityState, elapsedLabel, noRecentActivity } from "./lib/activity";

export function useActivity(serveUrl: string, runId: string | undefined) {
  const [value, setValue] = useState<ActivitySnapshot | null>(null);
  const [connection, setConnection] = useState<"connecting" | "connected" | "disconnected" | "unsupported">("connecting");
  const [now, setNow] = useState(0);
  const clock = useRef({ server: 0, local: 0 });
  useEffect(() => {
    if (!runId) return;
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout>;
    const controller = new AbortController();
    const poll = async () => {
      try {
        const response = await fetch(`${normalizeRuntimeUrl(serveUrl)}/v1/runs/${encodeURIComponent(runId)}/activity`, { signal: AbortSignal.any([controller.signal, AbortSignal.timeout(8000)]) });
        if (cancelled) return;
        if (response.status === 404) { setConnection("unsupported"); return; }
        if (!response.ok) throw new Error("Activity unavailable");
        const incoming = await response.json() as ActivitySnapshot;
        if (incoming.run_id !== runId || !Array.isArray(incoming.invocations) || !Number.isFinite(incoming.sequence) || !Number.isFinite(incoming.server_time)) throw new Error("Invalid activity snapshot");
        if (cancelled) return;
        clock.current = { server: incoming.server_time, local: performance.now() };
        setNow(incoming.server_time);
        setValue((current) => acceptActivity(current, incoming));
        setConnection("connected");
      } catch { if (!cancelled) setConnection("disconnected"); }
      finally { if (!cancelled) timer = setTimeout(poll, 1000); }
    };
    void poll();
    const tick = setInterval(() => { if (clock.current.server) setNow(clock.current.server + performance.now() - clock.current.local); }, 1000);
    return () => { cancelled = true; clearTimeout(timer); clearInterval(tick); controller.abort(); };
  }, [serveUrl, runId]);
  return { snapshot: value?.run_id === runId ? value : null, connection, now };
}

const thresholdKey = "omar.inactivitySeconds.v1";
function readThreshold() {
  try { const value = Number(localStorage.getItem(thresholdKey)); return value >= 30 && value <= 3600 ? value : 120; }
  catch { return 120; }
}
function subscribeThreshold(changed: () => void) {
  window.addEventListener("storage", changed);
  window.addEventListener("omar-inactivity", changed);
  return () => { window.removeEventListener("storage", changed); window.removeEventListener("omar-inactivity", changed); };
}
export function useInactivityThreshold() {
  const seconds = useSyncExternalStore(subscribeThreshold, readThreshold, () => 120);
  return [seconds, (value: number) => {
    try { localStorage.setItem(thresholdKey, String(Math.max(30, Math.min(3600, value)))); }
    catch { /* A restricted browser cannot persist preferences. */ }
    window.dispatchEvent(new Event("omar-inactivity"));
  }] as const;
}

export function ActivitySummary({ snapshot, now, connected, seconds }: { snapshot: ActivitySnapshot | null; now: number; connected: boolean; seconds: number }) {
  const active = snapshot?.invocations.filter((i) => i.execution === "running") ?? [];
  return <div className="activity-summary" aria-label="Agent activity summary">
    <span>{new Set(active.map((i) => i.agent_name)).size} active agents</span>
    <span>{active.filter((i) => noRecentActivity(i, now, seconds, connected)).length} with no recent reported activity</span>
    {!connected && snapshot ? <span>Activity connection lost · last known state</span> : null}
  </div>;
}

export function RoleSettings({ serveUrl, snapshot }: { serveUrl: string; snapshot: DiagramSnapshot }) {
  const [settings, setSettings] = useState<RoleSettingsSnapshot | null>(null);
  const [error, setError] = useState("");
  const [saving, setSaving] = useState<string | null>(null);
  const [saved, setSaved] = useState<string | null>(null);
  const reload = useCallback(async () => {
    const response = await fetch(`${normalizeRuntimeUrl(serveUrl)}/v1/role-settings`);
    if (!response.ok) throw new Error("Role settings unavailable from this runtime");
    setSettings(await response.json());
  }, [serveUrl]);
  useEffect(() => {
    let cancelled = false;
    fetch(`${normalizeRuntimeUrl(serveUrl)}/v1/role-settings`).then(async (response) => {
      if (!response.ok) throw new Error("Role settings unavailable");
      const value = await response.json();
      if (!cancelled) setSettings(value);
    }).catch(() => { if (!cancelled) setError("Role settings unavailable from this runtime"); });
    return () => { cancelled = true; };
  }, [serveUrl]);
  const update = async (agent: string, backend: string, selection: RoleSelection) => {
    setSaving(agent); setError(""); setSaved(null);
    try {
      const response = await fetch(`${normalizeRuntimeUrl(serveUrl)}/v1/role-settings`, {
        method: "POST", headers: { "content-type": "application/json" },
        body: JSON.stringify({ team: snapshot.team, agent, backend, selection }),
      });
      if (!response.ok) { const body = await response.json(); throw new Error(body.error ?? "Settings could not be saved"); }
      await reload(); setSaved(agent);
    } catch (error) { setError(error instanceof Error ? error.message : "Settings could not be saved"); }
    finally { setSaving(null); }
  };
  return <details className="role-settings">
    <summary>Role model and effort settings · next invocation</summary>
    <button type="button" className="secondary-button" onClick={() => void reload().catch(() => setError("Capabilities could not be refreshed"))}>Refresh supported models</button>
    <p>Saved choices apply when each role’s next invocation starts. Active responses keep their settings.</p>
    {!settings?.capabilities_available ? <p>Supported models unavailable. A connected Codex pane is needed to discover capabilities. Backend defaults apply.</p> : null}
    <div className="role-settings-table">
      {snapshot.agents.map((agent) => {
        const backend = agent.backend.toLowerCase();
        const selection = settings?.roles.find((r) => r.team === snapshot.team && r.agent === agent.name && r.backend === backend)?.selection ?? { model: null, effort: null };
        const supported = backend === "codex" && settings?.capabilities_available;
        const models = settings?.codex_models ?? [];
        const model = models.find((m) => m.model === selection.model);
        return <div className="role-settings-row" key={agent.id}>
          <span>{agent.name}<small>{agent.backend}</small></span>
          {supported ? <>
            <label>Requested model<select aria-label={`${agent.name} requested model`} disabled={saving !== null} value={selection.model ?? ""} onChange={(e) => void update(agent.name, backend, { model: e.target.value || null, effort: null })}>
              <option value="">Keep backend configuration</option>
              {selection.model && !model ? <option value={selection.model}>{selection.model} · no longer available</option> : null}
              {models.map((m) => <option key={m.model} value={m.model}>{m.name}</option>)}
            </select></label>
            <label>Requested effort<select aria-label={`${agent.name} requested effort`} disabled={!model || saving !== null} value={selection.effort ?? ""} onChange={(e) => void update(agent.name, backend, { ...selection, effort: e.target.value || null })}>
              <option value="">Backend default</option>
              {model?.efforts.map((effort) => <option key={effort}>{effort}</option>)}
            </select></label>
          </> : <span>Model and effort selection unsupported or unavailable</span>}
          {saved === agent.name ? <small role="status">Saved for next invocation</small> : null}
        </div>;
      })}
    </div>
    {error ? <p role="alert">{error}</p> : null}
  </details>;
}

function SettingsReadout({ i }: { i: InvocationActivity }) {
  return <>
    <dl className="activity-facts">
      <div><dt>Backend</dt><dd>{i.backend}</dd></div>
      <div><dt>Confirmed active model</dt><dd>{i.confirmed.model ?? "Not reported"}</dd></div>
      <div><dt>Confirmed active effort</dt><dd>{i.confirmed.effort ?? "Not reported"}</dd></div>
      <div><dt>Requested for this invocation</dt><dd>{i.requested.model ?? "Keep backend configuration"} · {i.requested.effort ?? "Default effort"}</dd></div>
      <div><dt>Backend thread configuration</dt><dd>{i.reported_thread_settings.model ?? "Not reported"} · {i.reported_thread_settings.effort ?? "Not reported"}</dd></div>
    </dl>
    <p className="activity-note">{i.settings_application}. Thread configuration does not confirm execution settings.</p>
  </>;
}

export function AgentActivity({ agent, activity, connection, now, seconds, onThreshold, onOpenTerminal, onClose }: {
  agent: string; activity: ActivitySnapshot | null; connection: string; now: number; seconds: number;
  onThreshold: (seconds: number) => void; onOpenTerminal?: () => void; onClose: () => void;
}) {
  const invocations = activity?.invocations.filter((i) => i.agent_name === agent).slice().reverse() ?? [];
  const [chosen, setChosen] = useState<string | null>(null);
  const i = invocations.find((i) => i.invocation_id === chosen) ?? invocations[0];
  const close = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    close.current?.focus();
    const escape = (event: KeyboardEvent) => { if (event.key === "Escape") onClose(); };
    window.addEventListener("keydown", escape);
    return () => window.removeEventListener("keydown", escape);
  }, [onClose]);
  return <aside className="agent-activity" aria-label={`${agent} activity`}>
    <header><div><span className="eyebrow">AGENT ACTIVITY</span><h2>{agent}</h2></div><button type="button" ref={close} className="secondary-button" onClick={onClose}>Close</button></header>
    {onOpenTerminal ? <button type="button" className="secondary-button" onClick={onOpenTerminal}>Open agent terminal</button> : null}
    <label className="inactivity-setting">Inactivity notice after
      <select aria-label="Inactivity notice threshold" value={seconds} onChange={(e) => onThreshold(Number(e.target.value))}>
        {[30, 60, 120, 300, 600, 1800, 3600].map((s) => <option key={s} value={s}>{s < 60 ? `${s} seconds` : `${s / 60} minutes`}</option>)}
      </select>
    </label>
    <p className="activity-note">This notice does not interrupt work or diagnose a stall.</p>
    {connection === "disconnected" ? <p role="status">Activity connection lost · showing last known observations.</p> : null}
    {!i ? <p>Activity details unavailable. {connection === "connecting" ? "Connecting to runtime…" : "No invocation activity has been reported for this agent."}</p> : <>
      <label>Invocation<select aria-label="Invocation history" value={i.invocation_id} onChange={(e) => setChosen(e.target.value)}>
        {invocations.map((i) => <option key={i.invocation_id} value={i.invocation_id}>{i.reaction_id} · {new Date(i.started_at).toLocaleTimeString()}</option>)}
      </select></label>
      <div className="activity-state"><strong>{activityState(i)}</strong><span>Elapsed {elapsedLabel(i.started_at, i.finished_at ?? now)}</span></div>
      <SettingsReadout i={i} />
      {i.connection === "disconnected" ? <p role="status">Agent monitoring connection lost. Execution status is the runtime’s last report.</p> : null}
      {i.connection === "unsupported" ? <p>Activity details unavailable for this backend connection.</p> : null}
      {i.connection === "connecting" ? <p>Connecting to agent activity…</p> : null}
      {noRecentActivity(i, now, seconds, connection === "connected") ? <p className="inactivity-notice">No reported activity for {elapsedLabel(i.last_activity_at, now)}. The agent has not reported another action. It may still be generating a response.</p> : null}
      <h3>Current observed activity</h3>
      {i.active_tools.length ? <ul>{i.active_tools.map((tool) => <li key={tool.id}>{tool.summary} · {tool.started_at === null ? `observed ${elapsedLabel(tool.observed_at, now)} ago; start time unavailable` : elapsedLabel(tool.started_at, now)}</li>)}</ul> : <p>{i.execution === "running" ? "No tool currently reported" : "Invocation ended"}</p>}
      <p>Latest activity: {i.events.at(-1)?.summary ?? "Not reported"}<br />{elapsedLabel(i.last_activity_at, i.finished_at ?? now)} {i.finished_at ? "before invocation ended" : "ago"}</p>
      <details open><summary>View activity · {i.events.length} recent events</summary>
        <ol className="activity-events">{i.events.map((event) => <li key={event.id}>
          <time dateTime={new Date(event.at).toISOString()}>{new Date(event.at).toLocaleTimeString()}</time>
          <span>{event.summary}{event.exit_code !== null ? ` · exit code ${event.exit_code}` : ""}
            {event.time_source === "recovered" ? <small>Recovered after connect; observation time, exact event time unavailable</small> : null}
          </span>
        </li>)}</ol>
      </details>
      <details open><summary>View changes · {i.artifacts.length} attributed files</summary>
        <p className="activity-note">Only successful file-edit events attributable to this agent are listed. Shell and shared workspace changes may be missing. Changed does not mean verified.</p>
        {i.artifacts.length === 0 ? <p>No attributable file changes reported.</p> : i.artifacts.map((file) => <details className="artifact" key={file.id}>
          <summary>{file.change} · {file.path}</summary>
          <p>{new Date(file.observed_at).toLocaleTimeString()} · {file.verification}</p>
          {file.diff ? <pre aria-label={`${file.path} diff`}>{file.diff}</pre> : <p>Diff unavailable</p>}
          {file.diff_truncated ? <p>Diff truncated to 16 KiB.</p> : null}
          <p className="activity-note">Sensitive files are omitted; sensitive lines are redacted.</p>
        </details>)}
      </details>
    </>}
  </aside>;
}
