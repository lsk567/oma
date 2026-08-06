/**
 * A stand-in for `omar serve` that speaks the same wire protocol.
 *
 * It exists so CI can drive the whole confirm-then-spawn flow without a model,
 * a Lean toolchain, or tmux. It deliberately serves the admission API *and* the
 * per-run diagram API from one port, because the only thing the client is
 * allowed to assume is that `diagram_address` is where the run can be observed.
 *
 * The golden snapshot it replays was captured from a real `omar serve` run, so
 * drift between this fake and the runtime shows up as a failing contract test.
 */

import { createServer } from "node:http";
import { WebSocketServer } from "ws";
import { readFile } from "node:fs/promises";
import { randomUUID } from "node:crypto";

/** Mirrors the daemon's cap; the names reach the assistant's pane. */
const MAX_SELECTION = 64;

const CORS = {
  "access-control-allow-origin": "*",
  "access-control-allow-methods": "GET, POST, OPTIONS",
  "access-control-allow-headers": "content-type",
};

/** Marker a test can put in a program to exercise the compile-failure path. */
const INVALID_MARKER = "!!invalid!!";

/** What the stand-in EA proposes. Real source, verified against omarc. */
const PROPOSED_PROGRAM = await readFile(
  new URL("./fixtures/review-flow.omar", import.meta.url),
  "utf8",
);

