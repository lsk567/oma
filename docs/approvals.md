# Approval monitoring in Omar's web UI

Approval requests are independent of execution status. A blocked agent shows an
orange dot with **Waiting for approval**, while other agents retain their running
state. The web header counts pending requests; clicking it opens the request and
selects its invocation when the corresponding topology is displayed. The executive
assistant has an indicator in its chat header even before a topology exists.

The review panel shows the action, tool, scope and elapsed waiting time. **Open
agent terminal** opens the same agent session where the backend can accept the
operator's response. Opening or closing the panel/terminal never resolves a request.
Inline Allow/Deny actions are intentionally absent until an integration supports
them end to end.

## Supported sessions

The initial observer supports Codex sessions attached to the per-pane app-server
that Omar provisions. It subscribes to the one loaded thread without overriding
the session's model, permissions or approval policy. Commands, file changes,
additional permissions, MCP elicitations and MCP tool approval questions provide
structured signals. Ordinary questions and silence do not.

An explicit backend `waitingOnApproval` flag can provide a generic request when
detailed replay is unavailable. Such a request directs the operator to the
terminal for the exact scope; its timer starts when Omar detects it. Detailed
requests use the backend's timestamp where available. MCP approval questions are
recognized by the backend's `mcp_tool_call_approval_` question marker.

Other backends, older versions without these APIs, command wrappers that prevent
attachment, and ambiguous panes with multiple loaded threads cannot provide full
monitoring. The API reports connection availability independently; the assistant
header offers terminal access when monitoring is unavailable. This feature does
not change how agents are launched or relax their permissions.

## Lifecycle and wire protocol

`GET /v1/approvals` returns an `ApprovalSnapshot`. `GET /v1/approvals/events`
streams complete snapshots as SSE `approvals` events, including an initial
snapshot on every connection. Wire types are generated from Rust alongside the
rest of the UI protocol. Requests are scoped to an agent, run and invocation;
the assistant has no run or invocation requirement.

Reloading the page preserves pending requests in the daemon. Disconnects retain
them and show **Connection lost**. Reconnection replays backend requests and
reconciles authoritative thread state. Acknowledgements resolve requests; a bare
acknowledgement is never described as approval or successful completion. Explicit
declined item status is recorded as denial; run teardown cancels remaining requests.
The recent resolution list is bounded. Daemon restart persistence is not provided;
live supported sessions are rediscovered and pending requests replayed.

Only bounded display metadata is published. Private reasoning, tool results and
arbitrary argument objects are excluded. Obvious credential-bearing commands and
explanations are hidden; the terminal remains the authoritative full review view.

Protocol reference: [Codex app-server](https://learn.chatgpt.com/docs/app-server).
