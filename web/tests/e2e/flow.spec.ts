import { expect, test } from "@playwright/test";
import { FAKE_SERVE_PORT, FAKE_SERVE_URL } from "../../playwright.config";
import { startFakeServe } from "../fake-serve.mjs";

type FakeServe = { url: string; close(): Promise<void> };

let fake: FakeServe;

test.beforeEach(async () => {
  fake = (await startFakeServe({
    stepMs: 120,
    port: FAKE_SERVE_PORT,
  })) as FakeServe;
});

test.afterEach(async () => {
  await fake.close();
});

/**
 * The app is already launched pointing at the fake daemon, so this only waits
 * for the health probe to report it reachable.
 */
/** Walk the conversation until the assistant proposes a program. */
async function draftUntilProposed(page: import("@playwright/test").Page) {
  await page.getByLabel("Describe a workflow").fill("Review the release plan");
  await page.getByLabel("Draft workflow").click();
  await expect(page.locator(".messages")).toContainText("Which agent should own");
  await page.getByLabel("Describe a workflow").fill("The planner");
  await page.getByLabel("Draft workflow").click();
  await expect(page.getByRole("group", { name: "Deploy design" })).toBeVisible();
}

/** Deploying is deliberately two steps; tests must go through both. */
async function deploy(page: import("@playwright/test").Page) {
  await page.getByRole("button", { name: "Deploy", exact: true }).click();
  await page.getByRole("button", { name: "Confirm deploy" }).click();
}

/** Drag a divider horizontally by `dx` pixels. */
async function dragDivider(
  page: import("@playwright/test").Page,
  name: string,
  dx: number,
) {
  const divider = page.getByRole("separator", { name });
  const box = (await divider.boundingBox())!;
  const y = box.y + box.height / 2;
  await page.mouse.move(box.x + box.width / 2, y);
  await page.mouse.down();
  await page.mouse.move(box.x + box.width / 2 + dx, y, { steps: 10 });
  await page.mouse.up();
}

async function useFakeServe(page: import("@playwright/test").Page) {
  await page.goto("/");
  await expect(page.locator(".daemon")).toHaveClass(/live/);
  await expect(page.locator(".daemon")).toContainText(FAKE_SERVE_URL);
}

test("prompt to finished run, gated on an explicit confirmation", async ({ page }) => {
  await useFakeServe(page);
  const phase = page.locator(".connection");
  await expect(phase).toContainText("idle");

  // 1. The operator describes a workflow, and the assistant asks first.
  await page
    .getByLabel("Describe a workflow")
    .fill("Review the release plan before we ship");
  await page.getByLabel("Draft workflow").click();
  await expect(page.locator(".messages")).toContainText(
    "Review the release plan before we ship",
  );
  await expect(page.locator(".messages")).toContainText("Which agent should own");
  await expect(page.getByRole("group", { name: "Deploy design" })).toBeHidden();

  // 2. Answering it produces a design, and nothing has run yet.
  await page.getByLabel("Describe a workflow").fill("The planner");
  await page.getByLabel("Draft workflow").click();
  const confirm = page.getByRole("group", { name: "Deploy design" });
  await expect(confirm).toBeVisible();
  // Holding a design says nothing about the assistant, so the corner keeps
  // naming the backend rather than reporting on a run that has not started.
  await expect(page.locator(".composer-status")).toBeHidden();
  await expect(page.getByRole("button", { name: /Codex/ })).toBeVisible();
  await expect(phase).toContainText("review");
  await page.getByRole("button", { name: "Show the source pane" }).click();
  await expect(page.locator(".source-code")).toContainText("team ReviewFlow[");
  // Nothing is deployed yet, so there is no event stream to offer.
  await expect(page.locator(".tabs button")).toHaveText(["Source"]);

  // 3. Deploying is a dedicated action behind its own confirmation.
  await page.getByRole("button", { name: "Deploy", exact: true }).click();
  await page.getByRole("button", { name: "Confirm deploy" }).click();

  // 4. The run is spawned and observed over the event stream.
  await expect(phase).toContainText("observing");
  await expect(page.locator(".event-strip")).toContainText("run started");
  await expect(page.locator(".event-strip")).toContainText("reaction started");

  // 5. It finishes successfully.
  await expect(phase).toContainText("finished", { timeout: 30_000 });
  await expect(page.locator(".event-strip")).toContainText("run completed");
  // The run's recorded status lives with the source; the tabs switch back.
  await page.getByRole("tab", { name: "Source" }).click();
  await expect(page.locator(".source-title")).toContainText("completed");
});

test("the diagram reflects live reaction state from the run", async ({ page }) => {
  await useFakeServe(page);
  await draftUntilProposed(page);
  await deploy(page);
  await expect(page.locator(".connection")).toContainText("finished", {
    timeout: 30_000,
  });

  // Every reaction ends completed, so the renderer should show no idle chevrons
  // and the diagram should carry one per reaction in the captured topology.
  const reactions = page.locator(".omar-reaction");
  await expect(reactions).toHaveCount(3);
  await expect(page.locator(".omar-reaction.completed")).toHaveCount(3);
  await expect(page.locator(".omar-reaction.idle")).toHaveCount(0);
  // Reactions are anonymous in OMAR, so the agent is the headline.
  await expect(page.locator(".omar-reactions")).toContainText("planner");
  await expect(page.locator(".omar-reactions")).toContainText("reviewer");

  // Effects written during the run mark their action ports as carrying a value.
  await expect(page.locator(".omar-action.filled")).toHaveCount(2);
});

test("the run header reports how far behind it is, in readable units", async ({
  page,
}) => {
  await useFakeServe(page);
  await draftUntilProposed(page);
  await deploy(page);
  await expect(page.locator(".connection")).toContainText("finished", {
    timeout: 30_000,
  });

  const stats = page.locator(".run-stats");
  // Nanoseconds are what the wire carries and what nobody can read: the third
  // tag is at 3000000000, and 250000000 behind it.
  await expect(stats).toContainText("3s:0");
  await expect(stats).toContainText("250ms");
  await expect(stats).not.toContainText("3000000000");
  await expect(stats).not.toContainText("250000000");
  // A sequence number counted published messages, which answered a question
  // nobody was asking.
  await expect(stats).not.toContainText("SEQ");
  await expect(stats).toContainText("LAG");
});

test("a reaction's deadline is drawn as a stopwatch on its chevron", async ({
  page,
}) => {
  await useFakeServe(page);
  await draftUntilProposed(page);
  await expect(page.locator(".omar-reaction")).toHaveCount(3);

  // One reaction in the captured topology declares `within(5min)`; the other
  // two are bounded only by the run and carry nothing.
  const deadlines = page.locator(".omar-within");
  await expect(deadlines).toHaveCount(1);
  await expect(deadlines.locator(".omar-within-face")).toBeVisible();
  // The crown is what tells a stopwatch from the clock a timer already uses.
  // Attached rather than visible: it is a vertical line, so its bounding box
  // has no width and Playwright counts that as hidden.
  await expect(deadlines.locator(".omar-within-crown")).toBeAttached();
  // 300000000000ns divides exactly into five minutes, so that is the unit it
  // reads in — not 300s, and not the digits.
  await expect(deadlines.locator(".omar-within-meta")).toHaveText("5min");
  await expect(deadlines.locator("title")).toHaveText(
    "must answer within 5min",
  );

  // The chevron widens for it rather than the stopwatch landing on the name.
  const marked = page.locator(".omar-reaction", { has: deadlines });
  const plain = page.locator(".omar-reaction").first();
  const markedBox = (await marked.locator("polygon").boundingBox())!;
  const plainBox = (await plain.locator("polygon").boundingBox())!;
  expect(markedBox.width).toBeGreaterThan(plainBox.width);
});

