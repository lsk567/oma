import assert from "node:assert/strict";
import test from "node:test";
import { acceptActivity, activityState, elapsedLabel, latestInvocation, noRecentActivity } from "../app/lib/activity.ts";

const invocation = (overrides = {}) => ({ invocation_id: "a", reaction_id: "flow.reaction.0", agent_name: "flow.one", execution: "running", connection: "connected", started_at: 1000, last_activity_at: 2000, finished_at: null, active_tools: [], ...overrides });

test("long tools remain observed work; inactivity and connection loss are distinct", () => {
  const quiet = invocation();
  assert.equal(noRecentActivity(quiet, 122000, 120), true);
  assert.equal(noRecentActivity(quiet, 122000, 300), false);
  for (const connection of ["unsupported", "disconnected", "connecting"]) assert.equal(noRecentActivity(invocation({connection}), 999999, 120), false);
  assert.equal(noRecentActivity(quiet, 999999, 120, false), false);
  assert.equal(noRecentActivity(invocation({ active_tools: [{id: "tool", started_at: 2000}] }), 999999, 120), false);
  assert.equal(activityState(invocation({ active_tools: [{id: "tool"}] })), "Running a tool");
  assert.equal(activityState(invocation({execution: "completed", active_tools: [{id: "old"}]})), "Completed");
});
test("invocation clock freezes on completion and never uses total workflow time", () => {
  assert.equal(elapsedLabel(1000, 273000), "4m 32s");
  assert.equal(elapsedLabel(273000, 1000), "0s");
  const done = invocation({ finished_at: 9000, execution: "completed" });
  assert.equal(elapsedLabel(done.started_at, done.finished_at ?? 999999), "8s");
  assert.equal(noRecentActivity(done, 999999, 30), false);
});
test("reload snapshots and delayed polls preserve concurrent invocation identity", () => {
  const a = invocation(); const b = invocation({invocation_id: "b", agent_name: "flow.two", reaction_id: "flow.reaction.1", started_at: 4000});
  const snapshot = {run_id:"run", sequence:2, invocations:[a,b]};
  assert.equal(latestInvocation(snapshot, "reaction::flow.reaction.0"), a);
  assert.equal(latestInvocation(snapshot, "reaction::flow.reaction.1"), b);
  assert.equal(acceptActivity(snapshot, {run_id:"run", sequence:1, invocations:[]}), snapshot);
  assert.equal(acceptActivity(null, snapshot), snapshot);
  assert.equal(acceptActivity(snapshot, {run_id:"new",sequence:0,invocations:[]}).run_id, "new");
});
