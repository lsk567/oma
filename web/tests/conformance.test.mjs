/**
 * Runs the real `omar serve` and the fake side by side and asserts they answer
 * the same way.
 *
 * The browser tests drive the fake, which makes them fast and model-free but
 * only as truthful as the fake is. This is what keeps the fake honest: if the
 * runtime changes a status code, an error shape, or a field name, the fake
 * stops matching and these fail — rather than the browser suite staying green
 * while the real integration is broken.
 *
 * It never invokes a model. Programs that name an unresolvable backend fail at
 * `resolve_backend`, before any agent process is started, which exercises the
 * whole admission and failure path for free.
 *
 * Needs the runtime and the Lean compiler, both built from this repository:
 *   (cd .. && cargo build --bin omar && cd lang && lake build omarc)
 *   node --test tests/conformance.test.mjs
 * Skips itself when they are absent, so it never blocks a Node-only checkout.
 */

import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import test, { after, before, describe } from "node:test";
// Node's global WebSocket cannot set an Origin, which is the whole point here.
import WebSocket from "ws";

import { startFakeServe } from "./fake-serve.mjs";

// The runtime is a sibling directory now, not a sibling checkout: these are
// built from the same commit as the client they are checked against.
const OMAR_BIN = process.env.OMAR_BIN ?? resolve("../target/debug/omar");
const OMARC_BIN = process.env.OMARC_BIN ?? resolve("../lang/.lake/build/bin/omarc");
const AVAILABLE = existsSync(OMAR_BIN) && existsSync(OMARC_BIN);

/**
 * Probe for features rather than pin a runtime version. A checkout that predates
 * them skips instead of failing, so this can run against `main` before the
 * runtime side has landed and start testing for real the moment it does.
 */
function runtimeSupports(args, needle) {
  if (!AVAILABLE) return false;
  const probe = spawnSync(OMAR_BIN, args, { encoding: "utf8" });
  if (probe.error) return false;
  if (needle === null) return probe.status === 0;
  return `${probe.stdout}${probe.stderr}`.includes(needle);
}

const SERVES_HEADLESS = runtimeSupports(["serve", "--help"], "--no-ea");
const HAS_STUB_AGENT = runtimeSupports(["stub-agent", "--help"], null);
// Topology agents run in tmux panes, so a real run needs one.
const HAS_TMUX = spawnSync("tmux", ["-V"], { encoding: "utf8" }).status === 0;

const WIRE_SKIP = !AVAILABLE
  ? "OMAR_BIN/OMARC_BIN not found"
  : !SERVES_HEADLESS
    ? "runtime predates `omar serve --no-ea`"
    : false;
const RUN_SKIP =
  WIRE_SKIP ||
  (!HAS_STUB_AGENT ? "runtime predates the stub agent backend" : false) ||
  (!HAS_TMUX ? "tmux is not available" : false);

/** Agents are stubs, so this runs to completion without a model. */
const STUB_FLOW = `team StubFlow[writer : Stub,
             editor : Stub]
{
    input topic : string
    output blurb : string
    action draft : string

    prompt writer(topic) -> draft
    "
        Draft something about $(topic).
    "

    prompt editor(draft) -> blurb
    "
        Tighten $(draft).
    "
}

main StubFlow {
    flow = StubFlow()
}`;

/** Compiles, but names a backend the runtime cannot resolve, so it fails before
 *  spawning anything. */
const UNRESOLVABLE = `team Conformance[worker : NotARealBackend]
{
    input go : string
    output done : string

    prompt worker(go) -> done
    "
        Echo $(go) into done.
    "
}

main Conformance {
    only = Conformance()
}`;

