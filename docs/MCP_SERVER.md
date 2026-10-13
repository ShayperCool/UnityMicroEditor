# Agent access through MCP

The server uses the `rmcp` crate, pinned to `=3.5.1`, and requires the Rust
toolchain pinned in `rust-toolchain.toml` (1.99.0). Active execution remains
Rust and C#.

```sh
./unity-micro-editor mcp --project /absolute/UnityProject
./unity-micro-editor mcp --project /absolute/UnityProject \
  --transport http --listen 127.0.0.1:8091
```

The main CLI supplies source authoring, the Player controller and explicit public
Editor project actions. The `mcp-standalone` subcommand exposes
source authoring only, with the same `--project`, `--transport`, `--listen` and
`--token-file` flags. A generic client can use this stdio configuration:

```json
{
  "mcpServers": {
    "unity-micro-editor": {
      "command": "/absolute/UnityMicroEditor/unity-micro-editor",
      "args": ["mcp", "--project", "/absolute/UnityProject"]
    }
  }
}
```

HTTP serves Streamable HTTP at `/mcp` and availability at `GET /health`. It binds
only to a literal loopback IP, rejects cross-origin browser requests and DNS
rebinding, and supports a bearer token through `--token-file` (at least 32
characters). Do not expose this source authoring service through a public tunnel.

Raw agent preparation and controller output can persist under the project's
`Library/UnityMicroEditor`, for example `Preparation/first/current.json` and
`Sessions/agent` with explicit `--output`. The controller default is
`Library/UnityMicroEditor/agent`. Legacy `Temp` paths remain accepted; Unity Editor
clears that directory on restart. Paths elsewhere in Library or outside the
project are rejected. Native import, extraction and adoption retain their
profile-bound owned `Library/UnityMicroEditor` or legacy `Temp` paths; existing catalogs are not migrated.

Source tools are `project_inspect`, `project_capabilities`, `project_inventory`,
`asset_read`, `asset_create`, `scene_hierarchy`, `scene_edit`, `script_read`,
`script_create`, `script_edit` and `source_undo`. Resources
`unitymicroeditor://project` and `unitymicroeditor://capabilities` describe the
project, native source component types and configured backends. The source operations and
transaction limits are implemented in `rust/src/authoring/`.

Read a source revision before editing. Submit a unique `operationId` and the same
`expectedRevision`; stale source or metadata gives `revision_conflict`. Retry the
exact request and operationId after a lost connection. `source_undo` accepts
`path`/`expectedRevision` for single-file edits, or `expectedRevisions` for every
changed file in an Editor staged publication. Creation undo does not erase files.
Source publication does not imply compile, import or runtime success.

Source reads page by UTF-8 byte offset: defaults are 262,144 source bytes and
65,536 metadata bytes; the maximum is 1,048,576 per page. Follow `nextOffset` and
`metadataNextOffset`, with explicit lengths and truncation flags. Bind later pages
with `expectedRevision`. `project_inventory` defaults to 100 entries and permits
up to 500, with path/query/extension filters and snapshot revision guards. Its
revision binds names, file statistics and metadata; optional per-page
`sourceRevision` values bind complete supported text bytes.

`scene_hierarchy` defaults to expanded nested prefab/variant objects, with source
provenance and dependency revisions. Its `revision` guards the expanded graph;
`sourceRevision` guards writes to the scene/prefab itself. Composite IDs are read
identities; local IDs in provenance address source edits. `resolvePrefabs: false`
retains the raw hierarchy behavior. Both views page by `offset` and `limit`
(default 100, maximum 500), and support `includeFields: false`. Replies above
8 MiB give `response_too_large`; narrow the page or filter. Known source paths
remain readable when a complete project inspection exceeds that limit.
Tool registration validates a separate JSON copy and preserves original numeric
tokens when decoding arguments. Integer source fields and replies retain their
64-bit values. Prefab override `value` accepts scalar, reference, array and null
JSON values; an omitted value remains absent.