export async function startFakeServe({
  stepMs = 120,
  host = "127.0.0.1",
  port = 0,
  /** Which captured topology to replay; see tests/fixtures. */
  snapshot: snapshotFile = "diagram-snapshot.v1.json",
  /** The geometry a terminal announces, as the daemon reports the agent's. */
  terminal: terminalSize = { cols: 96, rows: 28 },
} = {}) {
  const golden = JSON.parse(
    await readFile(new URL(`./fixtures/${snapshotFile}`, import.meta.url), "utf8"),
  );

  /** @type {Map<string, {record: object, snapshot: object, subscribers: Set<import("node:http").ServerResponse>, sequence: number}>} */
  const runs = new Map();

  const server = createServer((request, response) => {
    const url = new URL(request.url, `http://${request.headers.host}`);

    if (request.method === "OPTIONS") {
      response.writeHead(204, CORS).end();
      return;
    }
    if (request.method === "GET" && url.pathname === "/health") {
      return json(response, 200, { status: "ok", protocol_version: 1 });
    }
    if (request.method === "POST" && url.pathname === "/v1/runs") {
      return readBody(request).then((body) => admit(body, response));
    }
    if (request.method === "GET" && url.pathname === "/v1/runs") {
      return json(response, 200, {
        runs: [...runs.values()].map((entry) => entry.record),
      });
    }
    if (
      request.method === "POST" &&
      url.pathname.startsWith("/v1/runs/") &&
      url.pathname.endsWith("/inputs")
    ) {
      const runId = url.pathname.slice("/v1/runs/".length, -"/inputs".length);
      return readBody(request).then((body) => sendInputs(runId, body, response));
    }
    if (request.method === "GET" && url.pathname.startsWith("/v1/runs/")) {
      const entry = runs.get(url.pathname.slice("/v1/runs/".length));
      return entry
        ? json(response, 200, entry.record)
        : json(response, 404, { error: "unknown run" });
    }
    // Diagram surface for the single active run, mirroring the per-run server.
    if (request.method === "GET" && url.pathname === "/v1/diagram") {
      const entry = latest();
      return entry
        ? json(response, 200, entry.snapshot)
        : json(response, 404, { error: "no run" });
    }
    if (request.method === "GET" && url.pathname === "/v1/events") {
      return subscribe(response);
    }
    // Agent-only endpoints. The stand-in assistant is internal, so these exist
    // to mirror the real daemon's surface rather than to be used.
    // Which assistant is answering, and moving it to another one. The real
    // daemon relaunches the process; the fake only has to agree on the wire.
    if (request.method === "GET" && url.pathname === "/v1/agent") {
      return json(response, 200, {
        backend,
        available: ["claude", "codex", "cursor", "opencode", "agy"],
      });
    }
    if (request.method === "POST" && url.pathname === "/v1/agent/backend") {
      return readBody(request).then((body) => {
        let chosen;
        try {
          chosen = JSON.parse(body).backend;
        } catch {
          return json(response, 400, { error: "invalid request: not JSON" });
        }
        if (!["claude", "codex", "cursor", "opencode", "agy"].includes(chosen)) {
          return json(response, 400, {
            error: `Unknown backend '${chosen}'.`,
          });
        }
        backend = chosen;
        return json(response, 200, { backend });
      });
    }
    if (request.method === "POST" && url.pathname.startsWith("/v1/agent/")) {
      return readBody(request).then((body) => {
        let payload;
        try {
          payload = JSON.parse(body);
        } catch {
          return json(response, 400, { error: "invalid request: not JSON" });
        }
        if (payload.token !== agentToken) {
          return json(response, 403, { error: "forbidden" });
        }
        if (url.pathname === "/v1/agent/reply") {
          publishChat("assistant", String(payload.text ?? ""), null, payload.progress === true);
        } else if (url.pathname === "/v1/agent/proposals") {
          const preview = structuredClone(golden);
          preview.status = "ready";
          publishChat("assistant", String(payload.summary ?? ""), {
            program: String(payload.program ?? ""),
            inputs: payload.inputs ?? {},
            preview,
          });
        } else {
          return json(response, 404, { error: "not found" });
        }
        return json(response, 202, { status: "delivered" });
      });
    }
    // Operator/EA conversation.
    if (request.method === "GET" && url.pathname === "/v1/chat") {
      return json(response, 200, { messages: chat.messages });
    }
    if (request.method === "POST" && url.pathname === "/v1/chat") {
      return readBody(request).then((body) => converse(body, response));
    }
    if (request.method === "GET" && url.pathname === "/v1/chat/events") {
      return subscribeChat(response);
    }
    json(response, 404, { error: "not found" });
  });

  // A stand-in for the agent's tmux session: it announces a size the way the
  // daemon does, then echoes what is typed so a test can prove keystrokes
  // travelled up the same socket the screen comes down.
  const terminals = new WebSocketServer({ noServer: true });
  server.on("upgrade", (request, socket, head) => {
    const url = new URL(request.url, "http://127.0.0.1");
    // The assistant has a route of its own: its tmux session is not named
    // like an agent's, so no agent name reaches it.
    const assistant = url.pathname === "/v1/agent/terminal";
    const match = assistant
      ? ["", "the assistant"]
      : /^\/v1\/agents\/(.+)\/terminal$/.exec(url.pathname);
    // CORS never reaches a WebSocket, so the daemon refuses foreign origins at
    // the handshake and the fake has to behave the same way.
    const origin = request.headers.origin;
    const local =
      !origin || /^https?:\/\/(localhost|127\.0\.0\.1|\[::1\])(:\d+)?$/.test(origin);
    if (!match || !local) {
      socket.write(`HTTP/1.1 ${match ? 403 : 404} Refused\r\n\r\n`);
      socket.destroy();
      return;
    }
    terminals.handleUpgrade(request, socket, head, (client) => {
      const agent = decodeURIComponent(match[1]);
      let size = terminalSize;
      client.send(JSON.stringify(size));
      client.send(Buffer.from(`${agent} $ `));
      client.on("message", (data, isBinary) => {
        // Text is a resize control frame, binary is keystrokes. The daemon
        // reflows the session and answers with the shape it settled on;
        // echoing a control frame back would type it at the shell instead.
        if (!isBinary) {
          size = JSON.parse(data.toString());
          client.send(JSON.stringify(size));
          return;
        }
        client.send(Buffer.from(data));
      });
    });
  });

  let backend = "codex";
  const chat = { messages: [], subscribers: new Set(), sequence: 0 };
  const agentToken = randomUUID();

  function publishChat(role, text, design, progress = false, selection = []) {
    chat.sequence += 1;
    const message = {
      sequence: chat.sequence,
      role,
      text,
      progress,
      design: design ?? null,
      selection,
    };
    chat.messages.push(message);
    const kind = design ? "design_proposed" : "message";
    const frame = `id: ${message.sequence}\nevent: ${kind}\ndata: ${JSON.stringify(message)}\n\n`;
    for (const subscriber of chat.subscribers) subscriber.write(frame);
    return message;
  }

  function subscribeChat(response) {
    response.writeHead(200, {
      ...CORS,
      "content-type": "text/event-stream",
      "cache-control": "no-cache",
      connection: "keep-alive",
    });
    response.write(": connected\n\n");
    // Replay, matching the real server, so a reload rejoins the conversation.
    for (const message of chat.messages) {
      const kind = message.design ? "design_proposed" : "message";
      response.write(
        `id: ${message.sequence}\nevent: ${kind}\ndata: ${JSON.stringify(message)}\n\n`,
      );
    }
    chat.subscribers.add(response);
    response.on("close", () => chat.subscribers.delete(response));
  }

  /**
   * Stands in for the EA. Asks one clarifying question first, so the studio's
   * multi-turn handling is exercised, then proposes on the next message.
   */
  function converse(body, response) {
    let request;
    try {
      request = JSON.parse(body);
    } catch {
      return json(response, 400, { error: "invalid request: not JSON" });
    }
    if (typeof request.text !== "string" || request.text.trim().length === 0) {
      return json(response, 400, { error: "message is empty" });
    }
    const selection = Array.isArray(request.selection) ? request.selection : [];
    if (selection.length > MAX_SELECTION) {
      return json(response, 400, {
        error: `selection names more than ${MAX_SELECTION} components`,
      });
    }
    const operator = publishChat("operator", request.text, null, false, selection);
    json(response, 202, operator);

    const asked = chat.messages.some(
      (message) => message.role === "assistant" && !message.progress,
    );
    // Real assistants narrate while they work; that must not end the wait.
    setTimeout(
      () => publishChat("assistant", "Reading the ports and reactions…", null, true),
      Math.max(10, stepMs / 3),
    );
    setTimeout(() => {
      if (!asked) {
        publishChat(
          "assistant",
          "Which agent should own the final write, the planner or the reviewer?",
        );
        return;
      }
      // The real daemon compiles the proposal and attaches the topology, so
      // the operator sees what they are approving before any run exists.
      const preview = structuredClone(golden);
      preview.status = "ready";
      // Markdown, because that is what the EA actually writes.
      publishChat(
        "assistant",
        "Drafted a **three-reaction** review loop.\n\n" +
          "- `planner` drafts the plan\n" +
          "- `reviewer` critiques it\n\n" +
          "```\nrequest -> planner -> reviewer -> result\n```",
        {
          program: PROPOSED_PROGRAM,
          inputs: { "flow.request": chat.messages[0]?.text ?? "" },
          preview,
        },
      );
      // Real assistants comment straight after proposing. That must not look
      // like a withdrawal of the proposal.
      setTimeout(
        () => publishChat("assistant", "It is in your queue for approval."),
        stepMs,
      );
    }, stepMs);
  }

  function latest() {
    return [...runs.values()].at(-1);
  }

  function admit(body, response) {
    let request;
    try {
      request = JSON.parse(body);
    } catch {
      return json(response, 400, { error: "invalid request: not JSON" });
    }
    if (typeof request.program !== "string" || request.program.length === 0) {
      return json(response, 400, { error: "invalid request: missing program" });
    }
    if (request.program.includes(INVALID_MARKER)) {
      // What the real daemon does with a program omarc rejects.
      return json(response, 400, {
        error: "omarc failed: expected identifier, found some (Omar.Token.sym \"{\")",
      });
    }

    const runId = randomUUID();
    const address = `${host}:${server.address().port}`;
    const snapshot = structuredClone(golden);
    // Deploying no longer starts the program: with an open input unset the run
    // comes up and waits, which is the state the panel exists to resolve.
    const openInputs = openInputsOf(snapshot);
    const seeded = request.inputs ?? {};
    const unset = openInputs.filter((port) => !(port.name in seeded));
    snapshot.status = unset.length > 0 ? "awaiting_input" : "running";
    const requested = request.inputs?.["flow.request"];
    for (const port of snapshot.ports) {
      if (port.name === "flow.request" && typeof requested === "string") {
        port.value = requested;
      }
    }
    const entry = {
      record: {
        run_id: runId,
        team: snapshot.team,
        status: "running",
        diagram_address: address,
        started_at: Math.floor(Date.now() / 1000),
        finished_at: null,
        error: null,
      },
      snapshot,
      subscribers: new Set(),
      sequence: 0,
      driving: false,
      unset: new Set(unset.map((port) => port.name)),
    };
    runs.set(runId, entry);
    json(response, 201, entry.record);
  }

  function subscribe(response) {
    const entry = latest();
    if (!entry) return json(response, 404, { error: "no run" });
    response.writeHead(200, {
      ...CORS,
      "content-type": "text/event-stream",
      "cache-control": "no-cache",
      connection: "keep-alive",
    });
    response.write(": connected\n\n");
    entry.subscribers.add(response);
    response.on("close", () => entry.subscribers.delete(response));

    // The real diagram server does not replay history, and a real run lasts far
    // longer than a subscriber takes to attach. Only start driving once someone
    // is listening, so the test can never lose the race and hang.
    maybeDrive(entry);
  }

  /**
   * Start the run once nothing is holding it back.
   *
   * Two things hold it: an open input nobody has set, and nobody listening. The
   * second is not the real daemon's rule — it is here so a test can never lose
   * the race between subscribing and the first event.
   */
  function maybeDrive(entry) {
    if (entry.driving) return;
    if (entry.unset.size > 0) return;
    if (entry.subscribers.size === 0) return;
    entry.driving = true;
    void driveRun(entry);
  }

  /** Inputs nothing in the topology writes to: the operator's to set. */
  function openInputsOf(snapshot) {
    const fed = new Set(
      snapshot.edges.filter((edge) => edge.kind === "connection").map((edge) => edge.target),
    );
    return snapshot.ports.filter((port) => port.kind === "input" && !fed.has(port.id));
  }

  /** What the real daemon does with `POST /v1/runs/{id}/inputs`. */
  function sendInputs(runId, body, response) {
    const entry = runs.get(runId);
    if (!entry) return json(response, 404, { error: "unknown run" });
    let request;
    try {
      request = JSON.parse(body);
    } catch {
      return json(response, 400, { error: "invalid request: not JSON" });
    }
    const values = request.values ?? {};
    if (Object.keys(values).length === 0) {
      return json(response, 400, { error: "no values to send" });
    }
    const open = new Map(openInputsOf(entry.snapshot).map((port) => [port.name, port]));
    for (const [name, value] of Object.entries(values)) {
      const port = open.get(name);
      if (!port) {
        return json(response, 400, {
          error: `'${name}' is not an open input of this run`,
        });
      }
      // The real daemon checks the value against the port's type before it
      // reaches the run, and the client is expected to have parsed it.
      if (port.type === "int" && !Number.isInteger(value)) {
        return json(response, 400, { error: `${name}: expected int, got ${JSON.stringify(value)}` });
      }
      if (port.type === "string" && typeof value !== "string") {
        return json(response, 400, {
          error: `${name}: expected string, got ${JSON.stringify(value)}`,
        });
      }
    }

    for (const [name, value] of Object.entries(values)) {
      entry.unset.delete(name);
      const port = entry.snapshot.ports.find((candidate) => candidate.name === name);
      if (port) port.value = value;
    }

    // Fed everything it was waiting for, it runs — as the real loop does when
    // its queue stops being empty.
    if (entry.unset.size === 0) {
      entry.snapshot.status = "running";
      entry.record.status = "running";
      maybeDrive(entry);
    }
    return json(response, 202, { run_id: runId, sent: Object.keys(values) });
  }

  function publish(entry, kind, payload, tag = null) {
    entry.sequence += 1;
    entry.snapshot.sequence = entry.sequence;
    const event = {
      protocol_version: 1,
      sequence: entry.sequence,
      team: entry.snapshot.team,
      tag,
      kind,
      payload,
    };
    const frame = `id: ${event.sequence}\nevent: ${kind}\ndata: ${JSON.stringify(event)}\n\n`;
    for (const subscriber of entry.subscribers) subscriber.write(frame);
  }

  /** Walk the captured topology the way a real run would. */
  async function driveRun(entry) {
    const wait = () => new Promise((resolve) => setTimeout(resolve, stepMs));
    await wait();
    publish(entry, "run_started", {});

    for (const [index, reaction] of entry.snapshot.reactions.entries()) {
      const tag = { timestamp: index + 1, microstep: 0 };
      entry.snapshot.current_tag = tag;
      publish(entry, "tag_advanced", {}, tag);
      await wait();

      reaction.status = "running";
      reaction.invocation_id = `inv-${index}`;
      publish(entry, "reaction_started", { reaction: reaction.id }, tag);
      await wait();

      reaction.status = "completed";
      for (const port of entry.snapshot.ports) {
        if (reaction.effects.includes(port.id)) {
          port.value = `${reaction.contract} value`;
          port.last_tag = tag;
        }
      }
      publish(entry, "reaction_completed", { reaction: reaction.id }, tag);
      await wait();
    }

    entry.snapshot.status = "completed";
    entry.record.status = "completed";
    entry.record.finished_at = Math.floor(Date.now() / 1000);
    publish(entry, "run_completed", { outputs: { result: "final answer" } });
  }

  await new Promise((resolve) => server.listen(port, host, resolve));
  return {
    url: `http://${host}:${server.address().port}`,
    agentToken,
    async close() {
      for (const client of terminals.clients) client.terminate();
      terminals.close();
      for (const entry of runs.values()) {
        for (const subscriber of entry.subscribers) subscriber.end();
      }
      for (const subscriber of chat.subscribers) subscriber.end();
      // Pooled keep-alive sockets would otherwise hold `close` open until they
      // idle out, adding seconds to every test.
      server.closeAllConnections();
      await new Promise((resolve) => server.close(resolve));
    },
  };
}

function readBody(request) {
  return new Promise((resolve, reject) => {
    let body = "";
    request.on("data", (chunk) => {
      body += chunk;
    });
    request.on("end", () => resolve(body));
    request.on("error", reject);
  });
}

function json(response, status, body) {
  const payload = JSON.stringify(body);
  response.writeHead(status, {
    ...CORS,
    "content-type": "application/json",
    "content-length": Buffer.byteLength(payload),
  });
  response.end(payload);
}

export { INVALID_MARKER };

// `node tests/fake-serve.mjs` for manual poking against `npm run dev`.
if (import.meta.url === `file://${process.argv[1]}`) {
  const fake = await startFakeServe();
  console.log(`fake omar serve: ${fake.url}`);
}
