import type { ActivitySnapshot, InvocationActivity } from "./protocol-generated.ts";

export function elapsedLabel(start: number, end: number): string {
  const seconds = Math.max(0, Math.floor((end - start) / 1000));
  return seconds < 60 ? `${seconds}s` : `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
}

/** An open tool remains observed work even when it produces no new output. */
export function noRecentActivity(i: InvocationActivity, now: number, thresholdSeconds: number, connected = true): boolean {
  return connected && i.execution === "running" && i.connection === "connected" &&
    i.active_tools.length === 0 && now - i.last_activity_at >= thresholdSeconds * 1000;
}
export function activityState(i: InvocationActivity): string {
  if (i.execution === "completed") return "Completed";
  if (i.execution === "failed") return "Failed";
  return i.active_tools.length ? "Running a tool" : "Running";
}
export function latestInvocation(snapshot: ActivitySnapshot | null, reactionId: string): InvocationActivity | undefined {
  // Protocol reaction IDs carry a display prefix; runtime IDs do not.
  const id = reactionId.replace(/^reaction::/, "");
  return snapshot?.invocations.filter((i) => i.reaction_id === id || i.reaction_id === reactionId)
    .reduce<InvocationActivity | undefined>((latest, i) => !latest || i.started_at >= latest.started_at ? i : latest, undefined);
}
export function acceptActivity(previous: ActivitySnapshot | null, incoming: ActivitySnapshot): ActivitySnapshot {
  if (!previous || previous.run_id !== incoming.run_id) return incoming;
  return incoming.sequence >= previous.sequence ? incoming : previous;
}
