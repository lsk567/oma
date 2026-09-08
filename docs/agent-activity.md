# Agent activity in the web UI

Inspect an agent from the topology's **Inspect agent** picker or the selected
reaction's **View activity** action. Nodes show task/role, runtime state and
invocation elapsed time. The panel contains current tools, latest activity,
recent events, attributable file changes and diffs. A successful file edit is
**changed**, not **verified**. Command exit codes are check evidence; Omar does
not infer that they verify every file.

## Sources and compatibility

- Every `omar serve` workflow backend: runtime invocation start/end and elapsed
  time, recorded independently for concurrent agents. Activity does not wait for
  committed outputs or the rest of the execution layer.
- Codex per-pane app-server: `thread/loaded/list` must identify exactly one thread.
  `thread/read` hydrates history; `thread/resume` subscribes without configuration
  overrides. Allowlisted item starts/completions produce concise tool categories,
  errors and scoped file-edit patches. Neither terminal scraping nor elapsed time
  is used to infer tool execution.
- Backend turns are attributed by the delivered `invocation_id` user-message
  marker or the `turn/start` response. Bootstrap/manual turns are not attributed
  merely because their timestamps are close. Known IDs deduplicate replay;
  completed invocations reject late updates.
- Some Codex versions/new threads refuse history or resume before materializing a
  user message. Metadata-only reads retain configuration access; detailed activity
  is explicitly unavailable and retried. Ambiguous or changed threads fail closed.
- Other backends: runtime timing plus **Activity details unavailable** and terminal
  access. No inferred reasoning, tool execution, model/effort, or file attribution.

Unix milliseconds come from Omar for runtime lifecycle/observation and from the
backend when item lifecycle timestamps exist. Recovered history without timestamps
is labeled as recovered/observed; it does not invent tool starts. The browser clock
is anchored to server time. A long-running open tool suppresses the inactivity
notice; a disconnect is shown separately. The default notice is two minutes,
configurable from 30 seconds to one hour; it never interrupts execution.

## Model and effort

**Role model and effort settings** uses live `model/list` capabilities. Only Codex
choices returned by a connected pane are offered; refresh retries discovery.
Requested choices persist in the daemon's EA-scoped `role-settings-<id>.json`.
Each invocation snapshots its requested settings at start. With an explicit model,
Omar validates that pane's current catalog and submits `turn/start` with that model
and the requested/catalog-default effort, without changing approval policy. A
missing/ambiguous connection or rejected request fails explicitly, never silently
restarting or retrying a potentially accepted turn through the terminal.

**Keep backend configuration** means no override; it does not reset sticky backend
settings. Mid-run changes apply to the next invocation. The accepted request,
reported thread configuration, and confirmed execution settings are separate.
The current adapter does not have per-turn execution confirmation: confirmed model
and effort say **Not reported** even if a request was accepted or thread settings
were reported. Account/provider rejection remains a backend error.

## Artifacts, privacy and retention

Only successful `fileChange` items identify attributable created/modified/deleted
files. Diffs come from that tool item, never a shared-workspace watcher. Shell/MCP
edits without a structured file-change item may be missing. Paths outside the
workspace, traversal, symlinks, hidden/credential files are omitted. Raw tool
arguments, commands, output, reasoning and user-input requests are not exposed.
Sensitive patch lines/key blocks are redacted. Each diff is capped at 16 KiB;
truncation is explicit. This filtering intentionally favors omitted detail.

`GET /v1/runs/<run_id>/activity` returns a sequenced snapshot, polled once per second.
The daemon retains 100 recent invocations (all active ones retained), 200 recent
events and 30 recent artifacts per invocation. Reload/reconnect fetches the same
IDs and timestamps. The browser retains only the selected run and topology without
port values in session storage; activity is fetched afresh. Activity is in memory
for the daemon lifetime, including after run completion. Daemon restart/history
archival and restoration in a different browser are not implemented here.

`GET/POST /v1/role-settings` read/save requested role settings; saving never mutates
an active response. Rust owns these wire types; regenerate using
`UPDATE_PROTOCOL=1 cargo test --bin omar protocol`, then rerun without that variable.

## Separate approval source

Pending approvals belong to the independent [approval PR #246](https://github.com/omar-os/omar/pull/246): `/v1/approvals` and
`/v1/approvals/events`, with snapshot `sequence`, `requests`, `monitors`, `recent`.
Match `run_id` + `invocation_id` and `agent_id` (`agent::<qualified-name>`).
That source owns the orange waiting state, request links and approval counts.
Its connectivity must not replace execution/activity connectivity. This PR does
not infer approvals, add a response button, or include the terminal/history PRs
#244 and #245. When combining changes, retain both independent optional
`TopologyRunConfig` and `DiagramCanvas` props and regenerate protocol types.

## Verification

- Real runtime: Rust invocation-registry concurrency test and model-free daemon
  conformance with real stub-agent invocations; timing survives diagram shutdown.
- Real installed Codex 0.153.4, isolated empty app-server: Unix WebSocket handshake,
  `model/list` and metadata reads. Empty legacy/new-format history refusal confirmed.
  No model invocation or live user agent was used for this smoke check.
- Explicit fixtures: Unix-socket JSON-RPC settings/capability refusal; safe activity
  projection, duplicates, delayed events, privacy, recovery and independent agents.
  Playwright covers two concurrent agents, a long tool, inactivity threshold,
  request/active-setting separation, diffs, disconnect/reconnect, reload and
  completion at desktop and mobile sizes. Other backends' detailed telemetry and
  a live model-driven file edit remain unverified; approval lifecycle is tested by
  its separate PR.
