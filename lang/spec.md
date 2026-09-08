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
program     = { team }, main ;

team        = "team", identifier, [ "(", [ params ], ")" ],
                                  [ "[", [ agents ], "]" ],
              "{", { declaration }, "}" ;
params      = param, { ",", param } ;
param       = identifier, ":", type ;
agents      = agent, { ",", agent } ;
agent       = identifier, ":", backend ;
backend     = identifier ;

main        = "main", [ identifier ], "{", { instance | wiring }, "}" ;
wiring      = qualified, "->", qualified, [ "after", delay ] ;

declaration = input | output | action | timer | instance | connection
            | prompt | ";" ;
input       = "input", identifier, ":", type ;
output      = "output", identifier, ":", type ;
action      = "action", identifier, [ "(", "delay", "=", delay, ")" ],
              [ ":", type ] ;
timer       = "timer", identifier, "(", delay, ",", delay, ")" ;
instance    = identifier, "=", identifier, "(", [ args ], ")" ;
args        = literal, { ",", literal } ;
literal     = natural | string ;
connection  = endpoint, "->", endpoint, [ "after", delay ] ;
endpoint    = identifier | qualified ;
qualified   = identifier, ".", identifier ;

prompt      = "prompt", identifier, "(", [ triggers ], ")",
              "->", effects, [ deadline ], string ;
deadline    = "within", "(", duration, ")" ;
delay       = duration | "0" ;
duration    = natural, unit ;
unit        = "ns" | "us" | "ms" | "s" | "sec" | "min" | "h" | "hr" ;
triggers    = endpoint, { ",", endpoint } ;

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

### 2.1 Teams, agents and main

A team is a reusable unit. Its parameters go in `()`, its agents in `[]`, and
both lists are optional — `team Name { ... }` is a team with neither.

Declaring a team creates nothing; `main` instantiates. A program is the teams
plus the `main` that uses them:

```omar
team Node(idx : int)[agent : Codex]
{
    input token : int
    output out : int

    prompt agent(token) -> out "You are node $(idx). Token: $(token)"
}

main Relay {
    n1 = Node(1)
    n2 = Node(2)

    n1.out -> n2.token
}
```

A parameter is read back in a prompt as `$(idx)`, the same spelling a trigger
uses.

An agent's backend names what answers its reactions: `Claude` (or
`ClaudeCode`), `Codex`, `Cursor`, `OpenCode`, `Agy`, `Stub`, or `Web`. The
compiler takes any identifier here; an unknown one compiles and fails when the
runtime tries to start it. Case does not matter.

An argument is an int or a string literal, and nothing else — there are no
expressions. `main` may be named or bare; the name identifies the run, and a
runtime allows only one run of a given name at a time. A program without a
`main` does not compile.

Wiring in `main` is `instance.port -> instance.port`. Both sides must be
qualified, because in `main` there is nothing else for a bare name to mean.

A team may also instantiate a team, so teams nest:

```omar
team Stage(role : string)[worker : Codex]
{
    input inp : string
    output out : string

    prompt worker(inp) -> out "You are the $(role) stage. Got $(inp)."
}

team Pipeline[reporter : Codex]
{
    input brief : string
    output summary : string

    draft = Stage("draft");
    refine = Stage("refine");

    brief -> draft.inp
    draft.out -> refine.inp

    prompt reporter(refine.out) -> summary "Report $(refine.out)."
}

main Nest {
    run = Pipeline()
}
```

Inside a team, what it instantiated is reached as `instance.port` — the same
spelling `main` uses. A reaction may trigger on a contained instance's
*output* (`reporter(refine.out)`), which is how a team observes what it
contains; to send data the other way, connect to the contained input
(`brief -> draft.inp`). A bare name is this team's own port. From outside,
reach a nested port by its full path: `run.draft.out`.

A `;` may separate declarations in a team body and means nothing else. It is
not accepted in `main`.

### 2.2 Declarations

- `input` is a typed external entry point.
- `output` is a typed externally observable result.
- `action` is a typed internal port.
- An action without a type is a signal carrying no value.
- `action a(delay=2ms) : int` gives `a` a fixed logical delay of two
  milliseconds.
- `a -> b` copies each value present on `a` to `b` at the same tag. A plain
  connection costs nothing, so a reaction reading `b` sees the value in the tag
  it was written.
- `a -> b after 0` copies it one microstep later. No time passes, but the tag
  moves, which is what lets a cycle close.
- `a -> b after 3s` copies it three seconds later.
- `timer t(1s, 500ms)` fires at one second and every five hundred milliseconds
  after that. A period of `0` fires once, at the offset.
- `prompt agent(a, b)` declares a reaction on `agent` triggered when port `a`
  or port `b` is present. If both are present at the same tag, the reaction is
  invoked once with both values.

The prompt body is delivered to the agent. `$(name)` interpolates a trigger
value and may only reference that prompt's triggers. If a declared trigger is
not present in an invocation, its interpolation expands to `<absent>`.

### 2.3 Effect contracts

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

### 2.4 Deadlines

`within(30s)` bounds how long one invocation may take, measured from when that
invocation starts. Without it the run-wide timeout applies.

See §2.5: a deadline is a duration like any other, and carries a unit.

What expiry does is read off the effect contract, so a deadline needs no
second clause saying what to fall back to:

- a contract that writing nothing already satisfies — `a?`, or no effects at
  all — completes the tag with no writes. The ports it would have written are
  absent, so whatever they feed does not fire.
