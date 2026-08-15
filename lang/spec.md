# OMAR Language Specification

Status: draft  
Implementation language: Lean 4  
Source extension: `.omar`

## 1. Purpose

OMAR is a declarative language for agent systems. A program defines agents,
typed ports, and prompt reactions. The language controls topology startup,
teardown, and mutation. A deterministic runtime coordinates the running system.
AI is used only inside prompt reactions, never for orchestration.

## 2. Syntax

```ebnf
program     = "team", identifier, "(", agents, ")",
              "{", { declaration }, "}" ;

agents      = [ agent, { ",", agent } ] ;
agent       = identifier, ":", identifier ;

declaration = input | output | action | timer | connection | prompt ;
input       = "input", identifier, ":", type ;
output      = "output", identifier, ":", type ;
action      = "action", identifier, [ "(", "delay", "=", delay, ")" ],
              [ ":", type ] ;
timer       = "timer", identifier, "(", delay, ",", delay, ")" ;
connection  = identifier, "->", identifier, "after", delay ;

prompt      = "prompt", identifier, "(", triggers, ")",
              "->", effects, [ deadline ], prompt-string ;
deadline    = "within", "(", duration, ")" ;
delay       = duration | "0" ;
duration    = natural, unit ;
unit        = "ns" | "us" | "ms" | "s" | "sec" | "min" | "h" | "hr" ;
triggers    = [ identifier, { ",", identifier } ] ;

effects     = effect, { ",", effect } ;
effect      = atom, [ "?" ]
            | "(", atom, { "|", atom }, ")", [ "?" ] ;
atom        = identifier, [ "=", literal ] ;

type        = "bool" | "int" | "float" | "string" | "path" | "bytes"
            | "list", "<", type, ">"
            | "option", "<", type, ">" ;
natural     = digit, { digit } ;
```

Identifiers are case-sensitive. `//` starts a line comment and `/* ... */`
delimits a block comment.

### 2.1 Declarations

- `input` is a typed external entry point.
- `output` is a typed externally observable result.
- `action` is a typed internal port.
- An action without a type is a signal carrying no value.
- `action a(delay=2ms) : int` gives `a` a fixed logical delay of two
  milliseconds.
- `a -> b after 3s` copies each value present on `a` to `b` with an additional
  logical delay of three seconds.
- `timer t(1s, 500ms)` fires at one second and every five hundred milliseconds
  after that. A period of `0` fires once, at the offset.
- `prompt agent(a, b)` declares a reaction on `agent` triggered when port `a`
  or port `b` is present. If both are present at the same tag, the reaction is
  invoked once with both values.

The prompt body is delivered to the agent. `$(name)` interpolates a trigger
value and may only reference that prompt's triggers. If a declared trigger is
not present in an invocation, its interpolation expands to `<absent>`.

### 2.2 Effect contracts

The expression after `->` declares the ports a reaction may set:

```omar
-> result
-> result, log?
-> (continue_work | accepted = false)
```

- `a, b` requires both effects.
- `(a | b)` requires exactly one effect.
- `a?` makes an effect optional.
- `a = value` supplies a constant effect.
- A bare typed effect requires the agent to set a value.
- A bare untyped action emits a signal.

Every trigger must be an input or action. Every effect must be an output or
action. A connection target must be an output or action, and its source and
target types must match. Values must match their port types.

### 2.3 Deadlines

`within(30s)` bounds how long one invocation may take, measured from when that
invocation starts. Without it the run-wide timeout applies.

See §2.4: a deadline is a duration like any other, and carries a unit.

What expiry does is read off the effect contract, so a deadline needs no
second clause saying what to fall back to:

- a contract that writing nothing already satisfies — `a?`, or no effects at
  all — completes the tag with no writes. The ports it would have written are
  absent, so whatever they feed does not fire.
- a contract requiring an effect — `a`, `(a | b)` — has not been honoured.
  There is no value to invent, so the run fails.

### 2.4 Durations

Every span of time in the language is a duration: `after`, a timer's offset and
period, an action's `delay`, and `within`. All are stored as nanoseconds.