A configured Player adds `session_control`, `runtime_query`, `runtime_command`,
`project_tools`, `hot_reload` and `remote_session` (a browser page with the Player's video and
input on a listener of its own; see [AGENT_MCP.md](AGENT_MCP.md)). Start can return `phase: starting`; wait for readiness before
queries. While start still prepares the project it returns `result_pending` with
`progress` (stage and Editor log path), and `status`/`wait` report the same
preparation. `start`/`reset` take `gameArguments`, `contentScenes`,
`contentPrefabs`, `addressables`, `rebuildHost`, `hotReload` and
`onUnsupportedEdit` in `arguments`; see
[AGENT_MCP.md](AGENT_MCP.md). Query operations are `status`, `hierarchy`, `inspect`, `logs`, `raycast`
and `tools`. Commands include `screenshot`, `pause`, `resume`, `wait_frames`,
`custom`, `invoke`, `simulation_pause`, `simulation_resume`, `simulation_step` and
`input`. Arguments go in `arguments`; `requestId`, `expectedPid` and
`expectedRevision` bind a request to a Player generation. The legacy `pause`
changes scaled time. `simulation_pause`/step use the supported simulation controls
and report their actual behavior separately from Unity Editor Pause.

`screenshot` defaults to `arguments.mode: "screen"`: the complete viewport at
its actual size, including overlay UI. `mode: "camera"` accepts a camera
`target` (source `path` or `fileId`) or `camera` path, with optional `width` and
`height`; it reports `includesOverlayUi: false`. Both modes require a graphics
Player and return a PNG path, hash, size and frame. Screen capture completes
after the end of the frame; preserve the request identity while waiting.

