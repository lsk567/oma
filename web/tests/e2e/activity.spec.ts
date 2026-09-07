import { expect, test } from "@playwright/test";
import { readFile } from "node:fs/promises";
import { FAKE_SERVE_PORT, FAKE_SERVE_URL } from "../../playwright.config";
import { startFakeServe } from "../fake-serve.mjs";
import type { ActivitySnapshot, InvocationActivity, SavedRoleSettings } from "../../app/lib/protocol-generated";

// Explicit simulation: these app-server lifecycle cases do not invoke a model.
// Rust Unix-socket tests check the projection; conformance tests check real Omar.
test("activity fixtures: concurrent agents, long tool, settings, diff, reconnect, reload, completion", async ({ page, request }) => {
  const fake = await startFakeServe({ port: FAKE_SERVE_PORT });
  try {
    const program = await readFile(new URL("../fixtures/review-flow.omar", import.meta.url), "utf8");
    const response = await request.post(`${FAKE_SERVE_URL}/v1/runs`, { data: { program, inputs: {"flow.request":"fixture"} } });
    const record = await response.json();
    const diagram = JSON.parse(await readFile(new URL("../fixtures/diagram-snapshot.v1.json", import.meta.url), "utf8"));
    diagram.status = "running";
    const start = Date.now() - 240000;
    const invocation = (id: string, agent: string, reaction: string): InvocationActivity => ({
      invocation_id:id, agent_name:agent, reaction_id:reaction, backend:"codex", started_at:start, finished_at:null,
      execution:"running", connection:"connected", last_activity_at:start + 1000,
      requested:{model:"fixture-model", effort:"medium"}, confirmed:{model:null, effort:null}, reported_thread_settings:{model:"fixture-model",effort:"medium"},
      settings_application:"Fixture settings accepted by turn/start", active_tools:[], artifacts:[],
      events:[{id:`${id}:start`,at:start,time_source:"runtime",kind:"invocation_started",summary:"Fixture invocation started",tool_id:null,exit_code:null}],
    });
    const first = invocation("first", "flow.planner", "flow.reaction.0");
    first.active_tools = [{id:"long",summary:"Fixture validation command",started_at:start + 1000,observed_at:start+1000}];
    first.artifacts = [{id:"file",path:"API_CONTRACT.md",change:"created",observed_at:start+1000,diff:"+ fixture API contract\n",diff_truncated:false,verification:"Not verified"}];
    const second = invocation("second", "flow.reviewer", "flow.reaction.1");
    let activity: ActivitySnapshot = {run_id:record.run_id, sequence:1,server_time:Date.now(),invocations:[first,second]};
    let disconnected = false;
    let roles: SavedRoleSettings[] = [];
    await page.route(`${FAKE_SERVE_URL}/v1/runs/*/activity`, async (route) => {
      if (disconnected) return route.abort();
      await route.fulfill({json:{...activity,server_time:Date.now()}});
    });
    await page.route(`${FAKE_SERVE_URL}/v1/role-settings`, async (route) => {
      if (route.request().method() === "POST") { roles = [route.request().postDataJSON()]; await route.fulfill({json:{saved:true,applies_to:"next_invocation"}}); }
      else await route.fulfill({json:{roles,capabilities_available:true,codex_models:[{model:"fixture-model",name:"Fixture model",efforts:["medium","high"],default_effort:"medium"}]}});
    });
    // Keep fake runtime in flight; timeline events are not the fixture's source.
    await page.route(`${FAKE_SERVE_URL}/v1/events`, (route) => route.fulfill({ contentType:"text/event-stream", body:": fixture connection\n\n" }));
    await page.route(`${FAKE_SERVE_URL}/v1/diagram`, (route) => route.fulfill({json:diagram}));
    await page.addInitScript(({key, value}) => sessionStorage.setItem(key, JSON.stringify(value)), {
      key:`omar.activity.run:${FAKE_SERVE_URL}`, value:{run_id:record.run_id,snapshot:diagram,sequence:0},
    });
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.goto("/");
    await expect(page).toHaveTitle("OMAR Mission Control");
    await expect(page.getByLabel("Agent activity summary")).toContainText("2 active agents");
    await expect(page.getByLabel("Agent activity summary")).toContainText("1 with no recent");
    await page.getByLabel("Inspect agent", {exact:true}).selectOption("flow.planner");
    const panel = page.getByLabel("flow.planner activity", {exact:true});
    await expect(panel).toContainText("Running a tool");
    await expect(panel).toContainText("Fixture validation command");
    await expect(panel).not.toContainText("No reported activity for");
    await expect(panel).toContainText("Not reported");
    await panel.getByText("created · API_CONTRACT.md", {exact:true}).click();
    await expect(panel.getByLabel("API_CONTRACT.md diff")).toContainText("+ fixture API contract");
    await expect(panel).toContainText("Not verified");
    await panel.getByRole("button", {name:"Close",exact:true}).click();
    await page.getByText("Role model and effort settings · next invocation",{exact:true}).click();
    await page.getByLabel("flow.planner requested model").selectOption("fixture-model");
    await page.getByLabel("flow.planner requested effort").selectOption("high");
    await expect(page.getByRole("status")).toContainText("Saved for next invocation");
    expect(roles[0].selection.effort).toBe("high");
    await page.getByLabel("Inspect agent",{exact:true}).selectOption("flow.reviewer");
    const reviewer = page.getByLabel("flow.reviewer activity",{exact:true});
    await expect(reviewer).toContainText("No reported activity for");
    await reviewer.getByLabel("Inactivity notice threshold").selectOption("600");
    await expect(reviewer).not.toContainText("No reported activity for");
    await reviewer.getByLabel("Inactivity notice threshold").selectOption("120");
    disconnected = true;
    await expect(reviewer).toContainText("Activity connection lost");
    await expect(reviewer).not.toContainText("No reported activity for");
    disconnected = false;
    activity = {...activity, sequence:2};
    await expect(reviewer).not.toContainText("Activity connection lost");
    await expect(reviewer).toContainText("No reported activity for");
    await page.reload();
    await expect(page.getByLabel("Agent activity summary")).toContainText("2 active agents");
    await page.getByLabel("Inspect agent",{exact:true}).selectOption("flow.planner");
    await expect(panel).toContainText("Running a tool");
    await expect(panel).toContainText("medium"); // Requested at start is unchanged.
    activity = {...activity, sequence:3, invocations:[{...first, execution:"completed",finished_at:start+200000,active_tools:[]},second]};
    await expect(panel).toContainText("Completed");
    await expect(panel).toContainText("Elapsed 3m 20s");
    await expect(page.getByLabel("Agent activity summary")).toContainText("1 active agents");
    expect(errors).toEqual([]);
    await page.screenshot({path:"/tmp/omar-progress-desktop.png"});
    await page.setViewportSize({width:390,height:844});
    await expect(panel).toBeVisible();
    expect((await panel.boundingBox())!.width).toBeLessThanOrEqual(390);
    await page.screenshot({path:"/tmp/omar-progress-mobile.png"});
  } finally { await fake.close(); }
});