A duration must carry a unit — `ns`, `us`, `ms`, `s` (or `sec`), `min`, `h` (or
`hr`). A bare number says nothing about its magnitude, and `3` meaning three
nanoseconds rather than three hours is not something a reader should have to
know from elsewhere.

`m` is rejected as ambiguous: `min` is minutes, `ms` is milliseconds, and the
two differ by one character and five orders of magnitude.

Zero is the one exception. `after 0` is the same instant in every unit, so it
needs none — though `0ms` is also accepted and means the same thing.

A unit binds to the number it touches. `after 3ns` is one duration; `after 3`
followed by a declaration beginning with a word is a delay and then that
declaration, and `3 ns` with a space between is an error rather than a
duration.

## 3. Compiled topology

The compiler produces a canonical topology containing:

```text
team
agents
typed ports
delayed port connections
prompt reactions in declaration order
```

The runtime derives a port-to-reaction subscription index from reaction
triggers. Explicit connections copy values between compatible ports without
invoking an agent.

Prompt declaration order is semantically significant. It orders reactions that
may write the same port. Unrelated reactions remain unordered and may run in
parallel.

For each reaction and port, the compiler computes:

```text
mayWrite(reaction, port)
mustWrite(reaction, port)
```

Optional effects and alternatives may write a port but do not necessarily write
it. `mayWrite` determines scheduling conflicts; `mustWrite` determines whether
a reaction is statically guaranteed to replace an earlier value.

## 4. Tagged runtime

The runtime owns one global event queue ordered by logical tag:

```text
tag = (timestamp, microstep)
event = (tag, flow, port, value)
```

Logical time is a nonnegative integer timestamp in nanoseconds. External inputs
begin at `(0, 0)`. Scheduling uses the following rules:

- An effect written to a port without a fixed delay at `(t, m)` is enqueued at
  `(t, m + 1)`.
- An effect written to `action a(delay=D)` with `D > 0` is enqueued at
  `(t + D, 0)`.
- A connection `a -> b after X` schedules `b` with logical delay `X`.
- If `b` is an action with fixed delay `D`, connection and action delays are
  additive: `b` is scheduled at `(t + X + D, 0)`.
- A total logical delay of zero schedules the value at `(t, m + 1)`.

Thus, zero-delay causality advances the microstep while positive logical delay
advances the timestamp and resets the microstep, matching LF-style superdense
time.

At each tag, the runtime:

1. pops all events at that tag;
2. validates and groups values by flow and port;
3. enables prompts with at least one declared trigger present for the flow;
4. builds an invocation dependency DAG;
5. executes the DAG;
6. waits for every invocation at the tag to complete; and
7. atomically enqueues resulting effects and connected values at their
   calculated superdense tags.

The runtime advances only after the global tag barrier closes.

### 4.0 Pace

A timestamp is nanoseconds since the run began, and by default the runtime
holds the logical clock against the wall clock: a tag at timestamp `T` does not
execute before `T` has elapsed. A delay is therefore a statement about *when*,
not only about order.

A tag whose moment has already passed — because a reaction took longer than the
gap to the next one — executes immediately rather than being pushed further out.
Being late is not a reason to be later. How late is a separate question, and one
the language does not yet ask.

Microsteps carry no time. Every tag at the same timestamp is due at the same
moment, which is what makes zero-delay causality instantaneous rather than
merely fast.

`--fast` runs the same program as quickly as the work allows. Tags, ordering
and outputs are identical; only the waiting is skipped. It is what a test wants
and what a program's meaning must not depend on.

### 4.1 Invocation DAG

Each enabled prompt invocation is a DAG vertex. Add an edge `A -> B` when:

- `A` is declared before `B`; and
- both may write at least one common port.

Invocations assigned to the same non-concurrent agent are also ordered by
declaration order. Since all edges follow declaration order, the DAG is acyclic.

The runtime repeatedly runs all zero-indegree vertices concurrently, waits for
them to complete, removes them, and continues. Every firing order is therefore
a topological ordering of the DAG.

