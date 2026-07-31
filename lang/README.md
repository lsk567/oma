# OMAR language prototype

Build the Lean 4 compiler:

```sh
cd lang
lake build
```

Run a source program directly:

```sh
cargo run -- run tests/topology/src/HR.omar \
  --input resume=/absolute/path/to/resume.txt \
  --replace
```

The runtime invokes `omarc` internally and executes the resulting bytecode.
It finds `omarc` beside the `omar` executable, in the source tree's
`lang/.lake/build/bin` directory, or on `PATH`. Set `OMARC_BIN` to use an
explicit compiler path.

Use `omarc` directly only for compiler development and offline bytecode
inspection:

```sh
cd lang
lake exe omarc ../tests/topology/src/HR.omar /tmp/HR.bytecode.json
```

The runtime starts topology-scoped agents, delivers enabled prompts at each
logical tag, waits at the global tag barrier, and prints the final outputs.
`--replace` is required when sessions with the same agent names already exist.

## Serving programs over HTTP

`omar run` binds a diagram server that dies with the run, so it cannot be where
programs arrive. `omar serve` outlives individual runs and admits them:

```sh
omar serve --address 127.0.0.1:7340
```

```
POST   /v1/runs      { program, inputs, replace?, timeout_seconds? }
                     -> 201 { run_id, team, status, diagram_address, ... }
GET    /v1/runs      -> { runs: [ ... ] }
GET    /v1/runs/{id} -> one record, or 404
GET    /health       -> { status, protocol_version }
```

`program` is `.omar` source. Compilation and input validation happen before the
response, so a bad program is a 400 rather than a 201 that later fails. Each run
gets its own ephemeral diagram server; `diagram_address` is where to fetch
`/v1/diagram` and subscribe to `/v1/events`.

Agent sessions are named `prefix + agent`, so concurrent runs of one team would
collide. `serve` returns 409 while a team already has an active run.

Loopback only: the surface executes arbitrary programs.

### Talking to the executive assistant

`serve` also relays a conversation between the operator and the EA:

```
POST /v1/chat          { text }        -> delivered into the EA's session
GET  /v1/chat                          -> the conversation so far
GET  /v1/chat/events                   -> SSE; replays history, then streams
```

The EA gets two MCP tools, and only the EA: `omar_reply` to talk back, and
`omar_propose_design` to submit a program for approval. Both are gated on a
serve context that is present only for an EA that `serve` launched. Workflow
agents are restricted to `omar_set_port`/`omar_complete`, plain spawned agents
see neither, and calls are refused, not merely unlisted.

The EA proposes; it never executes. A proposal lands in the conversation and
stops there — only the operator can admit a run, via `POST /v1/runs`.

That context is baked in when the EA launches, so an already running EA cannot
gain the tools. `serve` says so and continues; `--restart-ea` relaunches it
(discarding its session).
