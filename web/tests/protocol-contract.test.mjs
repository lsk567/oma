/**
 * Contract tests over a diagram snapshot captured from a real `omar serve` run.
 *
 * The point is drift: the TypeScript protocol and the Rust serialiser are
 * written apart and nothing in either language ties them together. If the
 * runtime renames a field or changes an id convention, the golden file stops
 * matching what the client and renderer require and these fail. Conformance
 * catches the same drift against a live daemon; this catches it in seconds,
 * without a Rust or Lean build.
 */

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import { startFakeServe, INVALID_MARKER } from "./fake-serve.mjs";
import { RUN_STATUSES, assertRunRecord, isRunFinished } from "../app/lib/protocol.ts";

const golden = JSON.parse(
  await readFile(new URL("./fixtures/diagram-snapshot.v1.json", import.meta.url), "utf8"),
);

test("the captured snapshot matches diagram protocol v1", () => {
  assert.equal(golden.protocol_version, 1);
  assert.equal(typeof golden.team, "string");
  assert.equal(typeof golden.sequence, "number");
  assert.equal(typeof golden.status, "string");
  for (const field of ["agents", "ports", "reactions", "edges"]) {
    assert.ok(Array.isArray(golden[field]), `${field} is an array`);
    assert.ok(golden[field].length > 0, `${field} is populated`);
  }
});

test("ids follow the conventions the renderer resolves against", () => {
  for (const agent of golden.agents) {
    assert.match(agent.id, /^agent::/);
  }
  for (const port of golden.ports) {
    assert.match(port.id, /^port::/);
    assert.ok(
      ["input", "output", "action"].includes(port.kind),
      `port kind ${port.kind} is known`,
    );
    assert.equal(typeof port.type, "string");
    assert.ok("value" in port && "last_tag" in port && "delay" in port);
  }
  for (const reaction of golden.reactions) {
    assert.match(reaction.id, /^reaction::/);
    assert.equal(typeof reaction.order, "number");
    assert.equal(typeof reaction.contract, "string");
    assert.ok(
      ["idle", "running", "completed"].includes(reaction.status),
      `reaction status ${reaction.status} is known`,
    );
  }
});

test("every edge endpoint resolves to a node the diagram draws", () => {
  const ids = new Set([
    ...golden.ports.map((port) => port.id),
    ...golden.reactions.map((reaction) => reaction.id),
  ]);
  for (const edge of golden.edges) {
    assert.ok(ids.has(edge.source), `edge ${edge.id} source ${edge.source} resolves`);
    assert.ok(ids.has(edge.target), `edge ${edge.id} target ${edge.target} resolves`);
    assert.ok(
      ["connection", "trigger", "effect"].includes(edge.kind),
      `edge kind ${edge.kind} is known`,
    );
    // Null is a plain connection, which costs nothing; a number is what
    // `after` bought, with 0 meaning a microstep.
    assert.ok(edge.delay === null || typeof edge.delay === "number");
  }
  const agentIds = new Set(golden.agents.map((agent) => agent.id));
  for (const reaction of golden.reactions) {
    assert.ok(agentIds.has(reaction.agent), `${reaction.id} names a real agent`);
  }
});

test("the demo fixture is the captured topology, not an invented one", async () => {
  // A hand-written fixture drifting from the runtime is how this repo came to
  // document a CLI that never existed. Keep them pinned together.
  const fixtures = await readFile(new URL("../app/lib/fixtures.ts", import.meta.url), "utf8");
  for (const reaction of golden.reactions) {
    assert.ok(
      fixtures.includes(`id: "${reaction.id}"`),
      `fixture carries ${reaction.id}`,
    );
  }
  for (const port of golden.ports) {
    assert.ok(fixtures.includes(`id: "${port.id}"`), `fixture carries ${port.id}`);
  }
  assert.match(fixtures, /export const reviewProgram = `team ReviewFlow\[/);
});

test("the client accepts every run status the daemon can send", async () => {
  // Read out of the daemon's source rather than out of a built binary: this has
  // to fail in seconds in a checkout with no Rust toolchain, because `web.yml`'s
  // test job has none. Same bargain as the fixture check above.
  const serve = await readFile(new URL("../../src/serve.rs", import.meta.url), "utf8");
  const emitted = [...serve.matchAll(/\bstatus:? =? ?"([a-z]+)"/g)].map((m) => m[1]);
  assert.deepEqual(
    [...new Set(emitted)].sort(),
    [...RUN_STATUSES].sort(),
    "the daemon and the client disagree about what a run status is",
  );

  // Naming the status is not enough. `stopped` reached the union while the
  // parser kept its own hand-written list, so a stopped run arrived as an
  // unreadable one and the page sat on a run that had already ended.
  const record = {
    run_id: "run-1",
    team: "Cadence",
    diagram_address: null,
    started_at: 0,
    finished_at: null,
    error: null,
  };
  for (const status of RUN_STATUSES) {
    assert.equal(assertRunRecord({ ...record, status }).status, status);
  }

  // A stop is an ending; asking for one is not.
  assert.ok(isRunFinished("stopped"), "a stopped run has ended");
  assert.ok(!isRunFinished("stopping"), "a stopping run has not ended yet");
});

test("run admission accepts a program and reports where to observe it", async () => {
  const fake = await startFakeServe({ stepMs: 5 });
  try {
    const created = await fetch(`${fake.url}/v1/runs`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        program: "team ReviewFlow[planner : Codex] {} main ReviewFlow { flow = ReviewFlow() }",
        inputs: { "flow.request": "Review the release plan" },
      }),
    });
    assert.equal(created.status, 201);
    const record = await created.json();
    assert.equal(record.team, "ReviewFlow");
    assert.equal(record.status, "running");
    assert.match(record.diagram_address, /^127\.0\.0\.1:\d+$/);
    assert.equal(typeof record.run_id, "string");

    // The address is opaque to the client beyond "the run is observable there".
    const snapshot = await fetch(`http://${record.diagram_address}/v1/diagram`);
    assert.equal(snapshot.status, 200);
    const body = await snapshot.json();
    assert.equal(body.protocol_version, 1);
    assert.equal(
      body.ports.find((port) => port.name === "flow.request").value,
      "Review the release plan",
    );

    const listed = await (await fetch(`${fake.url}/v1/runs/${record.run_id}`)).json();
    assert.equal(listed.run_id, record.run_id);
    assert.equal((await fetch(`${fake.url}/v1/runs/nope`)).status, 404);
  } finally {
    await fake.close();
  }
});