test("a web component's panel opens from the diagram, and answers it", async ({
  page,
}) => {
  // Its own topology: one reaction on the `Web` backend. Kept apart from the
  // shared capture because a web agent parks until it is answered, and every
  // other test here drives a run to completion.
  await fake.close();
  fake = (await startFakeServe({
    stepMs: 30,
    port: FAKE_SERVE_PORT,
    snapshot: "diagram-web.v1.json",
  })) as FakeServe;
  await useFakeServe(page);
  await draftUntilProposed(page);
  await deploy(page);

  const panel = page.getByRole("dialog", { name: /Port panel for/ });
  // Not docked anywhere: nothing is on screen until the component is opened.
  await expect(panel).toBeHidden();

  // Drawn as reachable before it is touched: exactly one reaction on the
  // canvas is web-backed, and it is the only one a person can answer.
  const web = page.locator(".omar-reaction.web");
  await expect(web).toHaveCount(1, { timeout: 30_000 });
  await expect(page.locator(".omar-reaction")).toHaveCount(3);
  await expect(web).toContainText("reviewer");

  // Double-clicking a reaction opens the agent that runs it; this one has no
  // pane, so what opens is its panel.
  await web.dblclick();

  await expect(panel).toBeVisible();
  await expect(panel).toContainText("flow.reviewer");
  // Scoped to this component's wiring: what it reads, and what it may set.
  await expect(panel.locator(".port-list").first()).toContainText("flow.plan");
  await expect(panel).toContainText("flow.critique");

  // Only what this invocation is allowed to write, with its declared type.
  const waiting = panel.locator(".port-waiting");
  await expect(waiting).toHaveCount(1);
  await expect(waiting.locator(".port-field-name")).toHaveText("flow.critique");
  await expect(waiting.locator(".port-field-type")).toHaveText("string");

  await waiting.getByLabel("flow.critique (string)").fill("looks fine");
  await waiting.getByRole("button", { name: /^Send/ }).click();

  // Answered, so it leaves the queue; the panel stays open on the component.
  await expect(panel.locator(".port-waiting")).toHaveCount(0);
  await expect(panel).toContainText("Nothing is waiting on you");

  // And it closes, rather than living beside the events forever.
  await panel.getByRole("button", { name: "Close" }).click();
  await expect(panel).toBeHidden();
});

test("a delayed connection is solid, with its delay in a break in the line", async ({
  page,
}) => {
  // Captured from a real compile of tests/topology/src/PortManager.omar, whose
  // feedback carries a period so the loop closes over a tick.
  await fake.close();
  fake = (await startFakeServe({
    stepMs: 30,
    port: FAKE_SERVE_PORT,
    snapshot: "diagram-delay.v1.json",
  })) as FakeServe;
  await useFakeServe(page);
  await draftUntilProposed(page);

  // Both connections cost something, and each says what. `after 30s` is a
  // duration; `after 0` costs a microstep, which is a step in order rather
  // than in time and so is named instead of measured.
  const label = page.locator(".omar-edge-delay");
  await expect(label).toHaveCount(2);
  await expect(label).toHaveText(["\u03bcstep", "30s"]);

  // Solid, like any other connection: a stroke that could only say "some
  // delay" and never which is not worth spending the stroke on.
  const dashes = await page
    .locator(".omar-edge.connection")
    .first()
    .evaluate((node) => getComputedStyle(node).strokeDasharray);
  expect(["none", ""]).toContain(dashes);

  // The line breaks around the number rather than running through it, so a
  // connection carrying one is drawn as two paths where a plain one is drawn
  // as a single path. Both of these carry one.
  const connections = await page.locator(".omar-edge.connection").count();
  expect(connections).toBe(4);
});

test("Enter sends, modified Enter writes a new line", async ({ page }) => {
  await useFakeServe(page);
  const composer = page.getByLabel("Describe a workflow");

  await composer.fill("first line");
  await composer.press("Shift+Enter");
  await composer.pressSequentially("second line");
  // Still composing: a modified Enter must not send.
  await expect(composer).toHaveValue("first line\nsecond line");
  await expect(page.locator(".messages")).not.toContainText("first line");

  await composer.press("Enter");
  await expect(composer).toHaveValue("");
  await expect(page.locator(".messages")).toContainText("first line second line");
});

test("waiting holds one verb per turn and changes between turns", async ({
  page,
}) => {
  await fake.close();
  fake = (await startFakeServe({ stepMs: 3000, port: FAKE_SERVE_PORT })) as FakeServe;
  await useFakeServe(page);

  const waiting = page.locator(".waiting");
  const verb = waiting.locator(".waiting-verb");

  await page.getByLabel("Describe a workflow").fill("Review the release plan");
  await page.keyboard.press("Enter");
  await expect(waiting).toBeVisible();
  // Seconds under a minute; larger units only appear when they apply.
  await expect(waiting.locator(".waiting-elapsed")).toHaveText(/^\d{1,2}s$/);

  // The word must not change under the operator while one wait is in progress.
  const first = await verb.innerText();
  await expect(waiting.locator(".waiting-elapsed")).toHaveText(/^[1-9]\d*s$/);
  await expect(verb).toHaveText(first);

  await expect(page.locator(".messages")).toContainText("Which agent should own");
  await expect(waiting).toBeHidden();

  // A new turn is a new wait, and reads as one.
  await page.getByLabel("Describe a workflow").fill("The planner");
  await page.keyboard.press("Enter");
  await expect(waiting).toBeVisible();
  await expect(verb).not.toHaveText(first);
});

test("assistant markdown is rendered, not shown as syntax", async ({ page }) => {
  await useFakeServe(page);
  await draftUntilProposed(page);

  const body = page
    .locator(".message.assistant .message-body")
    .filter({ hasText: "three-reaction" });
  await expect(body.locator("strong")).toHaveText("three-reaction");
  await expect(body.locator("li")).toHaveCount(2);
  await expect(body.locator("li code").first()).toHaveText("planner");
  await expect(body.locator("pre code")).toContainText(
    "request -> planner -> reviewer -> result",
  );
  // The raw syntax must not survive into the rendered text.
  await expect(body).not.toContainText("**three-reaction**");
});

test("discarding a design leaves the gate without running anything", async ({
  page,
}) => {
  await useFakeServe(page);
  let admissions = 0;
  page.on("request", (request) => {
    if (request.method() === "POST" && request.url().includes("/v1/runs")) {
      admissions += 1;
    }
  });

  await draftUntilProposed(page);

  await page.getByRole("button", { name: "Discard" }).click();

  await expect(page.getByRole("group", { name: "Deploy design" })).toBeHidden();
  await expect(page.locator(".connection")).toContainText("idle");
  // The conversation is the runtime's record and is not rewritten by declining.
  await expect(page.locator(".messages")).toContainText("Which agent should own");
  expect(admissions, "declining a design must not admit a run").toBe(0);
});

test("an unreachable daemon is reported and blocks the run", async ({ page }) => {
  await useFakeServe(page);
  await draftUntilProposed(page);

  // Take the daemon away underneath a drafted design.
  await fake.close();

  await expect(page.locator(".daemon")).toHaveClass(/offline/, { timeout: 15_000 });
  await expect(page.locator(".daemon")).toContainText("unreachable");
  await expect(
    page.getByRole("button", { name: "Deploy", exact: true }),
  ).toBeDisabled();
  await expect(page.locator(".composer-status")).toContainText("Cannot reach omar serve");
});