async function startRealServe() {
  const home = await mkdtemp(join(tmpdir(), "omar-conformance-"));
  // Agents run in tmux sessions named from this prefix. Without a unique one
  // the test would spawn into — and later kill — the developer's own sessions.
  const sessionPrefix = `omar-conformance-${process.pid}-`;
  await mkdir(join(home, ".omar"), { recursive: true });
  await writeFile(
    join(home, ".omar/config.toml"),
    `[dashboard]\nsession_prefix = "${sessionPrefix}"\n`,
  );
  const child = spawn(OMAR_BIN, ["serve", "--address", "127.0.0.1:0", "--no-ea"], {
    env: { ...process.env, HOME: home, OMARC_BIN },
    stdio: ["ignore", "pipe", "pipe"],
  });

  const url = await new Promise((resolveUrl, rejectUrl) => {
    let buffer = "";
    const timer = setTimeout(
      () => rejectUrl(new Error(`serve did not report an address: ${buffer}`)),
      30_000,
    );
    child.stdout.on("data", (chunk) => {
      buffer += chunk;
      const match = /OMAR serve: (http:\/\/\S+)/.exec(buffer);
      if (match) {
        clearTimeout(timer);
        resolveUrl(match[1]);
      }
    });
    child.on("exit", (code) => {
      clearTimeout(timer);
      rejectUrl(new Error(`serve exited with ${code}: ${buffer}`));
    });
  });

  return {
    url,
    home,
    sessionPrefix,
    async close() {
      child.kill();
      // A failed or finished run leaves its agent sessions behind, so the
      // harness clears its own namespace rather than accumulating panes.
      await new Promise((done) => {
        const list = spawn("sh", [
          "-c",
          `tmux ls -F '#{session_name}' 2>/dev/null | grep '^${sessionPrefix}' | xargs -r -n1 tmux kill-session -t`,
        ]);
        list.on("exit", done);
        list.on("error", done);
      });
      await rm(home, { recursive: true, force: true });
    },
  };
}

