import { expect, test, type Page } from "@playwright/test";
import { FAKE_SERVE_PORT, FAKE_SERVE_URL } from "../../playwright.config";
import { startFakeServe } from "../fake-serve.mjs";
import type { ApprovalSnapshot, PendingApproval } from "../../app/lib/protocol";

let fake: Awaited<ReturnType<typeof startFakeServe>>;
test.beforeEach(async () => { fake = await startFakeServe({ port: FAKE_SERVE_PORT, autoAdvance: false }); });
test.afterEach(async () => { await fake.close(); });

function request(overrides: Partial<PendingApproval> = {}): PendingApproval {
  return { request_id: "approval-1", agent_id: "assistant", agent_name: "Executive assistant", run_id: null, invocation_id: null, requested_at: Date.now() - 134000, summary: "Publish the workflow result", tool_name: "omar_set_port", scope: "This workflow's contract_milestone output", command: null, cwd: null, resolution_mode: "terminal", ...overrides };
}
function feed(requests: PendingApproval[]): ApprovalSnapshot {
  return { sequence: 1, requests, monitors: requests.map((r) => ({ agent_id: r.agent_id, run_id: r.run_id, state: "connected" })), recent: [] };
}
async function open(page: Page) {
  await page.goto("/");
  await expect(page.locator(".daemon")).toHaveClass(/live/);
}
async function deploy(page: Page) {
  await page.getByLabel("Describe a workflow").fill("Review the release plan");
  await page.getByLabel("Draft workflow").click();
  await expect(page.locator(".messages")).toContainText("Which agent should own");
  await page.getByLabel("Describe a workflow").fill("The planner");
  await page.getByLabel("Draft workflow").click();
  await page.getByRole("button", { name: "Deploy", exact: true }).click();
  await page.getByRole("button", { name: "Confirm deploy" }).click();
  await expect(page.locator(".connection")).toContainText("observing");
  return (await (await page.request.get(`${FAKE_SERVE_URL}/v1/runs`)).json()).runs[0].run_id as string;
}

test("EA approvals work before a topology and survive terminal review and reload", async ({ page }, info) => {
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  const pending = request();
  fake.setApprovals(feed([pending]));
  await open(page);
  await expect(page.locator(".omar-reaction")).toHaveCount(0);
  await expect(page.locator(".panel-heading")).toContainText("Waiting for approval");
  await page.getByRole("button", { name: "1 approval needed" }).click();
  const panel = page.getByRole("dialog", { name: "Approval required" });
  await expect(panel).toContainText("contract_milestone");
  await expect(panel).toContainText("2m");
  const bounds = await panel.boundingBox();
  expect(bounds!.x).toBeGreaterThan(100);
  await expect(panel.getByRole("button", { name: /^Allow/ })).toHaveCount(0);
  await page.screenshot({ path: info.outputPath("approval-desktop.png"), fullPage: true });
  await panel.getByRole("button", { name: "Open agent terminal" }).click();
  await expect(page.getByRole("dialog", { name: "Terminal for the assistant" })).toBeVisible();
  await page.getByRole("button", { name: "Close terminal" }).click();
  await expect(page.getByRole("button", { name: "1 approval needed" })).toBeVisible();
  await page.reload();
  await expect(page.locator(".panel-heading")).toContainText("Waiting for approval");
  expect(errors).toEqual([]);
});

test("only the affected invocation turns orange and keyboard review opens its terminal", async ({ page }, info) => {
  await open(page);
  const run = await deploy(page);
  await expect(page.locator(".omar-reaction.running")).toHaveCount(2);
  const pending = request({ agent_id: "agent::flow.planner", agent_name: "flow.planner", run_id: run, invocation_id: "inv-0", summary: "Run the backend tests", tool_name: "Command execution", command: "python3 -m unittest discover -s tests -v", cwd: "/workspace/omar", scope: "Execute locally with the permissions shown in the terminal" });
  fake.setApprovals(feed([pending]));
  const node = page.locator(".omar-reaction.approval-pending");
  await expect(node).toHaveCount(1);
  await expect(page.locator(".omar-reaction.running")).toHaveCount(1);
  expect(await node.evaluate((el) => getComputedStyle(el).animationName)).toBe("none");
  await expect.poll(() => node.locator(".omar-reaction-body").evaluate((el) => getComputedStyle(el).fill)).toBe("rgb(255, 241, 222)");
  await expect(page.locator(".connection")).toContainText("observing");
  await page.screenshot({ path: info.outputPath("approval-topology.png"), fullPage: true });
  const review = node.getByRole("button", { name: /Review request/ });
  await review.focus();
  await page.keyboard.press("Enter");
  const panel = page.getByRole("dialog", { name: "Approval required" });
  await expect(panel).toContainText("python3 -m unittest discover");
  await panel.getByRole("button", { name: "Open agent terminal" }).click();
  await expect(page.getByRole("dialog", { name: "Terminal for flow.planner" })).toBeVisible();
  await page.getByRole("button", { name: "Close terminal" }).click();
  await expect(node).toHaveCount(1);
});

test("disconnect retains pending requests and backend denial is explicit", async ({ page }) => {
  const pending = request();
  fake.setApprovals(feed([pending]));
  await open(page);
  await page.getByRole("button", { name: "1 approval needed" }).click();
  fake.disconnectApprovals();
  await expect(page.getByRole("dialog")).toContainText("Connection lost");
  await expect(page.getByRole("dialog")).toContainText("Waiting for approval");
  fake.disconnectApprovals(false);
  const resolved = feed([]);
  resolved.recent = [{ request: pending, outcome: "denied", resolved_at: Date.now() }];
  fake.setApprovals(resolved);
  await expect(page.getByRole("dialog", { name: "Request denied" })).toBeVisible({ timeout: 10000 });
  await expect(page.getByRole("dialog")).not.toContainText("Completed");
  await expect(page.locator(".approval-count")).toHaveCount(0);
});

test("mobile review stays readable and multiple pending requests remain reachable", async ({ page }, info) => {
  await page.setViewportSize({ width: 390, height: 844 });
  fake.setApprovals(feed([request(), request({ request_id: "second", agent_id: "agent::reviewer", agent_name: "Reviewer", run_id: "other-run", tool_name: "File changes" })]));
  await open(page);
  await page.getByRole("button", { name: "2 approvals needed" }).click();
  await expect(page.getByRole("dialog")).toBeVisible();
  const box = await page.getByRole("dialog").boundingBox();
  expect(box!.width).toBeLessThanOrEqual(390);
  await page.screenshot({ path: info.outputPath("approval-mobile.png"), fullPage: true });
  await page.getByRole("button", { name: "Review request: Reviewer · File changes" }).click();
  await expect(page.getByRole("dialog")).toContainText("Reviewer");
  await page.keyboard.press("Escape");
  await expect(page.getByRole("dialog")).toHaveCount(0);
});