test("a design the compiler rejects is reported and stays unconfirmed", async ({
  page,
}) => {
  await useFakeServe(page);
  await draftUntilProposed(page);

  // Make the daemon reject the program the way omarc would.
  await page.route("**/v1/runs", async (route) => {
    if (route.request().method() !== "POST") return route.fallback();
    await route.fulfill({
      status: 400,
      contentType: "application/json",
      body: JSON.stringify({ error: "omarc failed: expected identifier" }),
    });
  });

  await deploy(page);

  await expect(page.locator(".connection-error")).toContainText("omarc failed");
  // Still at the gate, so the operator can fix and retry rather than losing it.
  await expect(page.locator(".connection")).toContainText("review");
  await expect(
    page.getByRole("button", { name: "Deploy", exact: true }),
  ).toBeVisible();
});

test("a feedback loop renders, and every arrow still points right", async ({
  page,
}) => {
  // A topology whose reaction both triggers on an action and writes back to it.
  await fake.close();
  fake = (await startFakeServe({
    stepMs: 120,
    port: FAKE_SERVE_PORT,
    snapshot: "diagram-feedback.v1.json",
  })) as FakeServe;

  await useFakeServe(page);
  await draftUntilProposed(page);

  // ELK throws on some cyclic graphs; losing a layout candidate must not lose
  // the diagram.
  await expect(page.locator(".diagram-error")).toHaveCount(0);
  await expect(page.locator(".omar-reaction")).toHaveCount(1);
  const edges = page.locator(".omar-edge");
  await expect(edges).toHaveCount(4);

  // Flow reads left to right, so a feedback edge wraps around rather than
  // doubling back: every arrowhead must sit on a rightward final segment.
  const finals = await edges.evaluateAll((paths) =>
    paths.map((path) => {
      const length = (path as SVGPathElement).getTotalLength();
      const before = (path as SVGPathElement).getPointAtLength(length - 2);
      const end = (path as SVGPathElement).getPointAtLength(length);
      return { dx: end.x - before.x, dy: end.y - before.y };
    }),
  );
  for (const { dx, dy } of finals) {
    expect(
      dx > Math.abs(dy),
      `final segment should run rightward, got dx=${dx} dy=${dy}`,
    ).toBe(true);
  }
});

test("deploying is armed, reversible, and only then shows events", async ({
  page,
}) => {
  await useFakeServe(page);
  let admissions = 0;
  page.on("request", (request) => {
    if (request.method() === "POST" && request.url().includes("/v1/runs")) {
      admissions += 1;
    }
  });
  await draftUntilProposed(page);

  // Nothing is deployed, so there are no events to look at.
  await expect(page.locator(".tabs button")).toHaveText(["Source"]);

  // Arming the deploy does not deploy.
  await page.getByRole("button", { name: "Deploy", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "Confirm deploy" }),
  ).toBeVisible();
  expect(admissions, "arming must not admit a run").toBe(0);

  // And it can be backed out of.
  await page.getByRole("button", { name: "Cancel" }).click();
  await expect(
    page.getByRole("button", { name: "Deploy", exact: true }),
  ).toBeVisible();
  expect(admissions).toBe(0);

  await deploy(page);
  expect(admissions).toBe(1);

  // Deployed: events exist, and the view moves to them.
  await expect(page.locator(".tabs button")).toHaveText(["Source", "Events"]);
  await expect(page.locator(".tabs button.active")).toHaveText("Events");
  await expect(page.locator(".event-strip")).toContainText("run started");
});

test("the source view highlights OMAR, and its pane can be hidden", async ({
  page,
}) => {
  await useFakeServe(page);
  await draftUntilProposed(page);

  await page.getByRole("button", { name: "Show the source pane" }).click();
  const code = page.locator(".source-code");
  await expect(code).toBeVisible();
  await expect(code.locator(".omar-tok-keyword").first()).toHaveText("team");
  // Backends and types are distinguished, and prompt interpolation stands out.
  await expect(code.locator(".omar-tok-agent").filter({ hasText: "Codex" }).first()).toBeVisible();
  await expect(code.locator(".omar-tok-type").first()).toHaveText("string");
  await expect(code.locator(".omar-tok-interpolation").first()).toContainText("$(");

  // Dragging the divider to the edge collapses the pane and gives the diagram
  // the width; the divider is left behind as the way to bring it back.
  const diagram = page.locator(".diagram-panel");
  const wide = (await diagram.boundingBox())!.width;
  await dragDivider(page, "Resize the source pane", 600);
  await expect(page.locator(".inspector-panel")).toBeHidden();
  expect((await diagram.boundingBox())!.width).toBeGreaterThan(wide);

  await page.getByRole("button", { name: "Show the source pane" }).click();
  await expect(code).toBeVisible();
});

test("the columns can be resized from either divider", async ({ page }) => {
  await useFakeServe(page);
  await draftUntilProposed(page);

  // The source pane starts collapsed, so open it before resizing it.
  await page.getByRole("button", { name: "Show the source pane" }).click();
  const builder = page.locator(".builder-panel");
  const inspector = page.locator(".inspector-panel");
  await expect(inspector).toBeVisible();
  const before = {
    builder: (await builder.boundingBox())!.width,
    inspector: (await inspector.boundingBox())!.width,
  };

  // The conversation starts at half the window, so narrow it first — there is
  // no room to widen until something gives.
  await dragDivider(page, "Resize the conversation", -120);
  const narrowed = (await builder.boundingBox())!.width;
  expect(narrowed).toBeLessThan(before.builder - 90);

  // And back the other way.
  await dragDivider(page, "Resize the conversation", 60);
  expect((await builder.boundingBox())!.width).toBeGreaterThan(narrowed + 40);

  // The source divider runs the other way: dragging left widens the pane.
  await dragDivider(page, "Resize the source pane", -80);
  expect((await inspector.boundingBox())!.width).toBeGreaterThan(
    before.inspector + 50,
  );

  // Keyboard works too, so the layout is not mouse-only.
  const widened = (await inspector.boundingBox())!.width;
  const right = page.getByRole("separator", { name: "Resize the source pane" });
  await right.focus();
  await right.press("ArrowRight");
  expect((await inspector.boundingBox())!.width).toBeLessThan(widened);

  // A drag can never squeeze the diagram out of existence.
  const diagram = page.locator(".diagram-panel");
  await dragDivider(page, "Resize the conversation", 4000);
  expect((await diagram.boundingBox())!.width).toBeGreaterThanOrEqual(300);
});

test("a comment after a proposal does not retract the deploy gate", async ({
  page,
}) => {
  await useFakeServe(page);
  await draftUntilProposed(page);

  // Assistants comment right after proposing. Treating that as "still
  // drafting" hid the gate, so the design became undeployable.
  await expect(page.locator(".messages")).toContainText("in your queue for approval");
  await expect(page.getByRole("group", { name: "Deploy design" })).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Deploy", exact: true }),
  ).toBeVisible();
  await expect(page.locator(".connection")).toContainText("review");
});

test("dragging a panel away collapses it, and its divider brings it back", async ({
  page,
}) => {
  await useFakeServe(page);
  await draftUntilProposed(page);

  const conversation = page.locator(".builder-panel");
  const diagram = page.locator(".diagram-panel");
  await expect(conversation).toBeVisible();
  const before = (await diagram.boundingBox())!.width;

  // There are no layout buttons: the divider is the whole control.
  await dragDivider(page, "Resize the conversation", -600);
  await expect(conversation).toBeHidden();
  expect((await diagram.boundingBox())!.width).toBeGreaterThan(before);

  // What is left behind is a slim handle pointing back at what it hides.
  const handle = page.getByRole("button", { name: "Show the conversation" });
  await expect(handle).toBeVisible();
  expect((await handle.boundingBox())!.width).toBeLessThan(24);

  await handle.click();
  await expect(conversation).toBeVisible();
  expect((await conversation.boundingBox())!.width).toBeGreaterThan(200);
});