describe("wire conformance between the fake and the real daemon", { skip: WIRE_SKIP }, () => {
  let real;
  let fake;

  before(async () => {
    real = await startRealServe();
    fake = await startFakeServe({ stepMs: 20 });
  });

  after(async () => {
    await real?.close();
    await fake?.close();
  });

  /** Send the same request to both and return `{real, fake}` responses. */
  async function both(path, init) {
    const send = async (base) => {
      const response = await fetch(`${base}${path}`, init);
      const text = await response.text();
      let body;
      try {
        body = JSON.parse(text);
      } catch {
        body = text;
      }
      return { status: response.status, body };
    };
    return { real: await send(real.url), fake: await send(fake.url) };
  }

  const post = (payload) => ({
    method: "POST",
    headers: { "content-type": "application/json" },
    body: typeof payload === "string" ? payload : JSON.stringify(payload),
  });

  test("health reports the same protocol version", async () => {
    const { real: r, fake: f } = await both("/health");
    assert.equal(r.status, 200);
    assert.equal(f.status, r.status);
    assert.equal(f.body.protocol_version, r.body.protocol_version);
    assert.equal(f.body.status, r.body.status);
  });

  test("an empty run listing has the same shape", async () => {
    const { real: r, fake: f } = await both("/v1/runs");
    assert.equal(r.status, 200);
    assert.equal(f.status, r.status);
    assert.deepEqual(Object.keys(f.body), Object.keys(r.body));
    assert.deepEqual(r.body.runs, []);
  });

  test("an unknown run is a 404 with an error field", async () => {
    const { real: r, fake: f } = await both("/v1/runs/does-not-exist");
    assert.equal(r.status, 404);
    assert.equal(f.status, r.status);
    assert.equal(typeof r.body.error, "string");
    assert.equal(typeof f.body.error, "string");
  });

  test("a malformed body is a 400 on both", async () => {
    const { real: r, fake: f } = await both("/v1/runs", post("not json"));
    assert.equal(r.status, 400);
    assert.equal(f.status, r.status);
    assert.match(r.body.error, /invalid request/);
    assert.match(f.body.error, /invalid request/);
  });

  test("an oversized selection is refused by both", async () => {
    // Selection is the newest field on the chat request, and the names are
    // echoed into the assistant's pane. The cap has to agree across the two,
    // and a 400 lands before delivery, so this needs no assistant running.
    const selection = Array.from({ length: 65 }, (_, index) => `p${index}`);
    const { real: r, fake: f } = await both(
      "/v1/chat",
      post({ text: "make these retry", selection }),
    );
    assert.equal(r.status, 400, `real: ${JSON.stringify(r.body)}`);
    assert.equal(f.status, r.status);
    assert.match(r.body.error, /more than/);
    assert.match(f.body.error, /more than/);
  });

  test("a terminal refuses a foreign origin on both", async () => {
    // CORS does not apply to WebSockets, so this check is the only thing
    // standing between a page the operator visits and a shell in their agent.
    // It happens before the upgrade, so no agent needs to be running.
    const handshake = (base) =>
      new Promise((resolve) => {
        const socket = new WebSocket(
          `${base.replace(/^http/, "ws")}/v1/agents/anything/terminal`,
          { origin: "https://evil.example" },
        );
        socket.on("unexpected-response", (_request, response) => {
          socket.terminate();
          resolve(response.statusCode);
        });
        socket.on("open", () => {
          socket.terminate();
          resolve(101);
        });
        socket.on("error", () => resolve(0));
      });

    const [realStatus, fakeStatus] = await Promise.all([
      handshake(real.url),
      handshake(fake.url),
    ]);
    assert.equal(realStatus, 403, "the real daemon refuses a foreign origin");
    assert.equal(fakeStatus, realStatus);
  });

  test("a program the compiler rejects is a 400 carrying omarc's diagnostic", async () => {
    const { real: r, fake: f } = await both(
      "/v1/runs",
      post({ program: "team Broken( {{{ !!invalid!!", inputs: {} }),
    );
    assert.equal(r.status, 400, `real: ${JSON.stringify(r.body)}`);
    assert.equal(f.status, r.status);
    // The fake hard-codes a diagnostic; the real one comes from omarc. Both
    // must name the compiler, which is what the studio surfaces.
    assert.match(r.body.error, /omarc/);
    assert.match(f.body.error, /omarc/);
  });

  test("an empty chat message is a 400 on both", async () => {
    const { real: r, fake: f } = await both("/v1/chat", post({ text: "   " }));
    assert.equal(r.status, 400);
    assert.equal(f.status, r.status);
  });

  test("the agent endpoints reject a wrong token identically", async () => {
    for (const path of ["/v1/agent/reply", "/v1/agent/proposals"]) {
      const { real: r, fake: f } = await both(
        path,
        post({
          token: "wrong",
          text: "hi",
          program: "team T { input a : int } main T { t = T() }",
          summary: "s",
        }),
      );
      assert.equal(r.status, 403, `${path} real`);
      assert.equal(f.status, r.status, `${path} fake`);
    }
  });

  test("a timeline is what the run then does, and is asked for", async () => {
    // The timeline's whole claim. If it and the run disagree, the operator is
    // being shown a prediction of a different program.
    const check = async (body) =>
      (
        await fetch(`${real.url}/v1/programs/check`, {
          ...post({ program: STUB_FLOW, filename: "StubFlow.omar", ...body }),
        })
      ).json();

    // Validity alone. A caller that did not ask for a timeline is not handed
    // one to ignore.
    const plain = await check({});
    assert.equal(plain.ok, true, JSON.stringify(plain));
    assert.deepEqual(plain.open_inputs, ["flow.topic"]);
    assert.equal(plain.steps, undefined);
    assert.equal(plain.truncated, undefined);

    // Nothing set: the program does not move, and says so rather than
    // pretending it would.
    const idle = await check({ timeline: true, present: [] });
    assert.deepEqual(idle.steps, []);

    const drawn = await check({ timeline: true, present: ["flow.topic"] });
    assert.equal(drawn.truncated, false);
    assert.ok(drawn.steps.length >= 2, JSON.stringify(drawn.steps));
    assert.deepEqual(drawn.steps[0].events, ["flow.topic"]);

    // A program that does not compile is reported, not drawn.
    const broken = await (
      await fetch(`${real.url}/v1/programs/check`, {
        ...post({ program: "team Broken {", filename: "Broken.omar", timeline: true }),
      })
    ).json();
    assert.equal(broken.ok, false);
    assert.match(broken.errors[0], /Broken\.omar/);
    // The scratch path it was compiled at is not the operator's business.
    assert.doesNotMatch(broken.errors[0], /omar-check-/);
  });

  test("a real proposal compiles and carries a snapshot the client accepts", async () => {
    // `--no-ea` still writes the context, so the harness can stand in for the
    // assistant without an agent process existing.
    const context = JSON.parse(
      await readFile(join(real.home, ".omar/mcp/ea-0/context.json"), "utf8"),
    );
    const token = context.serve?.token;
    assert.equal(typeof token, "string", "serve wrote its token into the context");

    const response = await fetch(`${real.url}/v1/agent/proposals`, {
      ...post({ token, program: UNRESOLVABLE, summary: "conformance", inputs: {} }),
    });
    assert.equal(response.status, 202, `proposal rejected: ${response.status}`);

    const { messages } = await (await fetch(`${real.url}/v1/chat`)).json();
    const proposal = messages.find((message) => message.design);
    assert.ok(proposal, "the proposal reached the conversation");

    // Everything the renderer resolves against, produced by the real compiler.
    const preview = proposal.design.preview;
    assert.equal(preview.protocol_version, 1);
    assert.equal(preview.team, "Conformance");
    const ids = new Set([
      ...preview.ports.map((port) => port.id),
      ...preview.reactions.map((reaction) => reaction.id),
    ]);
    for (const edge of preview.edges) {
      assert.ok(ids.has(edge.source), `edge ${edge.id} source resolves`);
      assert.ok(ids.has(edge.target), `edge ${edge.id} target resolves`);
    }
  });

  test("a run that cannot start reports failure through the registry", async () => {
    const created = await fetch(`${real.url}/v1/runs`, {
      ...post({ program: UNRESOLVABLE, inputs: { "only.go": "hello" } }),
    });
    // Admission succeeds: the program compiles and the diagram server binds
    // before anything is spawned.
    // Read once: an assertion message that consumes the body leaves nothing
    // to parse.
    const raw = await created.text();
    assert.equal(created.status, 201, raw);
    const record = JSON.parse(raw);
    assert.match(record.diagram_address, /^127\.0\.0\.1:\d+$/);

    // The per-run diagram server dies with the run, and this run fails in
    // milliseconds, so the address is only guaranteed well-formed — not still
    // answering. The proposal test above covers the snapshot contract.

    // The backend cannot be resolved, so the run fails without starting an
    // agent, and the failure is recorded rather than left hanging.
    const deadline = Date.now() + 30_000;
    let latest;
    do {
      latest = await (await fetch(`${real.url}/v1/runs/${record.run_id}`)).json();
      if (latest.status === "failed") break;
      await new Promise((wait) => setTimeout(wait, 200));
    } while (Date.now() < deadline);

    assert.equal(latest.status, "failed", JSON.stringify(latest));
    assert.match(latest.error, /NotARealBackend|Unknown backend/);
  });
});

