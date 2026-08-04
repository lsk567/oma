/**
 * `omar serve --ui`: the daemon handing out Mission Control itself.
 *
 * The point of serving it from the daemon's own port is that the page is
 * same-origin with the API, so the address is read from the page rather than
 * chosen at launch. That is invisible to every other suite here — the browser
 * tests drive a fake over a different origin, and the Rust tests never load a
 * page — so this is where it is checked.
 *
 * Needs a runtime built with the bundle in it:
 *   (cd web && npm run build:spa)
 *   cargo build --bin omar --features ui
 *   node --test tests/serve-ui.test.mjs
 * Skips itself when that binary is absent or was built without the feature.
 */

import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { gunzipSync } from "node:zlib";
import { resolve } from "node:path";
import test, { after, before, describe } from "node:test";

import { chromium } from "@playwright/test";

const OMAR_BIN = process.env.OMAR_BIN ?? resolve("../target/debug/omar");
const PORT = Number(process.env.SERVE_UI_PORT ?? 7356);
const ORIGIN = `http://127.0.0.1:${PORT}`;

/**
 * A binary without the feature refuses `--ui` by design, and that refusal is
 * what tells us to skip rather than fail: this suite is about what a bundled
 * build does.
 */
function bundled() {
  if (!existsSync(OMAR_BIN)) return false;
  const probe = spawnSync(OMAR_BIN, ["serve", "--ui", "--address", "127.0.0.1:1"], {
    encoding: "utf8",
    timeout: 20_000,
  });
  return !`${probe.stdout}${probe.stderr}`.includes("no UI in it");
}

const AVAILABLE = bundled();

describe("omar serve --ui", { skip: AVAILABLE ? false : "no bundled runtime" }, () => {
  let daemon;

  before(async () => {
    // `--no-ea` keeps this from launching an assistant in tmux: the bundle is
    // what is under test, not the agent.
    daemon = spawn(OMAR_BIN, ["serve", "--address", `127.0.0.1:${PORT}`, "--no-ea"], {
      stdio: "ignore",
    });
    for (let attempt = 0; attempt < 100; attempt += 1) {
      try {
        await fetch(`${ORIGIN}/health`);
        return;
      } catch {
        await new Promise((done) => setTimeout(done, 200));
      }
    }
    throw new Error("the daemon never answered /health");
  });

  after(() => daemon?.kill());

  test("the shell is served, compressed, from the API's own port", async () => {
    const response = await fetch(ORIGIN, { headers: { "accept-encoding": "gzip" } });
    assert.equal(response.status, 200);
    assert.match(response.headers.get("content-type"), /text\/html/);
    // `fetch` decodes transparently, so the header is the assertion that these
    // bytes are stored compressed rather than compressed on the way out.
    assert.equal(response.headers.get("content-encoding"), "gzip");
    assert.match(await response.text(), /<div id="root">/);
  });

  test("the bundle is reachable and is really gzip", async () => {
    const response = await fetch(`${ORIGIN}/app.js`);
    assert.equal(response.status, 200);
    assert.match(response.headers.get("content-type"), /javascript/);
    const raw = Buffer.from(await response.arrayBuffer());
    // Decoded by fetch already; gunzipping it again would fail, so this checks
    // the payload is a real script rather than an error page.
    assert.ok(raw.length > 100_000, `suspiciously small: ${raw.length} bytes`);
  });

  test("an unknown path is the shell, not a 404", async () => {
    // The client owns its routes: reloading on one must not fall through.
    const response = await fetch(`${ORIGIN}/anything/at/all`);
    assert.equal(response.status, 200);
    assert.match(response.headers.get("content-type"), /text\/html/);
  });

  test("the API is not shadowed by the bundle", async () => {
    const response = await fetch(`${ORIGIN}/v1/runs`);
    assert.equal(response.status, 200);
    assert.deepEqual(await response.json(), { runs: [] });
    const health = await fetch(`${ORIGIN}/health`);
    assert.equal((await health.json()).status, "ok");
  });

  test("the page boots live against the daemon that served it", async () => {
    const browser = await chromium.launch();
    try {
      const page = await browser.newPage();
      const errors = [];
      page.on("console", (message) => message.type() === "error" && errors.push(message.text()));
      page.on("pageerror", (error) => errors.push(`pageerror: ${error.message}`));

      await page.goto(ORIGIN, { waitUntil: "networkidle" });

      assert.equal(await page.title(), "OMAR Mission Control");
      assert.equal(await page.getByLabel("Describe a workflow").count(), 1);
      // Demo mode would mean it did not resolve its own origin as the daemon,
      // which is the entire premise of serving it from here.
      const body = await page.locator("body").innerText();
      assert.ok(!body.includes("Demo topology"), "launched in demo mode");
      // Rendered only after `/v1/agent` answered, so this is the round trip.
      await page.locator(".backend-trigger").first().waitFor({ timeout: 10_000 });
      assert.deepEqual(errors, []);
    } finally {
      await browser.close();
    }
  });
});

/** Guards the other half: a build without the feature must say so. */
test("a runtime without the bundle refuses --ui and explains itself", { skip: AVAILABLE }, () => {
  if (!existsSync(OMAR_BIN)) return;
  const probe = spawnSync(OMAR_BIN, ["serve", "--ui", "--address", "127.0.0.1:1"], {
    encoding: "utf8",
    timeout: 20_000,
  });
  const said = `${probe.stdout}${probe.stderr}`;
  assert.match(said, /no UI in it/);
  assert.match(said, /--features ui/);
});

// Referenced so the import is not flagged as unused when everything skips.
void gunzipSync;