test("the conversation owns the window until a design splits it", async ({
  page,
}) => {
  await useFakeServe(page);

  // Nothing to show beside the conversation yet.
  const conversation = page.locator(".builder-panel");
  const viewport = page.viewportSize()!;
  expect((await conversation.boundingBox())!.width).toBeGreaterThan(
    viewport.width - 40,
  );
  await expect(page.locator(".diagram-panel")).toHaveCount(0);

  await draftUntilProposed(page);

  // The first design splits the window down the middle, diagram on the right.
  const conversationBox = (await conversation.boundingBox())!;
  const diagramBox = (await page.locator(".diagram-panel").boundingBox())!;
  expect(Math.abs(conversationBox.width - viewport.width / 2)).toBeLessThan(60);
  expect(diagramBox.x).toBeGreaterThan(conversationBox.x);
  // And it is a pure diagram: the source pane waits behind its handle.
  await expect(page.locator(".inspector-panel")).toBeHidden();
  await expect(
    page.getByRole("button", { name: "Show the source pane" }),
  ).toBeVisible();
});

test("the studio opens on a centred prompt, then settles into a thread", async ({
  page,
}) => {
  await useFakeServe(page);

  // Nothing said and no topology: the prompt is the whole screen, the way a
  // chat client opens, rather than pinned under an empty thread.
  const composer = page.locator(".prompt-box");
  const viewport = page.viewportSize()!;
  const opening = (await composer.boundingBox())!;
  const middle = opening.y + opening.height / 2;
  expect(Math.abs(middle - viewport.height / 2)).toBeLessThan(viewport.height / 4);
  await expect(page.locator(".builder-panel")).toHaveClass(/opening/);

  // A compact box, like the one a chat client opens with — a full-width slab
  // five lines deep asks for an essay. Measured against claude.ai at the same
  // window size: 498x93.
  expect(opening.width).toBe(500);
  expect(opening.height).toBeLessThan(110);

  await composer.getByLabel("Describe a workflow").fill("Review the release plan");
  await page.keyboard.press("Enter");

  // Once there is a conversation it belongs at the bottom, under the thread.
  await expect(page.locator(".messages")).toContainText("Review the release plan");
  await expect(page.locator(".builder-panel")).not.toHaveClass(/opening/);
  const threaded = (await composer.boundingBox())!;
  expect(threaded.y).toBeGreaterThan(opening.y + 100);
  // The compact size belongs to the opening screen only; in a thread the box
  // spans the column, so a reply can be written at length.
  expect(threaded.width).toBeGreaterThan(opening.width);

  // And the thread stays a centred column rather than stretching across the
  // window, which is unreadable at this width.
  const message = (await page.locator(".message").first().boundingBox())!;
  expect(message.width).toBeLessThan(viewport.width * 0.6);
  const messageCentre = message.x + message.width / 2;
  expect(Math.abs(messageCentre - viewport.width / 2)).toBeLessThan(40);
});

test("the composer's buttons line up", async ({ page }) => {
  await useFakeServe(page);
  await draftUntilProposed(page);

  // Discard, Deploy and send sit in a row; differing heights read as sloppy.
  const boxes = await Promise.all(
    [
      page.getByRole("button", { name: "Discard" }),
      page.getByRole("button", { name: "Deploy", exact: true }),
      page.getByLabel("Draft workflow"),
    ].map(async (button) => (await button.boundingBox())!),
  );
  const [first] = boxes;
  for (const box of boxes) {
    expect(Math.round(box.height)).toBe(Math.round(first.height));
    expect(Math.round(box.y)).toBe(Math.round(first.y));
  }
});

test("discarding puts the topology away too", async ({ page }) => {
  await useFakeServe(page);
  await draftUntilProposed(page);
  await expect(page.locator(".diagram-panel")).toBeVisible();

  await page.getByRole("button", { name: "Discard" }).click();

  // Leaving the diagram up implies a design is still in play.
  await expect(page.locator(".diagram-panel")).toHaveCount(0);
  await expect(page.locator(".builder-panel")).toHaveClass(/solo/);

  // And the next proposal brings it back, split as the first one was.
  await draftUntilProposed(page);
  await expect(page.locator(".diagram-panel")).toBeVisible();
});

test("a zoomed diagram stays put while the run updates it", async ({ page }) => {
  await useFakeServe(page);
  await draftUntilProposed(page);

  const view = page.locator(".diagram-canvas > g");
  await expect(view).toBeVisible();
  // Zoom in deliberately, the way an operator inspecting a reaction would.
  await page.getByRole("button", { name: "Zoom in" }).click();
  await page.getByRole("button", { name: "Zoom in" }).click();
  const zoomed = (await view.getAttribute("transform"))!;

  // The run republishes the snapshot on every event; none of that should
  // reclaim the view.
  await deploy(page);
  await expect(page.locator(".connection")).toContainText("finished", {
    timeout: 30_000,
  });
  expect(await view.getAttribute("transform")).toBe(zoomed);

  // Fit is still one click away when it is actually wanted. It eases back
  // rather than cutting, so this settles rather than reading straight away.
  await page.getByRole("button", { name: "Fit" }).click();
  await expect
    .poll(() => view.getAttribute("transform"), { timeout: 3000 })
    .not.toBe(zoomed);
});

test("commentary keeps the wait alive rather than ending it", async ({ page }) => {
  await fake.close();
  fake = (await startFakeServe({ stepMs: 3000, port: FAKE_SERVE_PORT })) as FakeServe;
  await useFakeServe(page);

  await page.getByLabel("Describe a workflow").fill("Review the release plan");
  await page.keyboard.press("Enter");

  // The assistant narrates while it works. That is the point of the feature —
  // ending the wait on it would put the operator back in the dark.
  await expect(page.locator(".message.progress")).toContainText(
    "Reading the ports",
  );
  await expect(page.locator(".waiting")).toBeVisible();
  await expect(page.locator(".connection")).toContainText("drafting");

  // A real reply does end it.
  await expect(page.locator(".messages")).toContainText("Which agent should own");
  await expect(page.locator(".waiting")).toBeHidden();
  await expect(page.locator(".connection")).toContainText("idle");

  // Commentary is present but recessive, so it does not compete with the reply.
  const commentary = page.locator(".message.progress").first();
  const reply = page.locator(".message.assistant:not(.progress)").first();
  const sizeOf = async (row: typeof commentary) =>
    row.locator(".message-body").evaluate((el) => getComputedStyle(el).fontSize);
  expect(parseFloat(await sizeOf(commentary))).toBeLessThan(
    parseFloat(await sizeOf(reply)),
  );
});