- a contract requiring an effect — `a`, `(a | b)` — has not been honoured.
  There is no value to invent, so the run fails.

### 2.5 Durations

Every span of time in the language is a duration: `after`, a timer's offset and
period, an action's `delay`, and `within`. All are stored as nanoseconds.

A duration must carry a unit — `ns`, `us`, `ms`, `s` (or `sec`), `min`, `h` (or
`hr`). A bare number says nothing about its magnitude, and `3` meaning three
nanoseconds rather than three hours is not something a reader should have to
know from elsewhere.

`m` is rejected as ambiguous: `min` is minutes, `ms` is milliseconds, and the
two differ by one character and five orders of magnitude.

Zero is the one exception. `after 0` names no span of time, so it needs no unit
— though `0ms` is also accepted and means the same thing. It is not the same as
writing no `after` at all: `after 0` buys a microstep, and omitting it buys
nothing. See §4.

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
port connections
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
begin at `(0, 0)`. Every hop costs one of three things:

- **Nothing.** An effect written to an `input` or `output` port, and a plain
  connection `a -> b`, land at the tag they were written. The value is readable
  by the rest of that tag.
- **A microstep.** An effect written to `action a` with no fixed delay, and a
  connection written `after 0`, land at `(t, m + 1)`. No time passes.
- **Time.** `action a(delay=D)` with `D > 0`, and `a -> b after X` with
  `X > 0`, land at `(t + D + X, 0)`. Connection and action delays are additive,
  and a microstep anywhere in the pair costs one microstep.

This is Lingua Franca's rule: ports do not introduce delays. What a tag decides,
it decides completely.

### 4.0 Fixpoint at a tag

A tag is a moment something is present at. One with no events is not reached:
no reaction can be enabled there, and no connection, output or timer can move,
so announcing it would report an advance that did not happen. A program admitted
with no inputs and no timer therefore passes through no tags at all.

A reaction fires at most once per tag, and only once the presence of every one
of its triggers is decided. Ordering is not something a program asks for; it
follows from the wiring.

Reaction `A` must precede `B` when:

- `A` writes a port that reaches one of `B`'s triggers over hops that cost
  nothing; or
- both write a port in common, and `A` is declared first; or
- both run on the same agent, and `A` is declared first.

Those constraints form a graph. A cycle in it is a **causality loop**: every
reaction on the cycle would have to run before itself, and no order exists. The
runtime rejects such a program before it runs rather than picking an order.
Breaking a loop means saying what it costs to go round — an action, or a
connection written `after 0`.

A program without a loop has a topological order, and the runtime walks it once
per tag:

1. pop every event at the tag;
2. carry each connection whose source is present, instantaneous hops joining
   the tag and the rest going on the queue;
3. take the next group of reactions that need not follow each other, and invoke
   those with a trigger present;
4. wait for that group, then merge its effects — instantaneous ones join the
   tag, so reactions further down read them — and carry connections again;
5. repeat from 3 until the order is exhausted.

The tag is over when nothing further can fire. Its outputs are what its ports
hold then.

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
moment, which is what makes a microstep a step in order rather than in time.

`--fast` runs the same program as quickly as the work allows. Tags, ordering
and outputs are identical; only the waiting is skipped. It is what a test wants
and what a program's meaning must not depend on.

### 4.1 Invocation DAG

The precedence graph of §4.0 is the invocation DAG. It is a property of the
wiring, computed once, and the same at every tag; what changes from tag to tag
is only which of its vertices have a trigger present.

Two of its three edge kinds follow declaration order and so cannot cycle. The
third — `A` writes what `B` reads — can, and that is the loop the runtime
refuses.

### 4.2 Agent protocol

Topology agents do not address other agents. Each active invocation receives
two scoped MCP tools:

```text
omar_set_port(invocation_id, port, value)
omar_complete(invocation_id)
omar_pending()
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

`omar_pending` lists the invocations addressed to the asking agent that are not
yet complete, each with its prompt, trigger values and permitted effects. It is
scoped to that agent, so it discloses nothing the agent's own reactions were not
already wired to see.

When every invocation at the tag has completed, final snapshots are committed
in DAG order and routed through the subscription index. Agents send and forget;
the runtime performs all downstream delivery.

### 4.3 The `web` backend

An agent declared `[panel : Web]` is answered by a web client rather than a
spawned process. Nothing is started for it: no command is resolved, no pane is
opened, and no readiness is waited on. The agent exists as a name in the
topology and an inbox in the invocation registry.

The name says what answers the reaction, not how the bytes get there. `Codex`
and `ClaudeCode` do not encode their transport either; a `Web` agent's panel may
reach it however it likes, and changing that must not mean editing the program.

Everything after that is unchanged. Its reactions are invoked when their
triggers are present, their prompts are interpolated the same way, and their
writes are checked against the same effect contract. A client answering is a
slow agent, and the runtime does not distinguish them:

- it sees exactly its reaction's trigger values, because that is what any
  invocation carries;
- it may write exactly its reaction's effects, because `omar_set_port` refuses
  anything else;
- it is bound by `within` like any other reaction, and an expired deadline is
  read off the contract as in §2.4.

An operator's decision is therefore recorded dataflow rather than a side effect
on the run: it flows through the same completion path as a model's, so a run
with a person in it can be replayed by supplying the recorded writes.

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

CONNECT_PORTS source target [delay]

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
