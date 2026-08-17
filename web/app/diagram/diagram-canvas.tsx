"use client";

import {
  PointerEvent as ReactPointerEvent,
  WheelEvent as ReactWheelEvent,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { formatDuration, webAgents, type DiagramSnapshot } from "../lib/protocol";

/**
 * Lingua Franca renders reactors as labelled containers whose ports sit on the
 * container boundary and whose reactions are chevrons laid out inside. We
 * reproduce that visual grammar directly: ELK computes the layered layout and
 * this module draws it as SVG. No KIELER/KLighD involved.
 */

type Point = { x: number; y: number };

type ElkPort = {
  id: string;
  x?: number;
  y?: number;
  width?: number;
  height?: number;
};

type ElkSection = {
  startPoint: Point;
  endPoint: Point;
  bendPoints?: Point[];
};

type ElkEdge = {
  id: string;
  sections?: ElkSection[];
};

type ElkNode = {
  id: string;
  x?: number;
  y?: number;
  width?: number;
  height?: number;
  ports?: ElkPort[];
  edges?: ElkEdge[];
  children?: ElkNode[];
  /** Set on the way in; ELK does not return it. */
  layoutOptions?: Record<string, string>;
};

type Box = { x: number; y: number; width: number; height: number };

const REACTION_SIZE = { width: 154, height: 50 };
/** Chevron text starts after the order badge and must clear the right notch. */
const REACTION_TEXT_X = 46;
/**
 * The stopwatch a `within` deadline is drawn as, and the room it needs.
 *
 * A face plus a crown, so it reads as a stopwatch rather than the clock a timer
 * already uses — one counts down against a reaction, the other fires on its
 * own. It sits before the chevron's point with its interval beneath, and the
 * node widens to hold both rather than crowding the name.
 */
const WITHIN_RADIUS = 7;
const WITHIN_ROOM = 40;
const REACTION_MAX_WIDTH = 340;
const ACTION_SIZE = { width: 26, height: 26 };
/** A timer's clock face. Larger than an action: it carries two hands. */
const CLOCK_SIZE = { width: 34, height: 34 };
const PORT_SIZE = { width: 12, height: 12 };
const TEAM_PADDING = { top: 30, right: 78, bottom: 30, left: 78 };
const CANVAS_MARGIN = 28;
/** Feedback edges leave and re-enter horizontally before dropping to a lane. */
/**
 * How far above its box a container's name sits.
 *
 * The name used to live inside, which meant keeping ports away from the top of
 * the border. Every way of doing that desynchronised the layout: moving ports
 * afterwards left edges on a staircase, and `spacing.portsSurrounding` moves
 * the ports without moving the children ELK routes to, which orphans the
 * chevrons. Outside the box there is nothing to avoid.
 */
const TITLE_BAND = 30;
/** Least vertical gap between two ports on the same side. */
const PORT_GAP = 16;

/** How long the view takes to settle when it is fitted. */
const GLIDE_MS = 320;

const WRAP_STUB = 16;
const WRAP_LANE_GAP = 22;
const WRAP_LANE_STEP = 13;
/** Low enough that a wide program still fits a narrow panel. */
const MIN_SCALE = 0.04;
const MAX_SCALE = 1.6;
/** A tight layout should not be blown up to fill a large panel. */
const FIT_SCALE_CEILING = 1.15;

type Viewport = { scale: number; x: number; y: number };

type PortView = {
  id: string;
  name: string;
  kind: "input" | "output";
  center: Point;
};

type ActionView = {
  id: string;
  name: string;
  hasValue: boolean;
  center: Point;
};

type TimerView = {
  id: string;
  name: string;
  offset: number;
  period: number;
  /** Where the hand sits, 0..1 through the current period. */
  phase: number;
  /** True on the tag it fired at, which is when the face pulses. */
  firing: boolean;
  center: Point;
};

type ReactionView = {
  id: string;
  order: number;
  name: string;
  meta: string;
  status: string;
  box: Box;
  /** The agent that runs it, which is whose terminal opens on double click. */
  agent: string;
  /** Nanoseconds from `within`, drawn as a stopwatch. Null when unbounded. */
  within: number | null;
  /** Whether a web client answers this reaction rather than a spawned agent. */
  web: boolean;
};

type EdgeView = {
  id: string;
  kind: string;
  delayed: boolean;
  path: string;
};

type ContainerView = {
  id: string;
  /** The instance name, which is what the program calls this box. */
  name: string;
  /** The team it was instantiated from, shown beneath the name. */
  team: string;
  /** The container this one is drawn inside, or null at the top level. */
  parent: string | null;
  box: Box;
};

type Layout = {
  containers: ContainerView[];
  status: string;
  ports: PortView[];
  actions: ActionView[];
  timers: TimerView[];
  reactions: ReactionView[];
  edges: EdgeView[];
  width: number;
  height: number;
};

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

const CHEVRON_NOTCH = 11;

/** Shapes are drawn at the origin and placed by a group transform, so that
 * layout changes animate rather than jumping. */
/** Pointer travel that turns a click into a pan. */
const DRAG_SLOP = 4;

const ORIGIN: Point = { x: 0, y: 0 };

/** Six-point Lingua Franca reaction chevron. */
function chevronPoints(width: number, height: number): string {
  const mid = height / 2;
  return [
    `0,0`,
    `${width - CHEVRON_NOTCH},0`,
    `${width},${mid}`,
    `${width - CHEVRON_NOTCH},${height}`,
    `0,${height}`,
    `${CHEVRON_NOTCH},${mid}`,
  ].join(" ");
}

/** Right-pointing port triangle, centred on the container boundary. */
function portTriangle({ x, y }: Point): string {
  return `${x - 5},${y - 6.5} ${x + 6.5},${y} ${x - 5},${y + 6.5}`;
}

function diamondPoints({ x, y }: Point, radius: number): string {
  return `${x},${y - radius} ${x + radius},${y} ${x},${y + radius} ${x - radius},${y}`;
}

function distance(a: Point, b: Point): number {
  return Math.hypot(b.x - a.x, b.y - a.y);
}

/**
 * Nudge a polyline's endpoints along their own direction. Positive values pull
 * back (so arrowheads clear port glyphs); negative values push forward (so
 * arrowheads reach a chevron's recessed leading vertex instead of its bounds).
 */
/** Below this a segment counts as already on an axis. */
const AXIS_EPSILON = 0.5;

/**
 * Force every segment onto an axis.
 *
 * Ports are snapped onto their container's border and spread apart to clear its
 * title, which moves them off the point ELK routed to and leaves the last hop
 * on a slant. Rather than chase each cause, paths are squared off before they
 * are drawn: a diagram of boxes has no diagonals in it.
 */
function squareOff(points: Point[]): Point[] {
  if (points.length < 2) return points;
  const squared: Point[] = [{ ...points[0] }];
  for (let index = 1; index < points.length; index += 1) {
    const from = squared[squared.length - 1];
    const to = points[index];
    const askew =
      Math.abs(to.x - from.x) > AXIS_EPSILON &&
      Math.abs(to.y - from.y) > AXIS_EPSILON;
    if (askew) {
      if (points.length === 2) {
        // Port to port with nothing in between: leave one border horizontally,
        // cross, and arrive at the other horizontally.
        const middle = (from.x + to.x) / 2;
        squared.push({ x: middle, y: from.y }, { x: middle, y: to.y });
      } else if (index === points.length - 1) {
        // The last hop meets a port on a vertical border, so it has to arrive
        // horizontally.
        squared.push({ x: from.x, y: to.y });
      } else {
        squared.push({ x: to.x, y: from.y });
      }
    }
    squared.push({ ...to });
  }
  return squared;
}

function adjustEnds(points: Point[], start: number, end: number): Point[] {
  const result = points.map((point) => ({ ...point }));
  if (result.length < 2) return result;
  if (start !== 0) {
    const [first, second] = result;
    const length = distance(first, second);
    if (length > Math.abs(start)) {
      first.x += ((second.x - first.x) / length) * start;
      first.y += ((second.y - first.y) / length) * start;
    }
  }
  if (end !== 0) {
    const last = result[result.length - 1];
    const previous = result[result.length - 2];
    const length = distance(last, previous);
    if (length > Math.abs(end)) {
      last.x += ((previous.x - last.x) / length) * end;
      last.y += ((previous.y - last.y) / length) * end;
    }
  }
  return result;
}

/** Orthogonal polyline with softened corners, matching LF connection routing. */
function orthogonalPath(points: Point[], radius = 9): string {
  if (points.length === 0) return "";
  if (points.length < 3) {
    return points
      .map((point, index) => `${index === 0 ? "M" : "L"}${point.x},${point.y}`)
      .join(" ");
  }
  const commands = [`M${points[0].x},${points[0].y}`];
  for (let index = 1; index < points.length - 1; index += 1) {
    const previous = points[index - 1];
    const corner = points[index];
    const next = points[index + 1];
    const inLength = distance(previous, corner);
    const outLength = distance(corner, next);
    const inset = Math.min(radius, inLength / 2, outLength / 2);
    if (inset < 1) {
      commands.push(`L${corner.x},${corner.y}`);
      continue;
    }
    const entry = {
      x: corner.x + ((previous.x - corner.x) / inLength) * inset,
      y: corner.y + ((previous.y - corner.y) / inLength) * inset,
    };
    const exit = {
      x: corner.x + ((next.x - corner.x) / outLength) * inset,
      y: corner.y + ((next.y - corner.y) / outLength) * inset,
    };
    commands.push(`L${entry.x},${entry.y}`);
    commands.push(`Q${corner.x},${corner.y} ${exit.x},${exit.y}`);
  }
  const last = points[points.length - 1];
  commands.push(`L${last.x},${last.y}`);
  return commands.join(" ");
}

/** Resolve ELK's parent-relative geometry into one absolute coordinate space. */
function flattenLayout(
  node: ElkNode,
  originX: number,
  originY: number,
  nodes: Map<string, Box>,
  ports: Map<string, Box>,
  edges: Map<string, Point[]>,
): void {
  const x = originX + (node.x ?? 0);
  const y = originY + (node.y ?? 0);
  nodes.set(node.id, {
    x,
    y,
    width: node.width ?? 0,
    height: node.height ?? 0,
  });
  for (const port of node.ports ?? []) {
    ports.set(port.id, {
      x: x + (port.x ?? 0),
      y: y + (port.y ?? 0),
      width: port.width ?? 0,
      height: port.height ?? 0,
    });
  }
  for (const edge of node.edges ?? []) {
    const section = edge.sections?.[0];
    if (!section) continue;
    const points = [
      section.startPoint,
      ...(section.bendPoints ?? []),
      section.endPoint,
    ].map((point) => ({ x: x + point.x, y: y + point.y }));
    edges.set(edge.id, points);
  }
  for (const child of node.children ?? []) {
    flattenLayout(child, x, y, nodes, ports, edges);
  }
}

type ReactionLabels = { name: string; meta: string };

/**
 * Labels are needed before layout (to size the chevron) and after it (to draw
 * them), so they are derived once here rather than computed twice.
 */
function reactionLabels(snapshot: DiagramSnapshot): Map<string, ReactionLabels> {
  const agents = new Map(
    snapshot.agents.map((agent) => [
      agent.id,
      { name: agent.name, backend: agent.backend },
    ]),
  );
  return new Map(
    snapshot.reactions.map((reaction) => {
      const owner = agents.get(reaction.agent);
      const agent = owner?.name ?? reaction.agent;
      // Which model is behind an agent changes how you read what it did, and
      // the program picked it deliberately, so it belongs on the chevron.
      const detail = [owner?.backend, reaction.status]
        .filter(Boolean)
        .join(" \u00b7 ");
      // The runtime names reactions `reaction.N` because OMAR prompts are
      // anonymous, which makes a poor headline. Lead with the agent and let the
      // contract say what it writes; honour a real name if one ever arrives.
      const generated = /^reaction\.\d+$/.test(reaction.name);
      return [
        reaction.id,
        {
          name: generated ? agent : reaction.name,
          meta: generated ? detail : `${agent} \u00b7 ${detail}`,
        },
      ];
    }),
  );
}

/** Rough advance width; contracts vary in length and must not overflow. */
function textWidth(text: string, perChar: number): number {
  return text.length * perChar;
}

function reactionWidth(labels: ReactionLabels, within: number | null): number {
  const widest = Math.max(
    textWidth(labels.name, 6.9),
    textWidth(labels.meta, 5.2),
  );
  // A deadline is drawn between the text and the point, so the node has to be
  // wider for it rather than the stopwatch sitting on top of the name.
  const deadline = within === null ? 0 : WITHIN_ROOM;
  return clamp(
    Math.ceil(REACTION_TEXT_X + widest + deadline + CHEVRON_NOTCH + 10),
    REACTION_SIZE.width,
    REACTION_MAX_WIDTH,
  );
}

/**
 * The containers to draw, in declaration order.
 *
 * The runtime states these now. A program from before it says nothing, and is
 * drawn the way it always was: one box named after the program.
 */
function containersOf(snapshot: DiagramSnapshot): ContainerView[] {
  // A snapshot can reach here from more paths than the one that normalises it,
  // so treat a missing list as no instances rather than trusting the type.
  if (!snapshot.instances?.length) {
    return [
      {
        id: `instance::${snapshot.team}`,
        name: snapshot.team,
        team: "",
        parent: null,
        box: { x: 0, y: 0, width: 0, height: 0 },
      },
    ];
  }
  const declared = new Set(snapshot.instances.map((instance) => instance.id));
  const nameOf = new Map(
    snapshot.instances.map((instance) => [instance.id, instance.name]),
  );
  return snapshot.instances.map((instance) => ({
    id: instance.id,
    // Titled by its own name, not its path. The box is already drawn inside
    // its parent, so repeating the parent in the title says nothing — the
    // same reason a port inside a container reads `topic` and not
    // `writer.topic`.
    name: localName(instance.name, nameOf.get(instance.parent) ?? ""),
    team: instance.team,
    // A parent naming a container that is not in the snapshot would leave the
    // child out of the tree entirely, so an unknown one is treated as top
    // level: better a flat drawing than a missing box.
    parent:
      instance.parent && declared.has(instance.parent) ? instance.parent : null,
    box: { x: 0, y: 0, width: 0, height: 0 },
  }));
}

/** Which container a node belongs to, falling back to the lone legacy one. */
function containerId(instance: string, snapshot: DiagramSnapshot): string {
  return instance ? `instance::${instance}` : `instance::${snapshot.team}`;
}

/**
 * A port's name within its container.
 *
 * The container is already labelled `checker`, so drawing `checker.draft`
 * inside it repeats the prefix on every port and pushes the text into the
 * shapes beside it.
 */
function localName(name: string, instance: string): string {
  const prefix = `${instance}.`;
  return instance && name.startsWith(prefix) ? name.slice(prefix.length) : name;
}

type ElkFactory = { new (): { layout(graph: unknown): Promise<unknown> } };

async function runElk(
  ELK: ElkFactory,
  snapshot: DiagramSnapshot,
  containers: ContainerView[],
  aspectRatio: number | null,
): Promise<ElkNode> {
  const labels = reactionLabels(snapshot);
  const wrapping: Record<string, string> =
    aspectRatio === null
      ? {}
      : {
          "elk.aspectRatio": `${aspectRatio}`,
          "elk.layered.wrapping.strategy": "SINGLE_EDGE",
          "elk.layered.wrapping.additionalEdgeSpacing": "26",
        };

  // Port labels are drawn inside the container, so the padding has to leave
  // room for the longest of them or the text runs into the first node.
  const labelRoom = (names: string[]) =>
    names.length === 0
      ? 0
      : Math.ceil(Math.max(...names.map((name) => textWidth(name, 6.4))));

  // ELK expresses an edge's route relative to the deepest node containing both
  // of its ends, whatever node the edge was declared on. Declaring each edge
  // where it belongs keeps that node's origin the one to offset by; declaring
  // them all at the root would draw every inside-a-container edge relative to
  // the canvas instead.
  const home = new Map<string, string>();
  for (const port of snapshot.ports) {
    home.set(port.id, containerId(port.instance, snapshot));
  }
  for (const reaction of snapshot.reactions) {
    home.set(reaction.id, containerId(reaction.instance, snapshot));
  }
  for (const timer of snapshot.timers) {
    home.set(timer.id, containerId(timer.instance, snapshot));
  }
  // Containers nest, so an edge belongs to the deepest container holding both
  // of its ends — the same node ELK expresses its route relative to. With one
  // level "both ends in the same container" said the same thing; with two it
  // would push an edge from a nested port up to the canvas.
  const parentOf = new Map(
    containers.map((container) => [container.id, container.parent]),
  );
  const chainOf = (container: string | undefined): string[] => {
    const chain: string[] = [];
    let current = container;
    while (current) {
      chain.unshift(current);
      current = parentOf.get(current) ?? undefined;
    }
    return chain;
  };
  const commonAncestor = (a?: string, b?: string): string | null => {
    const from = chainOf(a);
    const to = chainOf(b);
    let deepest: string | null = null;
    for (let index = 0; index < Math.min(from.length, to.length); index += 1) {
      if (from[index] !== to[index]) break;
      deepest = from[index];
    }
    return deepest;
  };
  const edgesFor = (container: string | null) =>
    snapshot.edges
      .filter(
        (edge) =>
          commonAncestor(home.get(edge.source), home.get(edge.target)) ===
          container,
      )
      .map((edge) => ({
        id: edge.id,
        sources: [edge.source],
        targets: [edge.target],
      }));

  // A container's children are its own nodes plus the containers it holds.
  // Hoisted rather than inlined so it can recurse.
  function childrenOf(parent: string | null): ElkNode[] {
    return containers
      .filter((container) => container.parent === parent)
      .map(containerNode);
  }

  const elk = new ELK();
  return (await elk.layout({
    id: "root",
    layoutOptions: {
      "elk.algorithm": "layered",
      "elk.direction": "RIGHT",
      "elk.hierarchyHandling": "INCLUDE_CHILDREN",
      "elk.edgeRouting": "ORTHOGONAL",
      "elk.padding": "[top=0,left=0,bottom=0,right=0]",
      // Between containers, so a cross-instance edge has somewhere to run.
      "elk.spacing.nodeNode": "56",
      "elk.layered.spacing.nodeNodeBetweenLayers": "64",
    },
    children: childrenOf(null),
    // Only what crosses a container boundary belongs to the canvas.
    edges: edgesFor(null),
  })) as ElkNode;

  function containerNode(container: ContainerView): ElkNode {
      const mine = (instance: string) =>
        containerId(instance, snapshot) === container.id;
      const ports = snapshot.ports.filter(
        (port) => mine(port.instance) && port.kind !== "action",
      );
      const actions = snapshot.ports.filter(
        (port) => mine(port.instance) && port.kind === "action",
      );
      const reactions = snapshot.reactions.filter((reaction) =>
        mine(reaction.instance),
      );
      const timers = snapshot.timers.filter((timer) => mine(timer.instance));
      const room = (kind: string) =>
        labelRoom(
          ports
            .filter((port) => port.kind === kind)
            .map((port) => localName(port.name, port.instance)),
        );

      return {
        id: container.id,
        layoutOptions: {
          "elk.algorithm": "layered",
          "elk.direction": "RIGHT",
          "elk.portConstraints": "FIXED_SIDE",
          "elk.spacing.portPort": `${PORT_GAP}`,
          "elk.padding": `[top=${TEAM_PADDING.top},left=${
            TEAM_PADDING.left + room("input")
          },bottom=${TEAM_PADDING.bottom},right=${
            TEAM_PADDING.right + room("output")
          }]`,
          // In-layer (vertical) spacing only. Kept generous because an action's
          // name is drawn under its rhombus, outside the node box ELK sees.
          "elk.spacing.nodeNode": "48",
          "elk.layered.spacing.nodeNodeBetweenLayers": "44",
          "elk.spacing.edgeNode": "22",
          "elk.layered.spacing.edgeNodeBetweenLayers": "22",
          "elk.layered.nodePlacement.strategy": "NETWORK_SIMPLEX",
          ...wrapping,
        },
        ports: ports.map((port) => ({
          id: port.id,
          width: PORT_SIZE.width,
          height: PORT_SIZE.height,
          layoutOptions: {
            "elk.port.side": port.kind === "input" ? "WEST" : "EAST",
          },
        })),
        children: [
          ...timers.map((timer) => ({
            id: timer.id,
            width: CLOCK_SIZE.width,
            height: CLOCK_SIZE.height,
          })),
          ...actions.map((port) => ({
            id: port.id,
            width: ACTION_SIZE.width,
            height: ACTION_SIZE.height,
          })),
          ...reactions.map((reaction) => ({
            id: reaction.id,
            width: reactionWidth(
              labels.get(reaction.id) ?? { name: reaction.name, meta: "" },
              reaction.within,
            ),
            height: REACTION_SIZE.height,
          })),
          ...childrenOf(container.id),
        ],
        edges: edgesFor(container.id),
      };
  }
}

function buildLayout(
  snapshot: DiagramSnapshot,
  declared: ContainerView[],
  result: ElkNode,
): Layout {
  const boundaryPorts = snapshot.ports.filter((port) => port.kind !== "action");
  const actionPorts = snapshot.ports.filter((port) => port.kind === "action");

  const nodeBoxes = new Map<string, Box>();
  const portBoxes = new Map<string, Box>();
  const edgePoints = new Map<string, Point[]>();
  flattenLayout(result, 0, 0, nodeBoxes, portBoxes, edgePoints);

  const containers: ContainerView[] = declared.map((container) => ({
    ...container,
    box: nodeBoxes.get(container.id) ?? { x: 0, y: 0, width: 0, height: 0 },
  }));
  const boxOf = new Map(containers.map((container) => [container.id, container.box]));

  // ELK places boundary ports near the border; snap them exactly onto their own
  // container's border so the triangles read as anchored to it, the way LF
  // draws them.
  const portCenters = new Map<string, Point>();
  const ports: PortView[] = boundaryPorts.map((port) => {
    const owner = boxOf.get(containerId(port.instance, snapshot)) ?? {
      x: 0,
      y: 0,
      width: 0,
      height: 0,
    };
    // Only the x is snapped, onto the container's border. Moving the y would
    // take the port off the point ELK routed to and leave the last hop of
    // every edge on a staircase.
    const box = portBoxes.get(port.id);
    const center = {
      x: port.kind === "input" ? owner.x : owner.x + owner.width,
      y: (box?.y ?? owner.y) + (box?.height ?? PORT_SIZE.height) / 2,
    };
    portCenters.set(port.id, center);
    return {
      id: port.id,
      name: localName(port.name, port.instance),
      kind: port.kind === "input" ? "input" : "output",
      center,
    };
  });

  const actions: ActionView[] = actionPorts.map((port) => {
    const box = nodeBoxes.get(port.id);
    return {
      id: port.id,
      name: localName(port.name, port.instance),
      hasValue: port.value !== null,
      center: {
        x: (box?.x ?? 0) + (box?.width ?? ACTION_SIZE.width) / 2,
        y: (box?.y ?? 0) + (box?.height ?? ACTION_SIZE.height) / 2,
      },
    };
  });

  // The hand's position is derived from the run's own clock rather than from
  // wall time: logical time only moves when the runtime says so, and a hand
  // sweeping smoothly while the run is blocked on an agent would be a lie.
  const now = snapshot.current_tag?.timestamp ?? 0;
  const timers: TimerView[] = snapshot.timers.map((timer) => {
    const box = nodeBoxes.get(timer.id);
    const fired = timer.last_tag?.timestamp ?? null;
    const since = fired === null ? Math.max(0, now - timer.offset) : now - fired;
    return {
      id: timer.id,
      name: localName(timer.name, timer.instance),
      offset: timer.offset,
      period: timer.period,
      // A one-shot has no period to be part-way through: it is either waiting
      // or spent, so the hand sits at the top rather than creeping round.
      phase: timer.period > 0 ? (since % timer.period) / timer.period : 0,
      firing: fired !== null && fired === now,
      center: {
        x: (box?.x ?? 0) + (box?.width ?? CLOCK_SIZE.width) / 2,
        y: (box?.y ?? 0) + (box?.height ?? CLOCK_SIZE.height) / 2,
      },
    };
  });

  const labels = reactionLabels(snapshot);
  const web = webAgents(snapshot);
  const reactions: ReactionView[] = snapshot.reactions.map((reaction) => {
    const label = labels.get(reaction.id) ?? { name: reaction.name, meta: "" };
    return {
      id: reaction.id,
      order: reaction.order,
      name: label.name,
      meta: label.meta,
      status: reaction.status,
      box: nodeBoxes.get(reaction.id) ?? { x: 0, y: 0, ...REACTION_SIZE },
      agent: componentName(reaction.agent),
      within: reaction.within,
      web: web.has(reaction.agent),
    };
  });

  // Anything the diagram draws can be an edge endpoint. Ports are points on the
  // container boundary; everything else is a laid-out box.
  const anchorBox = (id: string): Box | null => {
    const port = portCenters.get(id);
    if (port) return { x: port.x, y: port.y, width: 0, height: 0 };
    return nodeBoxes.get(id) ?? null;
  };

  /**
   * Route an edge whose target sits left of its source.
   *
   * Flow reads left to right, so a feedback edge should still leave the right
   * of its source and arrive at the left of its target, wrapping under the row
   * rather than doubling back through it with a leftward arrowhead.
   */
  const wrapRoute = (from: Box, to: Box, lane: number): Point[] => {
    const start = { x: from.x + from.width, y: from.y + from.height / 2 };
    const end = { x: to.x, y: to.y + to.height / 2 };
    const exit = start.x + WRAP_STUB;
    const entry = end.x - WRAP_STUB;
    return [
      start,
      { x: exit, y: start.y },
      { x: exit, y: lane },
      { x: entry, y: lane },
      { x: entry, y: end.y },
      end,
    ];
  };

  // Which container each endpoint belongs to, so a feedback edge can be routed
  // in the space it actually loops through. An edge between two nodes of one
  // instance wraps under that instance; one that crosses instances wraps under
  // everything between them.
  const home = new Map<string, string>();
  for (const port of snapshot.ports) {
    home.set(port.id, containerId(port.instance, snapshot));
  }
  for (const reaction of snapshot.reactions) {
    home.set(reaction.id, containerId(reaction.instance, snapshot));
  }
  const ownerOf = (source: string, target: string): string | null => {
    const from = home.get(source);
    return from && from === home.get(target) ? from : null;
  };

  // Node boxes grouped by owner. Container boxes are deliberately absent: a
  // lane inside a container that had to clear the container itself would be
  // pushed straight out through its own floor.
  const boxesInside = new Map<string, Box[]>();
  for (const [id, owner] of home) {
    const box = nodeBoxes.get(id);
    if (!box) continue;
    const group = boxesInside.get(owner);
    if (group) group.push(box);
    else boxesInside.set(owner, [box]);
  }

  /**
   * A return lane has to clear everything it passes beneath, not just the two
   * ends it joins. Both ends are usually ports, which have no height, so a lane
   * placed from those alone runs straight through whatever box sits between
   * them.
   *
   * `over` is what the lane must clear. For an edge that loops inside one
   * instance that is the instance's own nodes — clearing every box in the
   * diagram that happened to share an x-range would drop the lane below
   * unrelated instances and drag its container down with it.
   */
  const laneBelow = (
    left: number,
    right: number,
    floor: number,
    over: Iterable<Box>,
  ): number => {
    let lowest = floor;
    for (const box of over) {
      const overlaps = box.x < right && box.x + box.width > left;
      if (overlaps) lowest = Math.max(lowest, box.y + box.height);
    }
    return lowest;
  };

  // Lanes are staggered per owner rather than globally: an instance's third
  // feedback edge should clear that instance's other two, not every wrapped
  // edge drawn anywhere before it.
  const wrapped = new Map<string | null, number>();
  const deepestLane = new Map<string, number>();
  const reactionIds = new Set(snapshot.reactions.map((reaction) => reaction.id));
  const edges: EdgeView[] = snapshot.edges.flatMap((edge) => {
    const points = edgePoints.get(edge.id);
    if (!points || points.length < 2) return [];
    let routed = points.map((point) => ({ ...point }));
    const sourcePort = portCenters.get(edge.source);
    const targetPort = portCenters.get(edge.target);
    if (sourcePort) routed[0] = { ...sourcePort };
    if (targetPort) routed[routed.length - 1] = { ...targetPort };

    const from = anchorBox(edge.source);
    const to = anchorBox(edge.target);
    if (from && to && to.x + to.width <= from.x + from.width) {
      // Stagger lanes so several feedback edges do not sit on top of each other.
      const span = {
        left: to.x - WRAP_STUB,
        right: from.x + from.width + WRAP_STUB,
      };
      const owner = ownerOf(edge.source, edge.target);
      const over = owner ? (boxesInside.get(owner) ?? []) : nodeBoxes.values();
      const lane =
        laneBelow(
          span.left,
          span.right,
          Math.max(from.y + from.height, to.y + to.height),
          over,
        ) +
        WRAP_LANE_GAP +
        (wrapped.get(owner) ?? 0) * WRAP_LANE_STEP;
      wrapped.set(owner, (wrapped.get(owner) ?? 0) + 1);
      if (owner) {
        deepestLane.set(owner, Math.max(deepestLane.get(owner) ?? lane, lane));
      }
      routed = wrapRoute(from, to, lane);
    }

    const start = sourcePort ? 8 : 0;
    const end = targetPort ? 9 : reactionIds.has(edge.target) ? -CHEVRON_NOTCH : 0;
    return [
      {
        id: edge.id,
        kind: edge.kind,
        delayed: edge.delay !== null,
        path: orthogonalPath(adjustEnds(squareOff(routed), start, end)),
      },
    ];
  });

  // ELK does not know about the lanes, so grow a container that has one running
  // beneath its own nodes rather than letting the edge escape its box. Only
  // that container: a lane belongs to the instance whose nodes it loops under,
  // and stretching every container that merely started above it was what left
  // 35 instances drawn on top of one another.
  const enclosing = containers.map((container) => {
    const lane = deepestLane.get(container.id);
    if (lane === undefined) return container;
    const overflow =
      lane + WRAP_LANE_GAP - (container.box.y + container.box.height);
    return overflow > 0
      ? {
          ...container,
          box: { ...container.box, height: container.box.height + overflow },
        }
      : container;
  });

  return {
    containers: enclosing,
    status: snapshot.status,
    ports,
    actions,
    timers,
    reactions,
    edges,
    width: Math.max(...enclosing.map((c) => c.box.x + c.box.width), 0),
    height: Math.max(...enclosing.map((c) => c.box.y + c.box.height), 0),
  };
}

/**
 * Identifies the shape of a topology, ignoring anything that changes during a
 * run — statuses, values, tags. Two snapshots of the same team mid-run share a
 * key; a different program does not.
 */
function structureKey(snapshot: DiagramSnapshot): string {
  return [
    snapshot.team,
    // Containers change the shape as much as nodes do, so a re-instantiation
    // must reclaim the fit rather than keep a view fitted to the old boxes.
    (snapshot.instances ?? []).map((instance) => instance.id).join(","),
    snapshot.ports.map((port) => port.id).join(","),
    snapshot.reactions.map((reaction) => reaction.id).join(","),
    snapshot.edges.map((edge) => edge.id).join(","),
  ].join("|");
}

type Size = { width: number; height: number };

function fitScale(layout: Layout, size: Size): number {
  return clamp(
    Math.min(
      size.width / (layout.width + CANVAS_MARGIN * 2),
      size.height / (layout.height + CANVAS_MARGIN * 2),
    ),
    MIN_SCALE,
    FIT_SCALE_CEILING,
  );
}

/**
 * A single left-to-right row reads best, so it is the default. But a long chain
 * squeezed into a narrow panel becomes illegible, and ELK can wrap the chain
 * into stacked rows instead. Lay the team out both ways and keep whichever
 * actually renders larger in the panel we have, biased towards the single row.
 */
const WRAP_CANDIDATES = [3.0, 2.2, 1.6];
const WRAP_GAIN = 1.08;

async function computeLayout(
  snapshot: DiagramSnapshot,
  size: Size,
): Promise<Layout> {
  const { default: ELK } = await import("elkjs/lib/elk.bundled.js");
  const factory = ELK as unknown as ElkFactory;

  const declared = containersOf(snapshot);
  const single = buildLayout(
    snapshot,
    declared,
    await runElk(factory, snapshot, declared, null),
  );
  const nodeCount = snapshot.reactions.length + snapshot.ports.length;
  if (size.width === 0 || size.height === 0 || nodeCount < 4) return single;

  let best = single;
  let bestScale = fitScale(single, size) * WRAP_GAIN;
  for (const aspectRatio of WRAP_CANDIDATES) {
    let candidate: Layout;
    try {
      candidate = buildLayout(
        snapshot,
        declared,
        await runElk(factory, snapshot, declared, aspectRatio),
      );
    } catch {
      // Wrapping is an optimisation, and ELK throws on some graphs — cyclic
      // ones especially. Losing a candidate is fine; losing the diagram is not.
      continue;
    }
    const scale = fitScale(candidate, size);
    if (scale > bestScale) {
      best = candidate;
      bestScale = scale;
    }
  }
  return best;
}

/**
 * The runtime's name for a node, which is what the operator and the EA both
 * call it. Diagram ids carry a kind prefix that means nothing outside the
 * protocol, so `port::n1.out` selects as `n1.out`.
 */
export function componentName(id: string): string {
  const separator = id.indexOf("::");
  return separator === -1 ? id : id.slice(separator + 2);
}

export function DiagramCanvas({
  snapshot,
  selection = [],
  onToggleComponent,
  onOpenTerminal,
  onOpenPanel,
}: {
  snapshot: DiagramSnapshot;
  selection?: string[];
  onToggleComponent?: (component: string) => void;
  /** Given only while agents are actually running and can be attached to. */
  onOpenTerminal?: (agent: string) => void;
  /** A web agent has no pane, so its double-click opens its panel instead. */
  onOpenPanel?: (agent: string) => void;
}) {
  const hostRef = useRef<HTMLDivElement | null>(null);
  const [layout, setLayout] = useState<Layout | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [size, setSize] = useState<Size>({ width: 0, height: 0 });
  const [userView, setUserView] = useState<Viewport | null>(null);
  const dragRef = useRef<{ x: number; y: number; view: Viewport } | null>(null);
  // The glide reads the live view without re-creating itself on every frame.
  const viewRef = useRef<Viewport | null>(null);
  const glideRef = useRef(0);
  // A click that panned the canvas is not a click on what sits under it.
  const draggedRef = useRef(false);
  const selected = useMemo(() => new Set(selection), [selection]);

  // Reads the drag flag when the click happens, never during render.
  function handleNodeClick(component: string) {
    if (draggedRef.current) return;
    onToggleComponent?.(component);
  }

  /** Props that make a node selectable, or just its class when selection is off. */
  function selectable(id: string, base: string) {
    if (!onToggleComponent) return { className: base };
    const component = componentName(id);
    return {
      className: `${base}${selected.has(component) ? " selected" : ""}`,
      onClick: () => handleNodeClick(component),
      // The canvas resets the view on double click; a node has its own meaning.
      onDoubleClick: (event: { stopPropagation: () => void }) =>
        event.stopPropagation(),
    };
  }

  // Layout depends on the panel size, but only coarsely: bucket it so ordinary
  // resizing repaints without re-running ELK on every animation frame.
  const sizeRef = useRef(size);
  // A run republishes the whole snapshot on every event. Refitting each time
  // would yank the view out from under anyone who has zoomed in, so the fit is
  // only reclaimed when the topology itself changes.
  const structureRef = useRef<string | null>(null);
  const sizeBucket =
    size.width && size.height
      ? `${Math.round(size.width / 80)}x${Math.round(size.height / 80)}`
      : "";

  useEffect(() => {
    let cancelled = false;
    computeLayout(snapshot, sizeRef.current)
      .then((next) => {
        if (cancelled) return;
        setLayout(next);
        const structure = structureKey(snapshot);
        if (structureRef.current !== structure) {
          structureRef.current = structure;
          setUserView(null);
        }
        setError(null);
      })
      .catch((cause: unknown) => {
        if (cancelled) return;
        setError(cause instanceof Error ? cause.message : String(cause));
      });
    return () => {
      cancelled = true;
    };
  }, [snapshot, sizeBucket]);

  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;
    const observer = new ResizeObserver((entries) => {
      const rect = entries[0]?.contentRect;
      if (!rect) return;
      const next = { width: rect.width, height: rect.height };
      sizeRef.current = next;
      setSize(next);
    });
    observer.observe(host);
    return () => observer.disconnect();
  }, []);

  const fitted = useMemo<Viewport | null>(() => {
    if (!layout || size.width === 0 || size.height === 0) return null;
    const scale = fitScale(layout, size);
    return {
      scale,
      x: (size.width - layout.width * scale) / 2,
      y: (size.height - layout.height * scale) / 2,
    };
  }, [layout, size]);

  const view = userView ?? fitted;

  /**
   * Ease the viewport to `target` instead of jumping.
   *
   * Fitting is a change of what you are looking at, and a cut gives no sense of
   * where the diagram went. Panning and zooming stay immediate: they are
   * already continuous, and easing them would just feel like lag.
   */
  const glideTo = useCallback(
    (target: Viewport) => {
      const from = viewRef.current;
      if (!from) {
        setUserView(target);
        return;
      }
      cancelAnimationFrame(glideRef.current);
      const started = performance.now();
      const step = (now: number) => {
        const progress = Math.min((now - started) / GLIDE_MS, 1);
        // Ease out: quick to start, settling at the end.
        const eased = 1 - (1 - progress) ** 3;
        setUserView({
          scale: from.scale + (target.scale - from.scale) * eased,
          x: from.x + (target.x - from.x) * eased,
          y: from.y + (target.y - from.y) * eased,
        });
        if (progress < 1) {
          glideRef.current = requestAnimationFrame(step);
        } else {
          // Back to following the fit, so a later layout change refits rather
          // than holding the view this glide happened to end on.
          setUserView(null);
        }
      };
      glideRef.current = requestAnimationFrame(step);
    },
    [],
  );

  // The glide interpolates from wherever the view is now, which it cannot read
  // during render.
  useEffect(() => {
    viewRef.current = view;
  }, [view]);

  useEffect(() => () => cancelAnimationFrame(glideRef.current), []);

  const fit = useCallback(() => {
    if (fitted) glideTo(fitted);
    else setUserView(null);
  }, [fitted, glideTo]);

  const zoomBy = useCallback(
    (factor: number, focus?: Point) => {
      setUserView((current) => {
        const base = current ?? fitted;
        if (!base) return current;
        const scale = clamp(base.scale * factor, MIN_SCALE, MAX_SCALE);
        const anchor = focus ?? { x: size.width / 2, y: size.height / 2 };
        const ratio = scale / base.scale;
        return {
          scale,
          x: anchor.x - (anchor.x - base.x) * ratio,
          y: anchor.y - (anchor.y - base.y) * ratio,
        };
      });
    },
    [fitted, size.height, size.width],
  );

  function handleWheel(event: ReactWheelEvent<SVGSVGElement>) {
    if (!view) return;
    const rect = event.currentTarget.getBoundingClientRect();
    zoomBy(Math.exp(-event.deltaY * 0.0015), {
      x: event.clientX - rect.left,
      y: event.clientY - rect.top,
    });
  }

  function handlePointerDown(event: ReactPointerEvent<SVGSVGElement>) {
    if (!view || event.button !== 0) return;
    // Touching the canvas ends any glide; the operator's hand wins.
    cancelAnimationFrame(glideRef.current);
    dragRef.current = { x: event.clientX, y: event.clientY, view };
    draggedRef.current = false;
    // Capture is claimed on first movement, not here: a captured pointer
    // delivers the click to the canvas instead of the node under it, which
    // would make nodes unselectable.
  }

  function handlePointerMove(event: ReactPointerEvent<SVGSVGElement>) {
    const drag = dragRef.current;
    if (!drag) return;
    if (
      !draggedRef.current &&
      (Math.abs(event.clientX - drag.x) > DRAG_SLOP ||
        Math.abs(event.clientY - drag.y) > DRAG_SLOP)
    ) {
      draggedRef.current = true;
      event.currentTarget.setPointerCapture(event.pointerId);
    }
    if (!draggedRef.current) return;
    setUserView({
      scale: drag.view.scale,
      x: drag.view.x + (event.clientX - drag.x),
      y: drag.view.y + (event.clientY - drag.y),
    });
  }

  function endDrag(event: ReactPointerEvent<SVGSVGElement>) {
    if (!dragRef.current) return;
    dragRef.current = null;
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId);
    }
  }

  return (
    <div className="diagram-wrap" ref={hostRef}>
      <svg
        className="diagram-canvas"
        role="img"
        aria-label={`Live topology diagram for ${snapshot.team}`}
        onWheel={handleWheel}
        onPointerDown={handlePointerDown}
        onPointerMove={handlePointerMove}
        onPointerUp={endDrag}
        onPointerCancel={endDrag}
        onDoubleClick={fit}
      >
        {layout && view ? (
          <g transform={`translate(${view.x},${view.y}) scale(${view.scale})`}>
            {layout.containers.map((container) => (
              <g
                key={container.id}
                className={`omar-team ${layout.status}`}
                // Which box this one sits in. Nesting is structure, so it is
                // readable from the drawing rather than only inferable from
                // the geometry.
                data-parent={container.parent ?? ""}
              >
                <rect
                  className="omar-team-shadow"
                  x={container.box.x + 5}
                  y={container.box.y + 5}
                  width={container.box.width}
                  height={container.box.height}
                  rx={9}
                  ry={9}
                />
                <rect
                  className="omar-team-body"
                  x={container.box.x}
                  y={container.box.y}
                  width={container.box.width}
                  height={container.box.height}
                  rx={9}
                  ry={9}
                />
                <text
                  className="omar-team-name"
                  x={container.box.x + 2}
                  y={container.box.y - TITLE_BAND + 12}
                >
                  {container.name}
                </text>
                <text
                  className="omar-team-kind"
                  x={container.box.x + 2}
                  y={container.box.y - TITLE_BAND + 25}
                >
                  {/* An instance is drawn as itself; the team it came from is
                      the subtitle, so two instances of one team read apart. */}
                  {container.team
                    ? `${container.team} · ${layout.status}`
                    : `team · ${layout.status}`}
                </text>
              </g>
            ))}

            <g className="omar-edges">
              {layout.edges.map((edge) => (
                <path
                  key={edge.id}
                  data-id={edge.id}
                  className={`omar-edge ${edge.kind}${edge.delayed ? " delayed" : ""}`}
                  // No arrowhead: the port triangles already point the way,
                  // and a head on the line repeats it.
                  d={edge.path}
                />
              ))}
            </g>

            <g className="omar-actions">
              {layout.actions.map((action) => (
                <g
                  key={action.id}
                  {...selectable(action.id, "omar-action-group")}
                  transform={`translate(${action.center.x},${action.center.y})`}
                >
                  <polygon
                    className={`omar-action${action.hasValue ? " filled" : ""}`}
                    points={diamondPoints(ORIGIN, ACTION_SIZE.width / 2)}
                  />
                  <text className="omar-action-label" y={27} textAnchor="middle">
                    {action.name}
                  </text>
                </g>
              ))}
            </g>

            <g className="omar-timers">
              {layout.timers.map((timer) => {
                const radius = CLOCK_SIZE.width / 2;
                // The long hand sweeps the period; the short one is a fixed
                // dial mark, so a face still reads as a clock when stopped.
                const angle = timer.phase * Math.PI * 2 - Math.PI / 2;
                const hand = {
                  x: Math.cos(angle) * (radius - 8),
                  y: Math.sin(angle) * (radius - 8),
                };
                return (
                  <g
                    key={timer.id}
                    {...selectable(
                      timer.id,
                      `omar-timer-group${timer.firing ? " firing" : ""}`,
                    )}
                    transform={`translate(${timer.center.x},${timer.center.y})`}
                  >
                    <circle className="omar-timer-face" r={radius} />
                    {/* Grouped so the fire animation turns both hands about
                        the pin rather than each about its own end. */}
                    <g className="omar-timer-hands">
                      <line
                        className="omar-timer-hand"
                        x1={0}
                        y1={0}
                        x2={hand.x}
                        y2={hand.y}
                      />
                      <line
                        className="omar-timer-hand short"
                        x1={0}
                        y1={0}
                        x2={0}
                        y2={-(radius - 12)}
                      />
                    </g>
                    <circle className="omar-timer-pin" r={1.8} />
                    <text className="omar-timer-label" y={radius + 12} textAnchor="middle">
                      {timer.name}
                    </text>
                    <text className="omar-timer-meta" y={radius + 22} textAnchor="middle">
                      {timer.period > 0
                        ? `${formatDuration(timer.offset)}, every ${formatDuration(timer.period)}`
                        : `once at ${formatDuration(timer.offset)}`}
                    </text>
                  </g>
                );
              })}
            </g>

            <g className="omar-reactions">
              {layout.reactions.map((reaction) => (
                <g
                  key={reaction.id}
                  {...selectable(
                    reaction.id,
                    `omar-reaction ${reaction.status}${reaction.web ? " web" : ""}`,
                  )}
                  onDoubleClick={(event) => {
                    // The canvas resets the view on double click; a reaction
                    // opens the agent that runs it. A web agent has no pane to
                    // attach to, so what opens is the panel it is answered
                    // through — the same gesture, the thing that is there.
                    event.stopPropagation();
                    if (reaction.web) onOpenPanel?.(reaction.agent);
                    else onOpenTerminal?.(reaction.agent);
                  }}
                  transform={`translate(${reaction.box.x},${reaction.box.y})`}
                >
                  <polygon
                    className="omar-reaction-body"
                    points={chevronPoints(reaction.box.width, reaction.box.height)}
                  />
                  <rect
                    className="omar-reaction-badge"
                    x={17}
                    y={reaction.box.height / 2 - 10}
                    width={20}
                    height={20}
                    rx={5}
                    ry={5}
                  />
                  <text
                    className="omar-reaction-index"
                    x={27}
                    y={reaction.box.height / 2 + 4}
                    textAnchor="middle"
                  >
                    {reaction.order + 1}
                  </text>
                  <text
                    className="omar-reaction-name"
                    x={REACTION_TEXT_X}
                    y={reaction.box.height / 2 - 2}
                  >
                    {reaction.name}
                  </text>
                  <text
                    className="omar-reaction-meta"
                    x={REACTION_TEXT_X}
                    y={reaction.box.height / 2 + 12}
                  >
                    {reaction.meta}
                  </text>
                  {/* A deadline the program set for itself. A stopwatch rather
                      than a clock: a timer fires on its own, this counts down
                      against work already running. */}
                  {reaction.within === null ? null : (
                    <g
                      className="omar-within"
                      transform={`translate(${
                        reaction.box.width - CHEVRON_NOTCH - WITHIN_ROOM / 2
                      },${reaction.box.height / 2})`}
                    >
                      <title>{`must answer within ${formatDuration(reaction.within)}`}</title>
                      {/* The crown, which is what tells it from a clock face. */}
                      <line
                        className="omar-within-crown"
                        x1={0}
                        y1={-WITHIN_RADIUS - 5}
                        x2={0}
                        y2={-WITHIN_RADIUS - 2}
                      />
                      <circle className="omar-within-face" cy={-4} r={WITHIN_RADIUS} />
                      <line
                        className="omar-within-hand"
                        x1={0}
                        y1={-4}
                        x2={3.6}
                        y2={-7.6}
                      />
                      <text className="omar-within-meta" y={14} textAnchor="middle">
                        {formatDuration(reaction.within)}
                      </text>
                    </g>
                  )}
                </g>
              ))}
            </g>

            <g className="omar-ports">
              {layout.ports.map((port) => (
                <g
                  key={port.id}
                  {...selectable(port.id, `omar-port-group ${port.kind}`)}
                  transform={`translate(${port.center.x},${port.center.y})`}
                >
                  <polygon className="omar-port" points={portTriangle(ORIGIN)} />
                  {/* Sits above the connection line so routing never crosses text. */}
                  <text
                    className="omar-port-label"
                    x={port.kind === "input" ? 10 : -10}
                    y={-11}
                    textAnchor={port.kind === "input" ? "start" : "end"}
                  >
                    {port.name}
                  </text>
                </g>
              ))}
            </g>
          </g>
        ) : null}
      </svg>

      {error ? <div className="diagram-error">{error}</div> : null}

      <div className="diagram-zoom">
        <button type="button" onClick={() => zoomBy(1 / 1.2)} aria-label="Zoom out">
          −
        </button>
        <button type="button" onClick={fit}>
          Fit
        </button>
        <button type="button" onClick={() => zoomBy(1.2)} aria-label="Zoom in">
          +
        </button>
      </div>

      <div className="diagram-legend" aria-label="Diagram legend">
        <span>
          <svg viewBox="0 0 12 12" aria-hidden="true">
            <polygon className="omar-port" points="2,1 10,6 2,11" />
          </svg>
          port
        </span>
        <span>
          <svg viewBox="0 0 12 12" aria-hidden="true">
            <polygon className="omar-action" points="6,1 11,6 6,11 1,6" />
          </svg>
          action
        </span>
        <span>
          <svg viewBox="0 0 12 12" aria-hidden="true">
            <circle className="omar-timer-face" cx="6" cy="6" r="5" />
            <line className="omar-timer-hand" x1="6" y1="6" x2="6" y2="2.5" />
            <line className="omar-timer-hand short" x1="6" y1="6" x2="8.5" y2="6" />
          </svg>
          timer
        </span>
        <span>
          <svg viewBox="0 0 18 12" aria-hidden="true">
            <polygon
              className="omar-reaction-body"
              points="0,1 13,1 18,6 13,11 0,11 4,6"
            />
          </svg>
          reaction
        </span>
        <span>
          <svg viewBox="0 0 18 12" aria-hidden="true">
            <polygon
              className="omar-reaction-body running"
              points="0,1 13,1 18,6 13,11 0,11 4,6"
            />
          </svg>
          running
        </span>
      </div>
    </div>
  );
}
