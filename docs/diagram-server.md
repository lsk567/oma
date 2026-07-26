# Topology diagram server

OMAR can expose a read-only diagram API for a running topology. The API is
derived from verified topology bytecode, so agents, ports, connections,
reactions, triggers, and effects keep the same identity as the runtime model.
It does not inspect or reconstruct the legacy tmux agent hierarchy.

Start it with an OMAR program:

```bash
omar run workflow.omar \
  --input request='Review this change' \
  --diagram-server
```

OMAR prints the selected loopback URL. Pass
`--diagram-address 127.0.0.1:7341` to use a stable port.

## Protocol

The first protocol version exposes:

- `GET /health` — process health
- `GET /v1/diagram` — the current semantic graph and execution state
- `GET /v1/events` — server-sent execution events

Snapshots and events include `protocol_version` and a monotonic `sequence`.
Stable IDs are namespaced as `agent::`, `port::`, and `reaction::`. Event
kinds include `run_started`, `tag_advanced`, `reaction_started`,
`reaction_completed`, `run_completed`, and `run_failed`.

The server deliberately binds only to a loopback address in this first
version. It permits browser clients with CORS headers but has no remote
authentication surface.

The API is presentation-neutral. A browser, VS Code webview, or desktop
application can share the same client-side layout and rendering package
without changing runtime semantics.