### 4.2 Agent protocol

Topology agents do not address other agents. Each active invocation receives
two scoped MCP tools:

```text
omar_set_port(invocation_id, port, value)
omar_complete(invocation_id)
```

The invocation's trigger map contains only the declared triggers present at
that tag. An absent trigger is omitted, while a present signal has the JSON
value `null`; this preserves the distinction between absence and a signal.

`omar_set_port`:

- only accepts ports listed in the invocation's effect contract;
- validates the value immediately;
- may be called many times; and
- uses last-writer-wins for repeated writes to the same port by that invocation.

Writes remain private until `omar_complete`. Completion validates the final
port snapshot against the contract. Calls after completion are rejected.

The topology runner owns invocation records in memory. Topology-scoped MCP
processes send authenticated commands to the runner over a loopback connection.
Each active invocation has a completion channel, so `omar_complete` wakes the
waiting reaction executor directly; invocation state is not persisted to or
polled from the filesystem.

If ordered invocations write the same port, the later declared invocation wins
when it actually writes that port. A mandatory effect is guaranteed to replace
the earlier value; an optional effect replaces it only when present.

When every invocation at the tag has completed, final snapshots are committed
in DAG order and routed through the subscription index. Agents send and forget;
the runtime performs all downstream delivery.

## 5. Topology lifecycle

Components have stable IDs derived from their qualified names. A compatible
agent retains its process, context, memory, and active work across topology
updates.

Mutation uses create-before-destroy:

1. verify the installed revision;
2. spawn new agents and install the new topology;
3. atomically activate the new revision;
4. drain obsolete reactions; and
5. kill obsolete agents.

An active invocation finishes against the topology revision on which it began.

## 6. Bytecode

Bytecode describes topology lifecycle. Runtime event coordination is not
encoded as bytecode instructions.

The user-facing runtime entry point is `omar run <program.omar>`. The runtime
invokes `omarc`, loads and verifies its temporary bytecode output, then removes
that output. JSON bytecode remains an internal compiler/runtime boundary and is
not accepted by the CLI.

```text
BEGIN_PLAN team

SPAWN_AGENT name backend
KILL_AGENT name

DEFINE_PORT kind name type [delay]
REMOVE_PORT name

CONNECT_PORTS source target delay

INSTALL_REACTION id agent triggers effects contract prompt within
UPDATE_REACTION id triggers effects contract prompt within
REMOVE_REACTION id

ACTIVATE_TOPOLOGY revision
COMMIT_PLAN
ABORT_PLAN reason
```

The initial construction subset consists of `BEGIN_PLAN`, `SPAWN_AGENT`,
`DEFINE_PORT`, `CONNECT_PORTS`, `INSTALL_REACTION`, and `COMMIT_PLAN`. Mutation
instructions are reserved for the next implementation stage.

JSON instruction fields are emitted in deterministic order with `op` first.
Port and connection fields are ordered:

```text
op, kind, name, type, delay
op, source, target, delay
```

The port `delay` field is omitted when no fixed delay is declared.
Reaction fields are ordered:

```text
op, id, agent, triggers, effects, contract, prompt, within
```

`within` is nanoseconds, and is omitted when the reaction declares no deadline.

The VM verifies the complete plan before performing effects. Unknown
instructions, invalid references, invalid types, or inconsistent contracts are
errors.

## 7. Lean 4 boundary

The compiler core is pure:

```lean
parse          : String -> Except ParseError Syntax
elaborate      : Syntax -> Except TypeError Topology
compile        : Topology -> Except CompileError Bytecode
verifyBytecode : Bytecode -> Except VerifyError VerifiedBytecode
```

MCP calls, event persistence, scheduling, and process observation belong to the
Rust runtime.

Important verification targets are:

- every trigger and effect references a compatible typed port;
- every per-tag scheduling graph is acyclic;
- bytecode never references an undefined component;
- compatible retained agents are never unnecessarily restarted; and
- equal compiler inputs produce equal bytecode.