test("selecting components names them for the assistant", async ({ page }) => {
  await useFakeServe(page);

  // A design has to exist before there is anything to point at.
  await page.getByLabel("Describe a workflow").fill("Review the release plan");
  await page.getByLabel("Draft workflow").click();
  await page.getByLabel("Describe a workflow").fill("The planner");
  await page.getByLabel("Draft workflow").click();
  await expect(page.getByRole("group", { name: "Deploy design" })).toBeVisible();

  // Nothing is selected until the operator says so.
  await expect(page.locator(".selection-bar")).toBeHidden();

  const port = page.locator(".omar-port-group").first();
  await port.locator(".omar-port").click();
  await expect(page.locator(".selection-bar")).toContainText("selected");
  await expect(port).toHaveClass(/selected/);

  const reaction = page.locator(".omar-reaction").first();
  await reaction.locator(".omar-reaction-body").click();
  const bar = page.locator(".selection-label");
  const listed = (await bar.textContent()) ?? "";
  expect(listed.startsWith("[") && listed.includes(",")).toBe(true);

  // Clicking again takes it back out.
  await reaction.locator(".omar-reaction-body").click();
  await expect(reaction).not.toHaveClass(/selected/);

  // The selection rides along with the message rather than being decoration.
  let sent: string[] | undefined;
  await page.route("**/v1/chat", async (route) => {
    if (route.request().method() === "POST") {
      sent = JSON.parse(route.request().postData() ?? "{}").selection;
    }
    await route.fallback();
  });
  await page.getByLabel("Describe a workflow").fill("make this one retry");
  await page.getByLabel("Draft workflow").click();

  await expect(page.locator(".messages")).toContainText("make this one retry");
  expect(sent).toHaveLength(1);
  // And the thread keeps it, so the exchange still reads later.
  await expect(page.locator(".message.operator").last()).toContainText(
    `[${sent?.[0]}]`,
  );

  // Sending consumes it; the next message must not inherit it silently.
  await expect(page.locator(".selection-bar")).toBeHidden();
});

test("dragging the canvas pans it rather than selecting what is under it", async ({
  page,
}) => {
  // Selection made the canvas claim the pointer only once a drag starts, since
  // a captured pointer delivers the click to the canvas instead of the node.
  // Panning has to keep working across that change.
  await useFakeServe(page);
  await page.getByLabel("Describe a workflow").fill("Review the release plan");
  await page.getByLabel("Draft workflow").click();
  await page.getByLabel("Describe a workflow").fill("The planner");
  await page.getByLabel("Draft workflow").click();
  await expect(page.getByRole("group", { name: "Deploy design" })).toBeVisible();

  const scene = page.locator(".diagram-canvas > g").first();
  const before = await scene.getAttribute("transform");

  // Start the drag on a node, so this also proves a drag is not a click.
  const port = page.locator(".omar-port-group .omar-port").first();
  const box = (await port.boundingBox())!;
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  await page.mouse.move(box.x + box.width / 2 + 90, box.y + box.height / 2 + 40, {
    steps: 12,
  });
  await page.mouse.up();

  expect(await scene.getAttribute("transform")).not.toBe(before);
  await expect(page.locator(".selection-bar")).toBeHidden();
});

test("double clicking a running reaction opens its agent's terminal", async ({
  page,
}) => {
  await useFakeServe(page);

  // Agents only exist once a run is going, so get there first.
  await page.getByLabel("Describe a workflow").fill("Review the release plan");
  await page.getByLabel("Draft workflow").click();
  await page.getByLabel("Describe a workflow").fill("The planner");
  await page.getByLabel("Draft workflow").click();
  await page.getByRole("button", { name: "Deploy", exact: true }).click();
  await page.getByRole("button", { name: "Confirm deploy" }).click();
  await expect(page.locator(".connection")).toContainText("observing");

  const terminal = page.getByRole("dialog");
  await expect(terminal).toBeHidden();

  await page.locator(".omar-reaction .omar-reaction-body").first().dblclick();
  await expect(terminal).toBeVisible();

  // The viewer adopts the agent's geometry rather than imposing its own.
  await expect(terminal).toContainText(/\d+×\d+ · attached/);
  // The agent's screen arrives, and the heading names whose it is.
  await expect(terminal.locator(".xterm")).toContainText("flow.planner");

  // Typing goes up the same socket; the fake echoes it back as screen output.
  await page.keyboard.type("whoami");
  await expect(terminal.locator(".xterm")).toContainText("whoami");

  // Closing detaches and leaves the studio as it was.
  await page.getByRole("button", { name: "Close terminal" }).click();
  await expect(terminal).toBeHidden();
  await expect(page.locator(".connection")).toContainText(/observing|finished/);
});

test("a finished run leaves nothing running and still opens a terminal", async ({
  page,
}) => {
  await useFakeServe(page);
  await page.getByLabel("Describe a workflow").fill("Review the release plan");
  await page.getByLabel("Draft workflow").click();
  await page.getByLabel("Describe a workflow").fill("The planner");
  await page.getByLabel("Draft workflow").click();
  await page.getByRole("button", { name: "Deploy", exact: true }).click();
  await page.getByRole("button", { name: "Confirm deploy" }).click();
  await expect(page.locator(".connection")).toContainText("finished");

  // The per-run diagram server dies with the run, so the refetch that follows
  // the closing events can lose the race. The events themselves say what
  // happened, and nothing can still be running once the run is over.
  await expect(page.locator(".omar-reaction.running")).toHaveCount(0);
  const reactions = page.locator(".omar-reaction");
  await expect(reactions).not.toHaveCount(0);
  for (const reaction of await reactions.all()) {
    await expect(reaction).toContainText("completed");
  }

  // The agents outlive the run, so their terminals are still reachable.
  await page.locator(".omar-reaction .omar-reaction-body").first().dblclick();
  await expect(page.getByRole("dialog")).toBeVisible();
});

test("each instance is drawn as its own container", async ({ page }) => {
  // Captured from a real run of a two-team program, so this is the runtime's
  // own answer rather than a hand-written shape.
  await fake.close();
  fake = (await startFakeServe({
    stepMs: 30,
    port: FAKE_SERVE_PORT,
    snapshot: "diagram-two-instances.v1.json",
  })) as FakeServe;
  await useFakeServe(page);
  await page.getByLabel("Describe a workflow").fill("smoke test");
  await page.getByLabel("Draft workflow").click();
  await page.getByLabel("Describe a workflow").fill("go");
  await page.getByLabel("Draft workflow").click();

  // One box per instantiation, each titled with the instance and subtitled
  // with the team it came from.
  const containers = page.locator(".omar-team");
  await expect(containers).toHaveCount(2);
  // The runtime reports instances in name order; the layout places them by
  // data flow, so compare the set rather than the sequence.
  const names = await page.locator(".omar-team-name").allTextContents();
  expect(names.sort()).toEqual(["checker", "producer"]);
  const subtitles = await page.locator(".omar-team-kind").allTextContents();
  expect(subtitles.join(" ")).toMatch(/Producer/);
  expect(subtitles.join(" ")).toMatch(/Checker/);

  // Ports read locally: the container already says `producer`, so repeating it
  // on every port is what pushed the labels into the shapes.
  const labels = await page.locator(".omar-port-label").allTextContents();
  expect(labels.sort()).toEqual(["draft", "draft", "request", "result"]);

  // And the containers are genuinely apart, not one box drawn twice: the
  // producer's box ends before the checker's begins.
  const spans = await Promise.all(
    (await containers.all()).map(async (box) => (await box.boundingBox())!),
  );
  spans.sort((left, right) => left.x - right.x);
  expect(spans[0].x + spans[0].width).toBeLessThanOrEqual(spans[1].x + 1);
});