test("a rejected program surfaces the compiler diagnostic", async () => {
  const fake = await startFakeServe({ stepMs: 5 });
  try {
    const response = await fetch(`${fake.url}/v1/runs`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ program: `team Broken( ${INVALID_MARKER}`, inputs: {} }),
    });
    assert.equal(response.status, 400);
    const body = await response.json();
    assert.match(body.error, /omarc failed/);
  } finally {
    await fake.close();
  }
});

test("a run drives to completion over the event stream", async () => {
  const fake = await startFakeServe({ stepMs: 5 });
  try {
    const record = await (
      await fetch(`${fake.url}/v1/runs`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          program: "team ReviewFlow {} main ReviewFlow { flow = ReviewFlow() }",
          inputs: {},
        }),
      })
    ).json();

    const kinds = await collectEvents(`http://${record.diagram_address}/v1/events`);
    assert.ok(kinds.includes("run_started"), "run_started observed");
    assert.ok(kinds.includes("reaction_started"), "reaction_started observed");
    assert.ok(kinds.includes("reaction_completed"), "reaction_completed observed");
    assert.equal(kinds.at(-1), "run_completed");

    const finished = await (await fetch(`${fake.url}/v1/runs/${record.run_id}`)).json();
    assert.equal(finished.status, "completed");
  } finally {
    await fake.close();
  }
});

/** Read the SSE stream until `run_completed`, returning the event kinds in order. */
async function collectEvents(url, timeoutMs = 15_000) {
  const response = await fetch(url);
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  const kinds = [];
  let buffer = "";
  const deadline = Date.now() + timeoutMs;
  while (kinds.at(-1) !== "run_completed") {
    if (Date.now() > deadline) {
      await reader.cancel();
      throw new Error(`timed out waiting for run_completed; saw ${kinds.join(", ")}`);
    }
    const { value, done } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    for (const line of buffer.split("\n")) {
      const match = /^event: (.+)$/.exec(line.trim());
      if (match) kinds.push(match[1]);
    }
    buffer = buffer.slice(buffer.lastIndexOf("\n") + 1);
  }
  await reader.cancel();
  return kinds;
}

test("a selection rides with the message and is bounded", async () => {
  const fake = await startFakeServe({ stepMs: 5 });
  try {
    const post = (payload) =>
      fetch(`${fake.url}/v1/chat`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(payload),
      });

    const accepted = await post({
      text: "make this one retry",
      selection: ["flow.plan", "flow.reaction.1"],
    });
    assert.equal(accepted.status, 202);
    const message = await accepted.json();
    // Stored on the message, so the thread still reads later.
    assert.deepEqual(message.selection, ["flow.plan", "flow.reaction.1"]);

    // Omitting it is what every older client does, and must stay valid.
    const plain = await post({ text: "no selection" });
    assert.equal(plain.status, 202);
    assert.deepEqual((await plain.json()).selection, []);

    // The names reach the assistant's pane, so they are bounded input.
    const flood = await post({
      text: "too many",
      selection: Array.from({ length: 65 }, (_, index) => `p${index}`),
    });
    assert.equal(flood.status, 400);
    assert.match((await flood.json()).error, /more than/);
  } finally {
    await fake.close();
  }
});