/**
 * These need tmux, because a topology agent runs in a pane. Kept apart from the
 * wire-conformance suite so that one stays runnable anywhere.
 */
describe(
  "a real run, driven by stub agents",
  { skip: RUN_SKIP },
  () => {
    let real;

    before(async () => {
      real = await startRealServe();
    });

    after(async () => {
      await real?.close();
    });

    test("reaches run_completed on the stream and completed in the registry", async () => {
      const created = await fetch(`${real.url}/v1/runs`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          program: STUB_FLOW,
          inputs: { "flow.topic": "release notes" },
        }),
      });
      const raw = await created.text();
      assert.equal(created.status, 201, raw);
      const record = JSON.parse(raw);

      // Watch the run's own stream while it happens. This is the path a client
      // actually observes, and nothing short of a real run exercises it.
      const kinds = await collectEvents(
        `http://${record.diagram_address}/v1/events`,
        90_000,
      );
      // `run_started` is published before admission returns and the diagram
      // server does not replay, so a client that subscribes afterwards will
      // never see it. Everything from the first tag onwards is observable.
      assert.ok(kinds.includes("tag_advanced"), `saw ${kinds.join(", ")}`);
      assert.ok(kinds.includes("reaction_started"), `saw ${kinds.join(", ")}`);
      assert.ok(kinds.includes("reaction_completed"), `saw ${kinds.join(", ")}`);
      assert.equal(kinds.at(-1), "run_completed");

      // The registry is written when `run_topology` returns, a moment after
      // the observer publishes `run_completed`. The two are not simultaneous,
      // which is why the studio reconciles them with a bounded poll.
      const deadline = Date.now() + 15_000;
      let latest;
      do {
        latest = await (await fetch(`${real.url}/v1/runs/${record.run_id}`)).json();
        if (latest.status === "completed") break;
        await new Promise((wait) => setTimeout(wait, 200));
      } while (Date.now() < deadline);

      assert.equal(latest.status, "completed", JSON.stringify(latest));
      assert.equal(latest.error, null);
    });
  },
);

/** Read an SSE stream until the run ends, returning the event kinds in order. */
async function collectEvents(url, timeoutMs) {
  const response = await fetch(url);
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  const kinds = [];
  const deadline = Date.now() + timeoutMs;
  let buffer = "";
  const done = () =>
    kinds.at(-1) === "run_completed" || kinds.at(-1) === "run_failed";
  while (!done()) {
    if (Date.now() > deadline) {
      await reader.cancel();
      throw new Error(`timed out; saw ${kinds.join(", ") || "nothing"}`);
    }
    const chunk = await reader.read();
    if (chunk.done) break;
    buffer += decoder.decode(chunk.value, { stream: true });
    for (const line of buffer.split("\n")) {
      const match = /^event: (.+)$/.exec(line.trim());
      if (match) kinds.push(match[1]);
    }
    buffer = buffer.slice(buffer.lastIndexOf("\n") + 1);
  }
  await reader.cancel();
  return kinds;
}