test("an edge inside a container is drawn inside it", async ({ page }) => {
  // ELK reports an edge's route relative to the deepest node holding both of
  // its ends, whatever node the edge was declared on. Declaring them all at the
  // root offset every inside-a-container edge by the canvas origin instead, so
  // they were drawn as long diagonals across the whole diagram.
  await fake.close();
  fake = (await startFakeServe({
    stepMs: 30,
    port: FAKE_SERVE_PORT,
    snapshot: "diagram-five-instances.v1.json",
  })) as FakeServe;
  await useFakeServe(page);
  await page.getByLabel("Describe a workflow").fill("five containers");
  await page.getByLabel("Draft workflow").click();
  await page.getByLabel("Describe a workflow").fill("go");
  await page.getByLabel("Draft workflow").click();
  await expect(page.locator(".omar-team")).toHaveCount(5);

  const auditor = (await page
    .locator(".omar-team")
    .filter({ hasText: "auditor" })
    .first()
    .boundingBox())!;

  // Every edge from one of the auditor's own ports to its own reaction has to
  // stay within the auditor's box.
  const inside = page.locator(
    '[data-id^="trigger::auditor."][data-id$="auditor.reaction.0"]',
  );
  await expect(inside).not.toHaveCount(0);
  for (const edge of await inside.all()) {
    const box = (await edge.boundingBox())!;
    expect(box.x).toBeGreaterThanOrEqual(auditor.x - 20);
    expect(box.x + box.width).toBeLessThanOrEqual(auditor.x + auditor.width + 20);
  }

  // And no port sits on the container's own title.
  const title = (await page
    .locator(".omar-team")
    .filter({ hasText: "auditor" })
    .locator(".omar-team-kind")
    .first()
    .boundingBox())!;
  for (const label of await page.locator(".omar-port-label").all()) {
    const box = (await label.boundingBox())!;
    const overlaps =
      box.x < title.x + title.width &&
      box.x + box.width > title.x &&
      box.y < title.y + title.height &&
      box.y + box.height > title.y;
    expect(overlaps).toBe(false);
  }
});

test("the composer says which assistant is answering, and can change it", async ({
  page,
}) => {
  await useFakeServe(page);

  // With no run to report on, the corner names the backend instead.
  const trigger = page.getByRole("button", { name: /Codex/ });
  await expect(trigger).toBeVisible();
  await expect(page.locator(".composer-status")).toBeHidden();

  await trigger.click();
  const menu = page.getByRole("menu");
  await expect(menu).toBeVisible();
  await expect(menu.getByRole("menuitem")).toHaveText([
    "Claude Code",
    "Codex✓",
    "Cursor",
    "opencode",
    "agy",
  ]);

  // It opens upward: it lives at the bottom of the composer.
  const menuBox = (await menu.boundingBox())!;
  const triggerBox = (await trigger.boundingBox())!;
  expect(menuBox.y + menuBox.height).toBeLessThanOrEqual(triggerBox.y + 1);

  // Choosing another restarts the assistant, so it asks first.
  await menu.getByRole("menuitem", { name: "Claude Code" }).click();
  await expect(page.locator(".backend-confirm")).toContainText(
    "session is lost",
  );
  await page.getByRole("button", { name: "Restart" }).click();

  await expect(page.getByRole("button", { name: /Claude Code/ })).toBeVisible();
});

test("a chevron names the backend behind its agent", async ({ page }) => {
  await useFakeServe(page);
  await page.getByLabel("Describe a workflow").fill("Review the release plan");
  await page.getByLabel("Draft workflow").click();
  await page.getByLabel("Describe a workflow").fill("The planner");
  await page.getByLabel("Draft workflow").click();

  // Which model is behind an agent changes how you read what it did.
  await expect(page.locator(".omar-reaction-meta").first()).toContainText(
    "Codex",
  );
});

test("a design from a runtime without instances still renders", async ({
  page,
}) => {
  // The preview arrives on the chat message, not from the diagram endpoint, so
  // it takes a different path into the renderer. Validating it without keeping
  // what the validation normalises left it missing the fields a newer client
  // expects, and the canvas died on the first one it touched.
  await fake.close();
  fake = (await startFakeServe({
    stepMs: 30,
    port: FAKE_SERVE_PORT,
    snapshot: "diagram-legacy.v1.json",
  })) as FakeServe;
  await useFakeServe(page);
  await page.getByLabel("Describe a workflow").fill("Review the release plan");
  await page.getByLabel("Draft workflow").click();
  await page.getByLabel("Describe a workflow").fill("The planner");
  await page.getByLabel("Draft workflow").click();

  await expect(page.getByRole("group", { name: "Deploy design" })).toBeVisible();
  await expect(page.locator(".diagram-error")).toBeHidden();
  // Such a program draws as one container named after itself, as it always did.
  await expect(page.locator(".omar-team")).toHaveCount(1);
  await expect(page.locator(".omar-reaction")).not.toHaveCount(0);
});

test("a message with a selection still reads as a paragraph", async ({
  page,
}) => {
  // The message grid has two columns: a 28px label and the rest. A third child
  // landed the text in the label's column, so it wrapped to a word per line.
  await useFakeServe(page);
  await page.getByLabel("Describe a workflow").fill("Review the release plan");
  await page.getByLabel("Draft workflow").click();
  await page.getByLabel("Describe a workflow").fill("The planner");
  await page.getByLabel("Draft workflow").click();
  await expect(page.getByRole("group", { name: "Deploy design" })).toBeVisible();

  await page.locator(".omar-reaction .omar-reaction-body").first().click();
  await expect(page.locator(".selection-bar")).toBeVisible();
  const sentence = "Change this to open code";
  await page.getByLabel("Describe a workflow").fill(sentence);
  await page.getByLabel("Draft workflow").click();

  const message = page.locator(".message.operator").last();
  await expect(message).toContainText(sentence);
  // A sentence that fits on one or two lines, not one line per word.
  const text = message.locator("p").last();
  const box = (await text.boundingBox())!;
  const lineHeight = Number(
    await text.evaluate((el) => parseFloat(getComputedStyle(el).lineHeight)),
  );
  expect(box.height).toBeLessThan(lineHeight * 3);
  // And it uses the width it has rather than the label's column.
  const body = (await message.locator(".message-content").boundingBox())!;
  expect(box.width).toBeGreaterThan(body.width * 0.5);
});

test("Fit eases the view back rather than cutting to it", async ({ page }) => {
  await useFakeServe(page);
  await page.getByLabel("Describe a workflow").fill("Review the release plan");
  await page.getByLabel("Draft workflow").click();
  await page.getByLabel("Describe a workflow").fill("The planner");
  await page.getByLabel("Draft workflow").click();

  const scene = page.locator(".diagram-canvas > g").first();
  const scaleOf = async () =>
    Number(
      ((await scene.getAttribute("transform")) ?? "").match(
        /scale\(([\d.]+)\)/,
      )?.[1] ?? 0,
    );

  const fitted = await scaleOf();
  // Zoom well away from the fit so the return has distance to cover.
  for (let i = 0; i < 6; i += 1) {
    await page.getByRole("button", { name: "Zoom out" }).click();
  }
  const zoomedOut = await scaleOf();
  expect(zoomedOut).toBeLessThan(fitted);

  await page.getByRole("button", { name: "Fit" }).click();
  // Sampled mid-flight: on the way back, not already arrived and not still put.
  await page.waitForTimeout(120);
  const midway = await scaleOf();
  expect(midway).toBeGreaterThan(zoomedOut);
  expect(midway).toBeLessThan(fitted);

  // And it does arrive.
  await expect
    .poll(scaleOf, { timeout: 3000 })
    .toBeCloseTo(fitted, 2);
});

/** Straight runs of a path, with the pen tracked through the corner arcs. */
function pathSegments(d: string) {
  const runs: { a: [number, number]; b: [number, number] }[] = [];
  let pen: [number, number] | null = null;
  for (const token of d.match(/[MLQ][^MLQ]*/g) ?? []) {
    const nums = (token.slice(1).trim().match(/-?[\d.]+/g) ?? []).map(Number);
    if (token[0] === "M") {
      pen = [nums[0], nums[1]];
    } else if (token[0] === "L") {
      const next: [number, number] = [nums[0], nums[1]];
      if (pen) runs.push({ a: pen, b: next });
      pen = next;
    } else {
      // Q is the rounded corner itself; the pen ends at its endpoint.
      pen = [nums[2], nums[3]];
    }
  }
  return runs;
}