`hot_reload` has the actions `status` and `recompile`. A saved C# edit is
compiled by the running session and applied to the running Player when
possible; the Player keeps its process and game state, and
`session_control(wait_update)` succeeds with the same PID and an advanced
`hotReload.codeGeneration`. `session_control(wait)` reports an edit in the
same way once a Player is ready, and also returns for a start or a restart.
Both waits first ask the runner to say when it has answered every source
change that was saved before the call, so one wait after several edits gives
the outcome of the last, however far apart they were saved; an edit made while
the call runs is not covered. `timeoutSeconds` bounds a wait, which then fails
with `wait_timeout`.
An edit that cannot be applied live is refused, not approximated: the Player
keeps the previous code, both waits fail with the error code
`full_recompile_required`, and the session state has
`hotReload.fullRecompileRequired: true` with the members and reasons. Neither
wait succeeds while that member is true, whatever `afterSequence` is.
`hot_reload` with `action: recompile` then restarts the Player on the newest
compiled code and resets game state; it returns the new
session state, or the compiler's text when the sources do not compile, which
keeps the Player. A change that is applied, or needs no patch, can still leave
something the Player takes over only after a recompile, such as the body of a
coroutine that is running: the reply of the wait then starts with `attention`
and `hotReload.newNotes` lists it, while `hotReload.lastPatch.notes` and
`hotReload.notes` hold every note that stands. While the runner has marked an
answer as not its last the state has `updatePending: true` and neither wait
returns. A session reply lists the newest events of the session and a wait
those after `afterSequence`; `events` sets how many. The events of a reply
leave out `notes`, the standing list that the state holds. `status` returns the
`hotReload` state, also while no session runs. The session options `hotReload: false` (restart on every C# edit) and
`onUnsupportedEdit: "restart"` (restart instead of the error) select the other
behaviors. The session state omits `hotReload` while it has nothing to report:
hot reload is enabled, no patch is applied, no edit is refused, no note
stands, no recompile is pending and hot reload did not restart this Player.
[AGENT_MCP.md](AGENT_MCP.md) describes the flow and its limits.

`project_tools` publishes public attributed/registered methods as
`project.<name>` MCP tools. Their `parameters` follow the project's DTO schema,
with a required `target` containing either `path` or `fileId` for component
methods. Mutating methods require `requestId`. Omitted PID/revision guards bind
the call to the catalog generation. The catalog refreshes after ready/update
waits and `hot_reload(recompile)`, and drops stale tools after updates,
stop/reset or failed discovery. A code patch keeps the catalog: it changes
neither the PID nor the revision.
Read-only annotations come from the public project declarations. Invalid names,
bindings or schemas are reported and not registered. There is no generic Editor
code evaluator or foreign private reflection API; source C# authoring and
verified compilation intentionally execute project code.

A configured project action handler adds five explicit tools:

- `project_prepare`: `scene`, `requestId`, optional `hostPlayer`, `nativeCatalog`
  and `activate`; exports the public graph/proof, prepares verified source and
  reports its baseline separately from Player readiness.
- `project_build`: explicit `scenes`, `requestId` and optional `development`;
  invokes the public Editor build backend into a new owned output directory.
- `prefab_edit`: `action`, `path`, `expectedRevision`, `operationId`, plus explicit
  target/source/output and override fields; public PrefabUtility plans in an
  isolated copy, then guarded publication writes the approved output set.
- `component_schema`: `componentType`, `requestId`; captures native serialized
  defaults and required companion components without target source changes.
- `component_add`: `path`, `fileId`, `componentType`, `expectedRevision`,
  `operationId`, optional `requestId` and field overrides; attaches captured
  components in one source transaction.

The main MCP CLI runs these five Editor actions as detached jobs. The initial
reply has `status: pending`, request/operation identity and `jobStatusPath` under
`Library/UnityMicroEditor/action-jobs`. Poll the same tool with identical arguments
and the same ID until a confirmed result arrives. A client timeout leaves the job
running. Completed results and known failures survive server restart. Pending
recovery retries the same coordinator phase identities; accepted Editor requests
with unknown outcomes are never automatically reexecuted. The service bounds
active jobs to 128 and cancels owned work when it closes. Session start without a
baseline can return a preparation job first; retry start after confirmation, then
wait for Player readiness. Command-line preparation/build remain synchronous.

Tool replies contain `success`, optional `data`, and a machine-readable `error`.
Rejected backend receipts set MCP `isError: true` and retain identity/revision in
`error.details`. An unconfirmed receipt returns `success: false` and
`status: pending`; this does not mean failure or successful application. Retry the same
requestId to obtain its receipt. Unknown outcomes require inspecting the session
before retrying the same requestId. Oversized confirmed mutation replies retain
operation/request identities so the result remains recoverable.

Error codes of hot reload:

| Code | Returned by | Meaning and `error.details` |
| --- | --- | --- |
| `full_recompile_required` | `session_control(wait_update)` and `session_control(wait)`; `hot_reload(recompile)` when an edit compiled after its restart was refused | The C# edit compiled but cannot be applied to the running Player, which keeps the previous code. `unsupported` (at most 16 of `{code, member, message}`), `unsupportedCount`, `report` (file with the complete result) and `next`, the call that applies the edit: `{"tool": "hot_reload", "arguments": {"action": "recompile"}}`. `rejected` (`{error, sequence}`) when the running Player rejected a scene or asset change after `afterSequence` while the refusal stands. |
| `wait_timeout` | `session_control(wait_update)` and `session_control(wait)` with `timeoutSeconds`, or when the request's own deadline passes | The wait ended without an answer; nothing was decided and the same call can be repeated. `afterSequence`, `phase`, `lastSequence`, `lastUpdateSequence`, `lastUpdateRejectedSequence`, `updatePending`, and `runnerAnswered`: false while the runner has not said that it answered every change saved before the call. |
| `session_not_ready` | `hot_reload(recompile)` | No Player is ready. `phase` of the session. |
| `recompile_pending` | `hot_reload(recompile)` | Another recompile has not been answered. `requestId` of the pending one. |

Both waits return `full_recompile_required` until the code is in the Player:
for every `afterSequence`, and also after a later scene update, which the
message then reports as applied, or a later scene edit that the running Player
rejected; the message and `details.rejected` then have that rejection's own
text, and `session_control(status).updateError` has it too. A C# compile error
keeps the code `operation_failed` with the compiler's text, from both waits
and from `hot_reload(recompile)` alike. So does a `recompile` whose runner no longer
reads commands; its message names `session_control(reset)`. A `requestId` of
`recompile` names one command: a repeated id returns that command's outcome
and does not restart the Player again.

```sh
cargo test --locked --lib mcp::
cargo test --locked --test mcp
```

Tests cover real stdio framing and Streamable HTTP, authentication and Origin/Host
rejection, paging and source conflicts, runtime routing, project action schemas,
exact DTO validation, signed 64-bit argument forwarding, catalog generation
changes, component target selection, and the `hot_reload` tool with its typed
errors. The controller's hot reload events and the `recompile` command are
tested with a simulated runner in `rust/src/agentcontrol/tests.rs`.
