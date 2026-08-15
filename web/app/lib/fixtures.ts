import type { DiagramSnapshot } from "./protocol";


/**
 * Captured from a live `omar serve` run of `reviewProgram` below, then overlaid
 * with mid-run state. Reaction names are `reaction.N` because the language does
 * not name prompts -- the diagram leans on the agent and contract instead.
 */
export const reviewWorkflow: DiagramSnapshot = {
  protocol_version: 1,
  team: "ReviewFlow",
  sequence: 14,
  status: "running",
  // Nanoseconds, so the demo shows what a real run shows: two seconds in, a
  // seventh of a second behind the schedule its delays promised.
  current_tag: { timestamp: 2_000_000_000, microstep: 0 },
  lag: 140_000_000,
  timers: [],
  instances: [{ id: "instance::flow", name: "flow", team: "ReviewFlow", parent: "" }],
  agents: [
    {
      id: "agent::flow.planner",
      name: "flow.planner",
      backend: "Codex",
      instance: "flow",
    },
    {
      id: "agent::flow.reviewer",
      name: "flow.reviewer",
      backend: "Codex",
      instance: "flow",
    },
  ],
  ports: [
    {
      id: "port::flow.critique",
      name: "flow.critique",
      kind: "action",
      type: "string",
      delay: null,
      value: null,
      last_tag: null,
      instance: "flow",
    },
    {
      id: "port::flow.plan",
      name: "flow.plan",
      kind: "action",
      type: "string",
      delay: null,
      value: "Draft plan",
      last_tag: { timestamp: 1, microstep: 0 },
      instance: "flow",
    },
    {
      id: "port::flow.request",
      name: "flow.request",
      kind: "input",
      type: "string",
      delay: null,
      value: "Review the release plan",
      last_tag: { timestamp: 0, microstep: 0 },
      instance: "flow",
    },
    {
      id: "port::flow.result",
      name: "flow.result",
      kind: "output",
      type: "string",
      delay: null,
      value: null,
      last_tag: null,
      instance: "flow",
    },
  ],
  reactions: [
    {
      id: "reaction::flow.reaction.0",
      name: "reaction.0",
      agent: "agent::flow.planner",
      order: 0,
      triggers: ["port::flow.request"],
      effects: ["port::flow.plan"],
      contract: "plan",
      status: "completed",
      invocation_id: "inv-001",
      instance: "flow",
      within: null,
    },
    {
      id: "reaction::flow.reaction.1",
      name: "reaction.1",
      agent: "agent::flow.reviewer",
      order: 1,
      triggers: ["port::flow.plan"],
      effects: ["port::flow.critique"],
      contract: "critique",
      status: "running",
      invocation_id: "inv-002",
      instance: "flow",
      // A deadline the program set for itself, drawn as a stopwatch.
      within: 300_000_000_000,
    },
    {
      id: "reaction::flow.reaction.2",
      name: "reaction.2",
      agent: "agent::flow.planner",
      order: 2,
      triggers: ["port::flow.critique"],
      effects: ["port::flow.result"],
      contract: "result",
      status: "idle",
      invocation_id: null,
      instance: "flow",
      within: null,
    },
  ],
  edges: [
    {
      id: "trigger::flow.request::flow.reaction.0",
      kind: "trigger",
      source: "port::flow.request",
      target: "reaction::flow.reaction.0",
      delay: 0,
    },
    {
      id: "effect::flow.reaction.0::flow.plan",
      kind: "effect",
      source: "reaction::flow.reaction.0",
      target: "port::flow.plan",
      delay: 0,
    },
    {
      id: "trigger::flow.plan::flow.reaction.1",
      kind: "trigger",
      source: "port::flow.plan",
      target: "reaction::flow.reaction.1",
      delay: 0,
    },
    {
      id: "effect::flow.reaction.1::flow.critique",
      kind: "effect",
      source: "reaction::flow.reaction.1",
      target: "port::flow.critique",
      delay: 0,
    },
    {
      id: "trigger::flow.critique::flow.reaction.2",
      kind: "trigger",
      source: "port::flow.critique",
      target: "reaction::flow.reaction.2",
      delay: 0,
    },
    {
      id: "effect::flow.reaction.2::flow.result",
      kind: "effect",
      source: "reaction::flow.reaction.2",
      target: "port::flow.result",
      delay: 0,
    },
  ],
};

/** Valid OMAR source. Verified against `omarc`; this is what gets admitted. */
export const reviewProgram = `team ReviewFlow[planner  : Codex,
               reviewer : Codex]
{
    input request : string
    output result : string
    action plan : string
    action critique : string

    prompt planner(request) -> plan
    "
        Draft a concrete plan for this request:
        $(request)
        Set \`plan\` to the plan.
    "

    prompt reviewer(plan) -> critique
    "
        Find risks and omissions in this plan:
        $(plan)
        Set \`critique\` to your findings.
    "

    prompt planner(critique) -> result
    "
        Integrate this critique into a final answer:
        $(critique)
        Set \`result\` to the final answer.
    "
}

main ReviewFlow {
    flow = ReviewFlow()
}`;