async function showFanIn(page: import("@playwright/test").Page) {
  await fake.close();
  fake = (await startFakeServe({
    stepMs: 30,
    port: FAKE_SERVE_PORT,
    snapshot: "diagram-fan-in.v1.json",
  })) as FakeServe;
  // eslint-disable-next-line react-hooks/rules-of-hooks -- a page helper, not a hook
  await useFakeServe(page);
  await page.getByLabel("Describe a workflow").fill("fan in");
  await page.getByLabel("Draft workflow").click();
  await page.getByLabel("Describe a workflow").fill("go");
  await page.getByLabel("Draft workflow").click();
  await expect(page.locator(".omar-team")).toHaveCount(8);
}

test("every edge runs on an axis", async ({ page }) => {
  // Ports are snapped onto their container's border and spread to clear its
  // title, which moves them off the point ELK routed to. Without squaring the
  // path off afterwards the last hop arrives on a slant.
  await showFanIn(page);

  const paths = await page
    .locator(".omar-edge")
    .evaluateAll((els) => els.map((el) => el.getAttribute("d") ?? ""));
  expect(paths.length).toBeGreaterThan(20);

  const tilted = paths.flatMap((d) =>
    pathSegments(d).filter(
      (run) =>
        Math.abs(run.a[0] - run.b[0]) > 0.6 &&
        Math.abs(run.a[1] - run.b[1]) > 0.6,
    ),
  );
  expect(tilted).toHaveLength(0);
});

test("a program wider than the panel still fits in it", async ({ page }) => {
  // The fit was clamped by the same floor as the zoom buttons, so a large
  // topology could not be shrunk enough to see whole.
  await showFanIn(page);
  await page.getByRole("button", { name: "Fit" }).click();
  await page.waitForTimeout(600);

  const panel = (await page.locator(".diagram-wrap").boundingBox())!;
  const scene = (await page
    .locator(".diagram-canvas > g")
    .first()
    .boundingBox())!;

  expect(scene.width).toBeLessThanOrEqual(panel.width + 1);
  expect(scene.height).toBeLessThanOrEqual(panel.height + 1);
});

test("an edge into a reaction actually reaches it", async ({ page }) => {
  // Reserving the title band on the border moved the ports but not the
  // children ELK routes to, so edges ran past the chevrons at the ports'
  // height and the chevrons read as unconnected.
  await showFanIn(page);

  const reactions = await page.locator(".omar-reaction").all();
  expect(reactions.length).toBeGreaterThan(5);
  for (const reaction of reactions) {
    const id = await reaction.locator(".omar-reaction-name").textContent();
    const box = (await reaction.boundingBox())!;
    const edges = page.locator(`[data-id$="${id}"]`);
    expect(await edges.count()).toBeGreaterThan(0);
    for (const edge of await edges.all()) {
      const line = (await edge.boundingBox())!;
      // The edge has to arrive within the chevron's own band, not above or
      // below it.
      expect(line.y + line.height).toBeGreaterThanOrEqual(box.y - 2);
      expect(line.y).toBeLessThanOrEqual(box.y + box.height + 2);
    }
  }
});

test("the timeline projects a program before anything is deployed", async ({
  page,
}) => {
  // The determinism claim, made checkable: what a program will do is decided
  // by the program, so it can be shown before it has done it.
  await useFakeServe(page);
  await draftUntilProposed(page);

  await page.getByRole("button", { name: "▲ Timeline" }).click();
  const timeline = page.getByLabel("Logical timeline");
  await expect(timeline).toBeVisible();

  // Nothing has run, and the tags are there anyway.
  await expect(timeline).toContainText("projected");
  await expect(timeline.locator(".timeline-tag")).toHaveText(/^\d+:\d+$/);

  // Stepping moves through tags, and the diagram marks what each one touches.
  const scrubber = timeline.getByLabel("Logical tag");
  await expect(scrubber).toHaveValue("0");
  await expect(page.locator(".omar-reaction.at-tag").first()).toBeVisible();
  await expect(page.locator(".off-tag").first()).toBeVisible();

  await timeline.getByRole("button", { name: "Next tag" }).click();
  await expect(scrubber).toHaveValue("1");
  await timeline.getByRole("button", { name: "Previous tag" }).click();
  await expect(scrubber).toHaveValue("0");

  await timeline.getByRole("button", { name: "Hide" }).click();
  await expect(timeline).toBeHidden();
});

test("the assistant's terminal opens from the wait, not from a menu", async ({
  page,
}) => {
  // The assistant is not on the diagram, so the gesture that opens an agent's
  // terminal cannot reach it. It is offered beside the wait it belongs to: the
  // moment there is something to look at is the moment it is drafting.
  await fake.close();
  fake = (await startFakeServe({ stepMs: 3000, port: FAKE_SERVE_PORT })) as FakeServe;
  await useFakeServe(page);

  // Nothing is drafting yet, so there is nothing to inspect.
  await expect(page.getByRole("button", { name: "Inspect on terminal" })).toBeHidden();

  await page.getByLabel("Describe a workflow").fill("Review the release plan");
  await page.keyboard.press("Enter");

  const inspect = page.getByRole("button", { name: "Inspect on terminal" });
  await expect(inspect).toBeVisible();
  // Beside the wait rather than under anything: one click, nothing to open.
  const waitBox = (await page.locator(".waiting").boundingBox())!;
  const inspectBox = (await inspect.boundingBox())!;
  expect(inspectBox.x).toBeGreaterThanOrEqual(waitBox.x + waitBox.width - 1);
  await inspect.click();

  const terminal = page.getByRole("dialog");
  await expect(terminal).toBeVisible();
  await expect(terminal).toContainText(/\d+×\d+ · attached/);
  // Named as itself, not as an agent.
  await expect(terminal.locator("h2")).toHaveText("assistant");
  // And said plainly: the chat drives this same pane.
  await expect(terminal).toContainText("same session the chat talks to");

  await page.getByRole("button", { name: "Close terminal" }).click();
  await expect(terminal).toBeHidden();
});

test("dragging the canvas does not select the diagram", async ({ page }) => {
  // The drag that pans is the same gesture the browser reads as selecting
  // text, and Safari highlighted the whole diagram while panning.
  //
  // Chromium does not reproduce that, so asserting on a selection here would
  // pass with or without the fix. The property that prevents it is what gets
  // pinned instead, on both the canvas and the wrapper the drag can reach.
  await useFakeServe(page);
  await page.getByLabel("Describe a workflow").fill("Review the release plan");
  await page.getByLabel("Draft workflow").click();
  await page.getByLabel("Describe a workflow").fill("The planner");
  await page.getByLabel("Draft workflow").click();
  await expect(page.locator(".omar-team")).toHaveCount(1);

  for (const selector of [".diagram-canvas", ".diagram-wrap"]) {
    const userSelect = await page
      .locator(selector)
      .evaluate((el) => getComputedStyle(el).userSelect);
    expect(userSelect).toBe("none");
  }

  // And panning still works, which is the gesture this must not break.
  const scene = page.locator(".diagram-canvas > g").first();
  const before = await scene.getAttribute("transform");
  const canvas = (await page.locator(".diagram-canvas").boundingBox())!;
  await page.mouse.move(canvas.x + 40, canvas.y + canvas.height / 2);
  await page.mouse.down();
  await page.mouse.move(canvas.x + 220, canvas.y + canvas.height / 2, { steps: 12 });
  await page.mouse.up();
  expect(await scene.getAttribute("transform")).not.toBe(before);
});

test("the terminal fits the panel and reflows to it", async ({ page }) => {
  // Scaling a fixed-size terminal to the panel meant getting a placement right
  // as well as a size, and the placement is what kept spilling off the edge. A
  // terminal that reflows has nothing to place: it is the panel.
  await fake.close();
  fake = (await startFakeServe({
    // Slow enough that the assistant is still drafting, which is when its
    // terminal is offered.
    stepMs: 3000,
    port: FAKE_SERVE_PORT,
    terminal: { cols: 200, rows: 50 },
  })) as FakeServe;
  await useFakeServe(page);
  await page.getByLabel("Describe a workflow").fill("Review the release plan");
  await page.keyboard.press("Enter");
  await page.getByRole("button", { name: "Inspect on terminal" }).click();
  await expect(page.getByRole("dialog")).toBeVisible();

  const inside = async () => {
    const frame = (await page.locator(".terminal-frame").boundingBox())!;
    const screen = (await page.locator(".xterm-screen").boundingBox())!;
    return (
      screen.x >= frame.x - 1 &&
      screen.y >= frame.y - 1 &&
      screen.x + screen.width <= frame.x + frame.width + 1 &&
      screen.y + screen.height <= frame.y + frame.height + 1
    );
  };
  await expect.poll(inside, { timeout: 4000 }).toBe(true);

  // The announced 200x50 is not what it renders: it takes the shape the panel
  // can hold, and the session follows.
  await expect(page.getByRole("dialog")).not.toContainText("200×50");

  // And it keeps fitting when the panel changes under it.
  await page.setViewportSize({ width: 900, height: 640 });
  await expect.poll(inside, { timeout: 4000 }).toBe(true);
});

test("containers do not overlap each other", async ({ page }) => {
  // A 35-instance program captured from the runtime. Containers are the one
  // thing on the canvas that must never intersect: each is a box drawn around
  // an instance, so two that overlap draw one instance's ports and reactions
  // inside another instance's frame.
  await fake.close();
  fake = (await startFakeServe({
    stepMs: 30,
    port: FAKE_SERVE_PORT,
    snapshot: "diagram-grand-test.v1.json",
  })) as FakeServe;
  await useFakeServe(page);
  await page.getByLabel("Describe a workflow").fill("smoke test");
  await page.getByLabel("Draft workflow").click();
  await page.getByLabel("Describe a workflow").fill("go");
  await page.getByLabel("Draft workflow").click();

  const boxes = page.locator(".omar-team-body");
  await expect(boxes).toHaveCount(35);

  // Read the layout's own coordinates rather than screen rects: the canvas is
  // scaled to fit, and rounding a scaled rect can invent or hide a 1px kiss.
  // Siblings only: a team can instantiate a team, and a container drawn inside
  // its parent overlaps it by design.
  const rects = await boxes.evaluateAll((nodes) =>
    nodes.map((node) => ({
      parent: node.parentElement?.getAttribute("data-parent") ?? "",
      x: Number(node.getAttribute("x")),
      y: Number(node.getAttribute("y")),
      w: Number(node.getAttribute("width")),
      h: Number(node.getAttribute("height")),
    })),
  );

  const overlaps: string[] = [];
  for (let a = 0; a < rects.length; a += 1) {
    for (let b = a + 1; b < rects.length; b += 1) {
      const p = rects[a];
      const q = rects[b];
      if (p.parent !== q.parent) continue;
      // Touching edges are fine; sharing area is not.
      const wide = Math.min(p.x + p.w, q.x + q.w) - Math.max(p.x, q.x);
      const tall = Math.min(p.y + p.h, q.y + q.h) - Math.max(p.y, q.y);
      if (wide > 1 && tall > 1) {
        overlaps.push(`${a}×${b} (${Math.round(wide)}×${Math.round(tall)}px)`);
      }
    }
  }
  expect(overlaps, `overlapping containers: ${overlaps.join(", ")}`).toEqual([]);
});

test("a timer is drawn as a clock, not as a port", async ({ page }) => {
  // Captured from a real run: `timer t(0, 10ns)` firing on its own, with no
  // input to the program at all.
  await fake.close();
  fake = (await startFakeServe({
    stepMs: 30,
    port: FAKE_SERVE_PORT,
    snapshot: "diagram-timer.v1.json",
  })) as FakeServe;
  await useFakeServe(page);
  await page.getByLabel("Describe a workflow").fill("smoke test");
  await page.getByLabel("Draft workflow").click();
  await page.getByLabel("Describe a workflow").fill("go");
  await page.getByLabel("Draft workflow").click();

  // A clock face with a hand, named locally and showing its schedule.
  const timer = page.locator(".omar-timer-group");
  await expect(timer).toHaveCount(1);
  await expect(timer.locator(".omar-timer-face")).toBeVisible();
  await expect(timer.locator(".omar-timer-label")).toHaveText("t");
  // Its schedule in the units it was written in, not the nanoseconds the wire
  // carries.
  await expect(timer.locator(".omar-timer-meta")).toHaveText("0, every 10ns");

  // And not as a port. A timer drawn as one would be an inlet nothing feeds.
  await expect(page.locator(".omar-port-group")).toHaveCount(1);
  await expect(page.locator(".omar-port-label")).toHaveText("note");

  // The trigger edge starts at the clock, so the reaction is visibly driven by
  // it rather than floating unconnected.
  const edges = await page.locator(".omar-edge").count();
  expect(edges).toBeGreaterThan(0);

  // The snapshot's tag is the tag it last fired at, so the face reads as
  // firing rather than idling.
  await expect(timer).toHaveClass(/firing/);
});

test("a team inside a team is drawn as a box inside a box", async ({ page }) => {
  // Captured from a real run of a Pipeline that instantiates two Stages.
  await fake.close();
  fake = (await startFakeServe({
    stepMs: 30,
    port: FAKE_SERVE_PORT,
    snapshot: "diagram-nested.v1.json",
  })) as FakeServe;
  await useFakeServe(page);
  await page.getByLabel("Describe a workflow").fill("smoke test");
  await page.getByLabel("Draft workflow").click();
  await page.getByLabel("Describe a workflow").fill("go");
  await page.getByLabel("Draft workflow").click();

  // Wait for the layout before reading it: `allTextContents` does not.
  await expect(page.locator(".omar-team-body")).toHaveCount(3);
  const names = await page.locator(".omar-team-name").allTextContents();
  expect(names.sort()).toEqual(["draft", "refine", "run"]);

  // Nesting is the whole point: the two stages must sit *inside* the pipeline,
  // not beside it. Drawn as siblings the picture would say the program has
  // three peers, which is the shape hierarchy exists to stop it claiming.
  const boxes = await page
    .locator(".omar-team-body")
    .evaluateAll((nodes) =>
      nodes.map((node) => ({
        x: Number(node.getAttribute("x")),
        y: Number(node.getAttribute("y")),
        w: Number(node.getAttribute("width")),
        h: Number(node.getAttribute("height")),
      })),
    );
  expect(boxes).toHaveLength(3);
  const outer = boxes.reduce((a, b) => (a.w * a.h > b.w * b.h ? a : b));
  const inner = boxes.filter((box) => box !== outer);
  for (const box of inner) {
    expect(
      box.x >= outer.x &&
        box.y >= outer.y &&
        box.x + box.w <= outer.x + outer.w &&
        box.y + box.h <= outer.y + outer.h,
      `stage ${JSON.stringify(box)} is not inside pipeline ${JSON.stringify(outer)}`,
    ).toBe(true);
  }
  // And the two stages are siblings, so they must not overlap each other.
  const [a, b] = inner;
  const wide = Math.min(a.x + a.w, b.x + b.w) - Math.max(a.x, b.x);
  const tall = Math.min(a.y + a.h, b.y + b.h) - Math.max(a.y, b.y);
  expect(wide > 1 && tall > 1).toBe(false);
});
