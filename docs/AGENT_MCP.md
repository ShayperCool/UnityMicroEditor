# Connect agents to Unity Micro Editor

Unity Micro Editor exposes authored project files and a reusable Mono Player over
MCP. Agents can prepare a project, inspect and edit supported assets, compile C#,
apply supported C# edits to the running Player, control the simulation, send
supported input, and call project tools declared with C# attributes. Read
`project_capabilities` before selecting an operation.

The implementation uses several explicit backends. Source transactions, prefab
operations, package resolution, Player assembly graphs and scene loading are ours;
SDK Roslyn compiles game code. Native component defaults are captured once with
public Unity APIs and read from an engine/context-bound cache. Native hosts and
uncovered heavy content can still require a one-time Editor bootstrap. **`project_build`
proxies to the real Unity Editor's `BuildPipeline`. It requires the installed
Editor and is not an independent replacement build pipeline.** Replies identify
the backend and preserve the Editor receipt and build report.

## Start with an explicit project and scene

Install Unity `6000.6.3f1`, revision `45d8eee7de74`, with Linux x64 Mono support.
Build with the Rust toolchain pinned in `rust-toolchain.toml` (1.99.0, installed through rustup). The verified graphics path is NVIDIA OpenGL Core. Tool
sources resolve from `UNITY_MICRO_EDITOR_ROOT` or the installation directory;
target projects are never inferred from this repository's parent directories.

```bash
/absolute/UnityMicroEditor/unity-micro-editor mcp \
  --project /absolute/UnityProject \
  --scene Assets/Scenes/SampleScene.unity \
  --output /absolute/UnityProject/Library/UnityMicroEditor/Sessions/agent
```

Stdio is the default. An agent's MCP configuration can use:

```json
{
  "mcpServers": {
    "unity-micro-editor": {
      "command": "/absolute/UnityMicroEditor/unity-micro-editor",
      "args": ["mcp", "--project", "/absolute/UnityProject", "--scene", "Assets/Scenes/SampleScene.unity"]
    }
  }
}
```

Source tools work immediately. Without `--baseline`, `session_control` with
`action: start` prepares the selected scene automatically. Preparation never
writes `Packages/manifest.json`, the lock or any other authored file of the
target: the runtime package is provided to our resolver from the installation
directory and declared only in the staging copies that run an Editor. No Editor
opens the target project during preparation. The package resolver reads manifest,
lock, embedded/file/Git/registry dependencies. Our graph reads `.asmdef`, `.asmref`,
source ownership, platform/define/version constraints and compiler options.
The installed SDK compiles the result. A package-compatible native Mono host and
its exact engine facts are captured once when no matching profile exists. This
initial native bootstrap also captures the component-default catalog.
Supported native dependencies are prepared in an isolated copy. The scene is
written in the Player format by Unity's standalone import worker and loaded by
the engine in the Player ([NATIVE_IMPORT.md](NATIVE_IMPORT.md#scenes-in-the-player-format));
it is not bundled into an AssetBundle.

Preparation starts with checks that need no build: authored metadata identities
(the issues `project_inspect` lists: a `.meta` without exactly one GUID,
duplicate GUIDs, symlinks) and the presence of
`ProjectSettings/ProjectSettings.asset`. They fail in the first second with
`invalid_identity` or `project_settings_missing`. A source without `.meta` stops
only a preparation that needs an Editor stage.

Preparation completion and Player readiness are distinct. Reissue the same
`session_control(start)` request while preparation is pending, then call
`session_control(wait)` for actual readiness. The reissued request may keep
the preparation inputs it began with (`hostPlayer`, `nativeCatalog`,
`engineProfile`, `rebuildHost`): once that preparation is the session's, they
are what the session runs, and the start goes on. Another input, or one of
another type than a path (or `true` for `rebuildHost`), is refused. A pending reply carries
`progress`: the current `stage`, its index among the stages, elapsed seconds
and, for Editor stages, `logPath` of the staging Editor's log. While a
preparation runs, `session_control(status)` adds the same object as
`preparation`, and `wait`/`wait_update` answer `result_pending` with it instead
of reporting a stopped session. The CLI prints the stages to stderr. Long project actions run as durable
jobs: poll identical arguments and the same `requestId` or `operationId`.
Disconnecting the MCP request does not cancel an accepted preparation or build.
A confirmed job is read after server restart without repeating its backend work.

To prepare before connecting an agent:

```bash
/absolute/UnityMicroEditor/unity-micro-editor auto-prepare \
  --project /absolute/UnityProject \
  --scene Assets/Scenes/SampleScene.unity --request-id initial-preparation
```

The result names the baseline, host, engine profile, package snapshot and native catalog. Existing validated
baselines remain accepted through `mcp --baseline /absolute/.../current.json`.
`--host-player` supplies an explicitly compatible host. Without it a preparation
reuses the newest host an earlier request built for this project and Editor,
together with its engine profile and, for the same scene, its native catalog;
a new `requestId` alone never rebuilds the host. The result records this as
`backends.hostSource: reused:<requestId>`. Only a project without such a host
builds one. A reused host is verified against the current sources where it is
used; when it no longer fits (changed packages), the
preparation fails with `host_reuse_failed`, and `rebuildHost: true` with a new
`requestId` builds a new one. Settings do not tie a project to its host: a
preparation writes the Player's settings managers from the project's
`ProjectSettings` over the host's own ([HOSTLESS_PLAYER.md](HOSTLESS_PLAYER.md));
a host of 6000.0 keeps its own ([UNITY_RELEASES.md](UNITY_RELEASES.md)).
`--editor` selects the matching Unity installation;
the default is `UNITY_EDITOR`, then the installation of the project's release under `$HOME/Unity/Hub/Editor`.
`--compile-for Android` (or another build target group) compiles the Player's
code as for that device, for a game written for a phone; the choice is kept for
the project ([UNITY_RELEASES.md](UNITY_RELEASES.md#a-game-written-for-a-device)).
`--play-mode` compiles it as the Editor's Play mode does, with `UNITY_EDITOR`,
so that code the game keeps for the Editor (SDKs that answer at once there
instead of calling a device's native library) runs in the Player as well; it is
kept beside `--compile-for` ([UNITY_RELEASES.md](UNITY_RELEASES.md#play-mode-code)).
`--engine-profile` selects a captured engine/host profile. Add `--no-editor` to
`mcp` or `auto-prepare` to forbid main Editor bootstrap and build calls; missing
native facts fail explicitly instead of launching it. Prepared native import
workers and the Mono Player are separate processes from the main Editor.
Manual `compiler-prepare` and `prepare-project --scene-proof` remain available for
explicit public graph/proof exports. Do not relabel another project's evidence.

Projects without gameplay scripts use an explicit verified native-only baseline:
the host's assembly-registration digest and package GUID/type bindings remain
required. Adding gameplay scripts or changing package and assembly configuration
uses a fresh source graph and SDK preparation; engine-bound native assembly slots
and retained content remain verified. Package and assembly configuration changes
also trigger preparation recovery; ordinary C# compile errors retain the last
confirmed Player. The engine runs every lifecycle callback itself, inherited
ones included; the compiler does not rewrite them.

Owned outputs are under the explicit project's `Library/UnityMicroEditor` and
`Builds/UnityMicroEditor`. Automatic host builds run in the owned staging copy
`Library/UnityMicroEditor/stage/project`: package build processors, including
Addressables, can change generated Assets only there. Every preparation of the
project reuses that copy and updates only the files that changed, so a host
rebuild starts from the import, script and Player build caches of the build
before it. The same staging Editor exports the public Player graph, whose paths
are then mapped back to the target's authored roots and to the package roots our
resolver verified. `hostEvidence` links the immutable source snapshot, Editor
receipt, package profile and verified host artifacts. The target's authored
roots are checked on both success and failure.

Addressables content built by the host build stays with the host; a session
serves the project's addressable scenes and assets from sources instead
([Addressables from sources](#addressables-from-sources)), so the host's copy
matters only for code and shaders addressed directly and for
`Addressables.InitializeAsync`. When there is any, the preparation result lists
it in `hostContent`:
`addressablesStreamingAssets` (local groups, inside the host's
`StreamingAssets/aa`, which the session's Player copy contains) and
`addressablesRemoteBuild` (remote groups, the staging copy's
`ServerData/StandaloneLinux64`). It is not published into the target's
`ServerData`, and content edits do not rebuild it: local groups are as old as
their host, and the shared staging copy's `ServerData` holds the remote groups
of the latest host build. A
game that loads remote groups from a configurable location can be pointed at
that directory through `gameArguments`.

Explicit `project_build` invokes the target Editor's normal build pipeline and
package build processors. It is the one action that runs an Editor in the target
project, and it needs the runtime package there. The tool does not add it:
without a declaration in the target's own `Packages/manifest.json` the action
fails with `runtime_package_missing` and names the dependency to add.
The staging copy seeds its import cache once, when it is created, from the
target's `Library` when no Editor has the target open, or after an Editor the
tool owned there has stopped; that is a warm cache copy, not a cold import
measurement. A target that is open in an interactive Editor is never copied, so
its first stage imports cold.
Existing native catalogs are not migrated or relabeled. Legacy project `Temp`
outputs remain accepted, but Unity clears them on Editor restart. Do not move just
`current.json` when its referenced artifacts still point into Temp. Parent
traversal, arbitrary Library paths and symlink escapes are refused.

For local Streamable HTTP, append
`--transport http --listen 127.0.0.1:8091` and connect to
`http://127.0.0.1:8091/mcp`. `/health` checks server availability.
`--token-file /absolute/token-file` optionally requires a bearer token of at least
32 characters. The server binds a literal loopback IP and checks Origin/Host.
Keep this project control endpoint local. Stopping the server reaps its owned
runner and Player; `session_control(stop)` stops the current runtime session.
An attached interactive Editor is not owned or terminated by the tool.

## Tools and backends

| Tool | Behavior |
| --- | --- |
| `project_inspect` | Project identity, source inventory summary and identity issues. |
| `project_inventory` | Revision-guarded pages of authored asset paths and GUIDs. |
| `project_capabilities` | Supported operations, native component types and configured backends. |
| `asset_read`, `script_read` | Exact UTF-8 pages with metadata and a full source revision. |
| `scene_hierarchy` | Resolved nested/variant prefab hierarchy with provenance; `resolvePrefabs: false` selects the direct authored graph. |
| `scene_edit` | Revision-checked source graph/property changes and supported prefab overrides, or a preview. |
| `asset_create` | Absent `.mat`, `.asset` or `.prefab` from explicit YAML and metadata. |
| `script_create`, `script_edit` | New source/MonoImporter identity or replacement preserving metadata. |
| `source_undo` | Conflict-checked undo of existing-file or staged multi-file changes. |
| `project_prepare` | Our package resolver, source assembly graph, SDK compilation, source/native-resource scene validation and optional baseline activation. |
| `project_build` | **Real Editor build proxy**, with explicit scenes and a fresh owned Linux Mono output. |
| `prefab_edit` | Source instantiate/create/variant/override/apply/revert/unpack, with revision-checked multi-file publication. |
| `component_schema`, `component_add` | Cached native defaults and required companions; attachment uses our source writer and starts no Editor. |
| `session_control` | Start/stop/status/reset/wait/wait_update. `start`/`reset` accept `arguments`: `scene`, `gameArguments`, `contentScenes`, `contentPrefabs`, `addressables`, `hotReload`, `onUnsupportedEdit`, and the inputs of the first preparation `rebuildHost`, `hostPlayer`, `nativeCatalog`, `engineProfile`. Another key is refused, and so is a preparation input of another type than a path (`true` or `false` for `rebuildHost`) with `invalid_request`. With a prepared session a preparation input is refused with `preparation_input_ignored`, which names `project_prepare` with the input, a new `requestId` and `activate: true`, unless the session's preparation was made with that input (the reissue of the start that began it). A start or reset while another session (of another `mcp` server of the project) holds the project's session output is refused with `session_output_in_use`, naming the process. `wait` and `wait_update` fail with `full_recompile_required` while a C# edit that cannot be applied to the running Player is not in it. |
| `hot_reload` | `status` of C# hot reload, and `recompile`: restart the Player on the newest compiled code. See [Hot reload](#hot-reload). |
| `runtime_query` | Public status, hierarchy, inspection, logs, raycast and tool catalog. |
| `runtime_command` | Simulation, input, screenshots and public project tool calls. |
| `project_tools` | Refresh the live catalog and register `project.<name>` MCP tools. |
| `remote_session` | Start/stop/status of a browser page that shows the Player as H.264 video with Opus sound and plays it with keyboard and mouse. |

Resources `unitymicroeditor://project` and `unitymicroeditor://capabilities` expose
project context and supported behavior. These are our tool names, not an assertion
of Coplay wire compatibility. General CustomEditor/EditorWindow execution and an
EditMode runner are outside this integration's requested scope.

## Register project tools with attributes

```csharp
using UnityMicroEditor;
using UnityEngine;

public sealed class SetCounterArgs
{
    [AgentToolArgument(Minimum = 0, Maximum = 100)]
    public int value;
}

public sealed class CounterResult { public int counter; }

public sealed class AgentCounter : MonoBehaviour
{
    public int counter;

    [AgentTool("game.set_counter", Description = "Set this object's counter.")]
    public CounterResult SetCounter(SetCounterArgs args)
    {
        counter = args.value;
        return new CounterResult { counter = counter };
    }
}
```

Only attributed public methods on public project types are discovered. Static
methods need no object target; MonoBehaviour methods require a unique live target,
including inactive objects. `ReadOnly = true` describes the author's contract.
Methods accept no parameter or one public DTO. Writable public fields support
primitive values, finite numbers, enums, arrays/lists, bounded dictionaries and
nested DTOs. Fields are required by default; `AgentToolArgument` supplies optional,
range and string-length constraints. Unknown fields and invalid values are
rejected before DTO construction or invocation. Execution is synchronous on
Unity's main thread. Unsupported/recursive contracts and duplicate tool names are
reported in catalog errors. Private members are never exposed.

After Player readiness, call `project_tools`, then invoke the generated tool:

```json
{
  "name": "project.game.set_counter",
  "arguments": {
    "parameters": {"value": 7},
    "target": {"path": "Counter"},
    "requestId": "set-counter-001",
    "expectedPid": 12345,
    "expectedRevision": "REVISION_FROM_RUNTIME_STATUS"
  }
}
```

A target `path` is the object's path as `runtime_query(hierarchy)` prints it:
the names from the root joined with `/`, without a leading slash (`/` in a name
is written `~1`, `~` is written `~0`). `runtime_query(logs)` returns the newest
log entries the Player keeps, at most 128, each with its stack trace;
`arguments.afterSequence` limits the reply to the entries after that one.

The catalog binds omitted PID/revision guards to the Player generation that
published the schema. Reacquire tools and targets after replacement. Removed or
failed catalogs remove stale MCP tools. Legacy `AgentRuntimeCommands.Register`
remains compatible; attributed/legacy name collisions are rejected. Project
methods that throw or return invalid output may already have changed state; their
receipt records an uncertain outcome. Inspect state before deciding on a new call.

## Make persistent changes and verify them

Read the source first. Its revision hashes authored bytes and metadata. Send
64-bit Unity fileIDs as decimal strings, never JSON numbers. Resolved prefab
objects carry composite identities and source provenance; the editable source
revision differs from the resolved dependency graph revision. Use source fileIDs
for ordinary objects and explicit prefab operations for instance overrides.

```json
{
  "path": "Assets/Scenes/SampleScene.unity",
  "expectedRevision": "REVISION_FROM_ASSET_READ",
  "operationId": "add-marker-001",
  "operations": [
    {"action": "create_gameobject", "key": "marker", "name": "Agent marker"},
    {"action": "set_property", "fileId": "$marker.transform", "property": "m_LocalPosition.x", "value": 2.5}
  ]
}
```

Source operations include create/delete/reparent/set_property/add/remove_component
and supported instantiate/set/revert prefab overrides. Creation keys can reference
`$key`, `$key.transform` and `$key.instance`. Reparent keeps local transform.
Deletion checks references and requires explicit recursive deletion. Native source
attachment needs complete explicit fields or a revision-bound authored template;
`component_add` reads captured native defaults and required companions instead.
Neither path proves successful compilation or runtime loading.

`prefab_edit` computes a source operation plan and publishes changed files with
a multi-file journal and compare-and-swap checks. Native component attachment
remaps the captured owner/companion references in our source writer. Existing
GUIDs remain intact. An external edit or unsupported deletion is rejected before
publication. Keep external source writers
out of the target during transactions; they do not join our lock.

After a changed commit, call `wait_update`. A scene `updated` ACK, a
`code-patched` confirmation, or confirmed `code-updated`/`content-updated`
readiness, proves application rather than behavior.
Read the current PID/revision, inspect values, check logs, and run an assertion.

Every `wait_update` and `wait` first asks the runner to say when it has
answered every source change that was saved before the call, and returns only
after that: several edits may precede one call. The reply is therefore the
outcome of the last save, whether the saves were milliseconds or minutes
apart, and a compile error of the first of two files that the second file
repairs is not reported. A save must have returned before the wait is called:
an edit made while the call runs, for example a `script_edit` sent in parallel
with it, may not be covered. `status` does not ask the runner; right after a
save it shows the state from before it.

Once the runner has answered, works on nothing and a Player is ready, the wait
reports what stands, whatever the caller has read before:

| What stands | The reply |
| --- | --- |
| The Player runs the saved sources | Success, with the session state. |
| A compile error | Failure with the compiler's text. The Player keeps the code it had. |
| A scene or asset the Player or the importer would not take | Failure: `The scene or asset part of the save was rejected: ...`, and, when the C# part of the same save was patched in, that it is applied. |
| An edit that compiled and cannot be patched | Failure with the error code `full_recompile_required` (see [Hot reload](#hot-reload)). |

A rejection stands until an update follows it, so a caller that has read a
compile error and waits again with its sequence is told the same error again,
not sent to a recompile that could not succeed. Nothing but a new save changes
the answer, and no wait is held for one.

`afterSequence` says what is news to the caller: the reply lists the events
after it and reports the notes they brought. Take
`session_control(status).lastSequence` before the first of the edits. Without
the argument the mark is the last sequence that a `wait`, a `wait_update` or a
`hot_reload(recompile)` of this server reported, to any of its callers. That is
right for one caller that waits after each of its edits; callers that share a
server (subagents on one stdio connection, agents on one HTTP server, whose
requests carry no session of their own) take each other's answers that way
and should pass `afterSequence`. The mark is read once per call: the events,
the notes and `attention` of one reply are about the same mark. When nothing
was applied or rejected after the mark, the reply is a success that starts
with `attention`: `Nothing was applied or rejected after afterSequence N: ...`
(`after sequence N, the last one a wait or recompile of this server reported`
without the argument), followed by what stands: the code generation the Player
runs and where it was patched in, or the restart the runner made by itself,
and the first limit of it that a restart lifts. When none stands it names a
lost value (`field_value_reset`) or a callback that threw (`callback_failed`)
in the same words, as a limit that lasts until the Player restarts, although
a restart undoes neither. That is the answer for a
save that changed nothing the runner reads (a path outside `Assets`,
`Packages` and `ProjectSettings`, a file kind it does not watch, a file that is
still open) and for a mark that was read after the answer, also when another
caller's wait read it first. A failed call returns no session state; the
`details` of `full_recompile_required` and of `wait_timeout` carry
`lastSequence`. The events of a server are numbered from 1 when it starts, and
a `start`, a `reset` and a recovery continue the count. An `afterSequence`
above `lastSequence` therefore comes from an earlier server process or another
project: the reply covers every event of this server, as for 0, and
`attention` says so.

`wait` reports an edit exactly as `wait_update` does; it is the call after a
start or a restart, where the Player becoming ready is the news. Both are held
while the session starts, while the runner compiles or marks its answer as not
its last (`updatePending`), and while a recompile is under way. A wait has no
time limit of its own. `timeoutSeconds` (1 to 86400) gives it one; when it is
over the call fails with the error code `wait_timeout`, whose message and
`details` (`phase`, `lastSequence`, `lastUpdateSequence`,
`lastUpdateRejectedSequence`, `updatePending`, `runnerAnswered`,
`afterSequence`) say what the session was doing. Nothing was decided, and the
same call can be made again. A client that gives up on a call cancels it, and
that reply is `cancelled`.

A C# edit is applied to the running Player when possible and keeps game state.
An edit that cannot be applied live fails `wait_update` and `wait` with
`full_recompile_required`; `hot_reload(recompile)` then restarts the Player and
resets game state (see [Hot reload](#hot-reload)). Ordinary compile failures
retain the last confirmed Player and fail both waits with the compiler's text.
The state keeps the first 4 KiB of a rejection, cut at a line; a longer one
ends with the number of bytes left out and the file under the session output
(`rejections/<sequence>.txt`) that holds all of it.

A graph/configuration rejection cannot be applied to the running Player at
all: an added or removed `.asmdef`, a deleted script file, a new `.cs` in an
assembly the prepared graph does not hold, a changed package. The session
state then has `preparationRequired: true` next to `updateError`, and `status`,
`wait_update` and `wait` recover from it automatically. Nothing is done on an
answer the runner marked as not its last (`updatePending`): the rejection may
be taken back by the next one.

- A preparation the project already has for the saved sources is used: the one
  a session started on when an edit that left its graph is undone, the one of
  the edit when it is made again, a branch switched back. What counts is what
  a preparation was made from, not what it is called: its compiler plan must
  accept the sources on disk, which is the check a start of the runner makes
  (every assembly definition, package and project setting as the preparation
  recorded it, no managed configuration in `Assets` outside its graph, and no
  C# source or assembly definition in a package of the project
  (`Packages/<name>`, the folders of its Player assemblies) that the
  preparation does not hold: such a script needs a new preparation; a new
  script under `Assets` goes into an assembly the preparation has, and C#
  sources may differ: they are compiled at the start). The newest eight
  preparations of the scene are looked at. The call that finds one restarts
  the session on it at once: `status` returns the session as it starts,
  without `preparationRequired`, and a wait answers `result_pending` with a
  `message` that says so. The restart is finished also when its caller gives
  up meanwhile.
- Otherwise a fresh source preparation is queued. `status` keeps its normal
  shape and adds `preparationReason` and `preparation`; the waits answer
  `result_pending` with the phase `preparing_configuration` and a `message`.
  The old Player remains available while it compiles, and replacement occurs
  only after successful preparation. A wait that is held while the Player is
  replaced answers `result_pending` too, and so does a `start` or `reset`
  that is sent while a preparation runs: it returns the stage at once instead
  of waiting for the preparation to end. A preparation starts a session that
  ran when it replaces it; a session that was stopped stays stopped, and the
  replies then name `session_control(start)`. When the sources were changed
  again while they were prepared (the edit was taken back), the preparation
  replaces no Player. A save that leaves the graph as it is (C# content, the
  scene) does not: the session starts on the preparation and compiles the
  current sources.

The request id of such a preparation is `config-` and a hash of the scene and
the graph: every managed configuration file, package and setting by its
content, and C# sources and the selected scene by their presence. A receipt is
durable, and its name can be wrong about it: the job reads the sources after
the id was computed, so an edit that is taken back in between is prepared
under the name of the edit. A confirmed receipt is therefore used only when
its preparation fits the sources, and one that does not is passed over for
the next id of the same sources. A receipt whose job failed answers the
sources it was made for, which the recovery records by their content
(`action-jobs/<hash of the id>/sources.txt`): `preparationError` on `status`
and the failure on a wait name `project_prepare` with this scene, a new
`requestId` and `activate: true` to try again, and an edit of the sources
since (the fix of a compile error, also of C# content) is prepared again
under the next id by itself. Preparations are not removed: each is a copy of
the Player (about 250 MB for the sample project) under
`Library/UnityMicroEditor/preparations`, and one that no session uses can be
deleted while no server of the project runs.

A session that could not start because the sources have left the prepared
graph (a `reset` after such an edit ends with the phase `failed` and the
runner's text about a fresh managed graph) is recovered the same way by the
next `status` or wait. When the sources cannot be read for the recovery,
`status` still returns the session state, with `preparationError`, and the
waits fail with the rejection and the calls that are left: `project_prepare`
with the scene and `activate: true`, or `session_control(reset)`.

A reply with the session state lists the newest 8 of the session's events in
`events`, a wait those after its mark (at most 16) and `hot_reload(recompile)`
those of its restart. `eventsOmitted` counts the kept events that are not
listed, and the argument `events` (0 to 64) asks for another number. An event
in a reply says what happened; what stands since is in the state, and the
lists that repeat it are left out of a listed event: the hashes of every
source that a `code-updated` carries, `notes`, `newNotes`, `members` and
`unsupported` (`hotReload.notes`, `hotReload.lastPatch`, `hotReload.newNotes`
and `hotReload.unsupported` have them; the event keeps the counts and the
report), all but the first 512 bytes of an `error`, and all but the first 8
`paths` (each once; `pathsCount` counts them when there are more). The session
keeps its events as the runner printed them. A rejection longer than 4 KiB is
kept whole in `<output>/rejections/<server>-<sequence>.txt`; the file is
written whole under a name of this server process, and the newest 64 are kept.

All source mutations require a stable `operationId`. Exact retry returns its
original durable result; changed arguments with that ID conflict. Stale source or
metadata revisions fail before publication. Undo needs the original operation ID,
a new ID and exact resulting revision(s); it refuses intervening changes or
erasing newly created files. Preview mode proposes ordinary source edits without
publishing them.

Capture native defaults once for the project's exact pinned engine and native
package/settings context:

```bash
/absolute/UnityMicroEditor/unity-micro-editor cache-component-defaults \
  --project /absolute/UnityProject
```

This explicit bootstrap uses an isolated Editor copy. The cache binds Unity
version/revision, installed native modules, supported rendering/UI package trees,
project settings and capture policy. Subsequent schema and attachment requests
report `editorStarted: false`; a missing or changed profile returns
`component_defaults_missing`. Gameplay/IDE package changes do not themselves
invalidate native defaults. Custom Editor `Reset` extensions are outside this
native default contract. ParticleSystem/Renderer companions and Canvas's
RectTransform conversion are retained. An inherited prefab Transform cannot be
replaced by RectTransform without first unpacking that inherited object.

For package and compiler diagnostics, the CLI also provides `resolve-packages`,
`source-graph` and `compiler-source-prepare`. The latter two require an explicit
`--project`, matching `--player`, `--engine-profile` and new owned `--output`.
`engine-profile-capture` consumes an independent public Development graph and the
host's build-time source proof; an old release export or a source graph cannot
stand in for those engine facts. Native package source/configuration changes are
compiled by the SDK when supported. Changes that need new native engine/plugin
facts fail explicitly under `--no-editor`.

Runtime mutations require stable `requestId` plus current PID/revision guards.
The controller persists immutable request and result records across restart. An
accepted ID whose reply is lost or whose Player disappeared reports unknown and
is not executed again in a replacement Player. Poll the same ID; issuing a new
ID authorizes a new execution. This is not a claim of transactional exactly-once
execution across arbitrary crashes. Long project jobs use their own durable IDs;
`result_pending` is not a completed or failed operation.

## Hot reload

A running session applies a saved C# edit to the running Player when it can.
The runner compiles the changed assemblies as before, compares each with the
assembly the Player loaded, and loads a patch with the new method bodies into
the same process. The Player keeps its PID, scene revision, control directory,
loaded scenes, objects, static fields and the `project.<name>` tools. No call is
needed: the watcher does it for every saved `.cs` file, whoever wrote it.

An edit outside the supported set is never approximated. The Player keeps
running the previous code and the caller is told, so that it decides when the
game loses its state.

[HOT_RELOAD.md](HOT_RELOAD.md) is the guide: which edits are applied live and
which are refused, every reason and note code with what to do, the API for
game code, an example exchange, measured timings and the limits. This section
is the contract of the replies.

| The C# edit | `session_control(wait_update)` | Session state |
| --- | --- | --- |
| Applied live | Success; same `pid` and `revision`; the last event is `code-patched` | `hotReload.codeGeneration` is the patch generation, `hotReload.lastPatch` describes the patch |
| Applied live, with a limit that lasts until a restart (a coroutine that was running, a value of an existing object, a call by name of an added method) | Success; the reply starts with `attention` | `hotReload.newNotes` lists every note since the mark, with `newNotesCount` and `newNotesSequence` |
| Applied live, and the runner had to restart the Player for it by itself | Success; the reply starts with `attention`; a new `pid` | `hotReload.lastRestart` |
| No difference in the compiled code, no patch applied | Success | Unchanged |
| No difference in the compiled code while a patch is applied (a comment, a blank line) | Success; the last event is `code-patched` | `hotReload.codeGeneration` advances; the game's reload callbacks do not run |
| Nothing to patch, but not all of the change is in the Player (a method made `public`, a removed `[RuntimeInitializeOnLoadMethod]`) | Success; `attention` for a note that matters (see below) | `hotReload.notes` lists it, with `notesCount` and `notesReport`, until the next compile or restart |
| A scene, prefab or asset that holds a value for a field a patch added | Success; the reply starts with `attention` (`serialized_field_not_restored`) | `hotReload.newNotes` |
| Cannot be applied live | Error `full_recompile_required` | `hotReload.fullRecompileRequired: true` with `unsupported`, `unsupportedCount` and `report`; `updateError` |
| Does not compile | Error `operation_failed` with the compiler's text, as before | `updateError` |
| Needs a new source graph (an `.asmdef` added or removed, a script file deleted) | Pending preparation, as before | `preparationRequired: true` |

`wait_update` and `wait` report success only for a Player that runs the newest
compiled code. While `hotReload.fullRecompileRequired` is true both fail with
`full_recompile_required`, whatever `afterSequence` is: also for a caller
that has read the refusal and passes its sequence, and also when something
was applied since. The message then says what the wait did bring, by what it
was: `an earlier C# edit was applied at sequence N and is in the Player`
(the first of two edits, of which the second was refused), `a scene or asset
change confirmed at sequence N is applied`, or that the Player was restarted
before this edit compiled; `status` shows it as `lastUpdateSequence` and
`revision`. A confirmation that applied nothing is not mentioned. Only a
compile error that nothing followed is reported first, for every mark: no
recompile can succeed until the sources compile.

The `full_recompile_required` reply names the first reason and the call to
make:

```json
{
  "success": false,
  "error": {
    "code": "full_recompile_required",
    "message": "The C# edit compiled but cannot be applied to the running Player: Game.Boss: Type Game.Boss was added or renamed [type_added] (and 2 more). The Player keeps the code it ran before this edit. Call hot_reload with action \"recompile\" to restart the Player on the new code; game state is reset.",
    "details": {
      "unsupported": [
        {"code": "type_added", "member": "Game.Boss", "message": "Type Game.Boss was added or renamed."},
        {"code": "generic_method", "member": "Game.Pool::Take`1()", "message": "Game.Pool::Take`1() is a generic method; its code is shared between instantiations."}
      ],
      "unsupportedCount": 3,
      "report": "ABSOLUTE_PATH_OF_THE_COMPLETE_RESULT",
      "next": {"tool": "hot_reload", "arguments": {"action": "recompile"}},
      "lastSequence": 12
    }
  }
}
```

When the running Player rejected a scene or asset change after
`afterSequence` while the refusal stands, the message names it (`A scene or
asset change saved since was rejected by the running Player: ...`) and
`details.rejected` holds `{"error", "sequence"}` of that rejection. A scene
that only lacks the refused code is applied by the recompile. A scene that is
wrong by itself fails the recompile with the same text, and the Player is
kept. `details.lastSequence` is the session's last sequence, the mark for the
caller's next wait: a failed call returns no session state.

`unsupported` holds at most 16 entries; `unsupportedCount` counts all of them
and `report` names the file with the complete result. Each entry has a
lower_snake_case reason `code`, the `member` and a `message`; the guide lists
[every code](HOT_RELOAD.md#refused). The same list stays in
`session_control(status)` and `hot_reload(status)` as
`hotReload.unsupported` until the code is applied.

A new script is the common case. `script_create` adds a type, so `wait_update`
fails with `full_recompile_required` and the reason `type_added`. A scene edit
that attaches the new component before the recompile is rejected for the
running Player, which does not have the type. `wait_update` and `wait` then
still answer `full_recompile_required` with the same reasons, and the reply
has the scene's own error (`unresolved managed component type ...`) in its
message and in `details.rejected`; `session_control(status).updateError`
holds it too. Both edits can be made before the one recompile: the Player it
starts has the type and loads the scene with the component.

Then call the tool the reply names:

```json
{"name": "hot_reload", "arguments": {"action": "recompile"}}
```

`recompile` checks that the whole source graph compiles, restarts the Player on
the newest compiled code inside the running session, and returns the session
state as `wait_update` does, with a new `pid`. `hotReload.lastRestart` then has
`reason: "requested"`. The call waits for the restart. Game state is reset:
the scene is loaded again, static fields start over, and runtime requests bound
to the old PID report an unknown outcome. The `project.<name>` tools are read
again from the new Player. When the sources do not compile, `recompile` fails
with the compiler's text and the Player keeps running.

The runner names the command in its answer, and only that event ends the call.
A compile error, a failed scene reload or a restart that belongs to a source
edit the runner was still handling is reported to `wait_update` as usual and
leaves the recompile pending.

| `hot_reload` action | Result |
| --- | --- |
| `status` | `enabled`, `onUnsupportedEdit`, `codeGeneration`, `fullRecompileRequired`, `unsupported`, `unsupportedCount`, `report`, `lastPatch`, `notes`, `notesCount`, `notesReport`, `newNotes`, `newNotesCount`, `newNotesSequence`, `lastRestart`, `pendingRecompile`, `updatePending` while it is true, and the session `phase`. Without a session it reports what the next launch uses, with `codeGeneration: 0`. `onUnsupportedEdit` is named also while hot reload is off. |
| `recompile` | The session state after the restart, with the events of the restart. Optional `requestId` identifies the command. |

`lastRestart` is present when hot reload caused the start of this Player
process: `sequence`, `reason` (`requested`, `hot_reload_unsupported` or
`hot_reload_failed`) and, where they apply, `requestId`, `error`, `unsupported`,
`unsupportedCount` and `report`.

`recompile` fails with `session_not_ready` when no Player is ready, and with
`recompile_pending` when another recompile has not been answered; that reply's
`details.requestId` is the pending one. A change that needs a new source graph
is not applied by a recompile either: while such a rejection stands (a new
assembly definition, in `Assets` or in a package of the project), `recompile`
is answered like `wait_update`, with the preparation that answers it
(`result_pending`), and without the service of an `mcp` server with the
rejection and the sentence that `session_control(wait_update)` or `status`
queues the preparation; the Player is not restarted on the old graph.

A recompile that restarts the Player onto what it did not run (an edit it
refused, or a limit of a patch that lasts until a restart, such as a
once-run method) says so in `attention`, and that other processes of the same
game that took the same edits without a restart (the other Players of a
network game, each in a session of its own) still run without it until they
are restarted on the saved code too. A request the client gave up on keeps
running: `hotReload.pendingRecompile` shows it, and
`session_control(wait_update)` returns when the restart is confirmed.

A `requestId` names one command, so repeating it never restarts the Player a
second time. While the command is pending the call waits for it. After the
answer it returns the recorded outcome: the current session state for a
restart, or the compiler's text, marked as already rejected, for a compile
error. Omit `requestId`, or send a new one, to recompile again; this also
applies after the compile error was fixed. The session keeps the outcome of
its last 16 commands, and a new session starts without any.

`recompile` also fails with `full_recompile_required` when it restarted the
Player and an edit that compiled after that restart was refused: the message
says so, and the next `recompile` needs a new `requestId`.

An edit that is applied can have a limit: something the running Player does
not take over until it restarts. The builder says so in notes, each with
`code`, `member` and `message`, and the session shows them in three places:

- `hotReload.newNotes`: in a `status`, the notes that the last change of the
  Player's code brought. In the reply of a wait or a recompile, the notes of
  every answer after the caller's mark, each once, the most important first:
  one wait after two saves reports what the first of them brought as well,
  whether the runner handled them together or one after the other. A note
  that a later answer shows to be gone (the edit was taken back) is dropped.
  `newNotesSequence` is the newest event that brought one. This is what to
  read after an edit.
- `attention`, the first member of that reply, is a sentence about the note
  that matters most, with the number of the others. It claims no more than is
  known: a note names a limit, and whether it matters depends on the game (a
  coroutine that was not running lost nothing). For a limit a restart lifts
  it offers `hot_reload(recompile)` and says when to go on without it. For a
  value that is lost (`field_value_reset`) or a callback of the game that
  threw (`callback_failed`) it offers nothing, since a restart undoes
  neither. For a limit it also says that a process of the same game that
  starts on the saved code meanwhile (another Player of a network game) runs
  all of it, so the two may not agree until this Player restarts too. A limit
  ranks before those two, so the sentence never hides one. Notes about lookups
  by name or reflection alone (`added_method_invisible_by_name`, `accessibility_changed`)
  are listed and make no sentence; an added method that a string of the game
  names (`added_method_named_by_string`) or that the game may call by a name it
  computes (`added_method_computed_name`) does. With more new notes than a list
  holds (16) it names the report that has all. `attention` also says when the
  runner restarted the Player by itself (`hot_reload_failed`, or
  `hot_reload_unsupported` under `onUnsupportedEdit: restart`; for a
  `netcode_*` reason it adds that the other Players of the network game must
  be restarted on the new code too), and when nothing followed the mark, with
  what stands.
- `hotReload.lastPatch.notes`: all notes of the applied patch. A patch holds
  every edit since the Player started, so these and
  `hotReload.lastPatch.members` describe all of them, not the last edit. The
  new notes lead the list, which holds 16; `notesCount` counts all and
  `report` names the builder's result with every one (a file under the
  session output; read it from the file system, the asset tools do not).
- `hotReload.notes`: the notes of a change that needed no patch, absent when
  there are none.

Every note code, what it means and what to do is in
[the guide](HOT_RELOAD.md#patched-with-a-note). Two notes come from the runner
when a scene, prefab or asset edit meets what a patch added:
`serialized_field_not_restored`, when the saved data holds a value for a field
a patch added (the rebuilt object has what the initializer gives, not the
value), and `added_method_named_by_string`, when a UnityEvent (`m_MethodName`)
or an animation event (`functionName`) names a method a patch added (the call
does nothing until a recompile). Both name the file, and both stand, also for
a wait whose mark nothing followed, as long as the patch notes the member. A
value of an added field is a key in a component of the declaring script or of
a script that derives from it (where Unity writes an inherited field), a key
below the field that holds an object of a `[Serializable]` class, or a prefab
instance's override (`propertyPath`) in a scene or prefab variant. An override
of a field the prefab's document has no key for (a field added after the
prefab was saved) is applied as Unity applies it.

`scene_edit` `set_property` writes a field the scene does not hold yet when
the component's script, or a base class of it in a file of its own, declares
it in its class body as one Unity serializes (public or `[SerializeField]`, not
static, const, readonly or `[NonSerialized]`, not a delegate), so the value of
a field a patch added can be given in the scene. The value must fit the
declared type (a number, a boolean, a string, the components of a vector,
quaternion, color or rect, a sequence for an array or a `List`, a mapping or a
number where the type is a reference, a `[Serializable]` class or an enum);
another is refused with `invalid_property_value`. The wait after the save says
that the Player does not read the value before a recompile.

In a game that uses Netcode for GameObjects ([the guide](HOT_RELOAD.md#netcode-for-gameobjects)),
the refusal and the runner's own restart for a `netcode_*` reason say that the
other Players of the game that still run the old code must be restarted on the
new code too, and the reply of a recompile that puts a refused or noted edit
into effect says it in general words. A `recompile` restarts this Player, which
leaves the network session: the other Players see it disconnect, and its
network state starts over.

Launch options, in `arguments` of `session_control(start)` or `reset`:

| Argument | Default | Meaning |
| --- | --- | --- |
| `hotReload` | `true` | `false` turns hot reload off for the session: every C# edit restarts the Player and reports `code-updated`, as before hot reload existed. `hot_reload(recompile)` still works. |
| `onUnsupportedEdit` | `report` | `report` keeps the Player and fails `wait_update` with `full_recompile_required`. `restart` lets the runner restart the Player by itself for such an edit: `wait_update` succeeds with a new `pid`, and `hotReload.lastRestart` has `reason: "hot_reload_unsupported"` and the reasons. |

Like the other launch options, a value that is given stays for the later
launches of this server, so a `start` after a session with `hotReload: false`
has it off until `hotReload` is given again; `null` restores the default. The
session state shows the values in `launch`, and `hot_reload(status)` names
what the next launch uses.

Game code that has to react to a patch marks methods with
`[InvokeOnHotReload]` or `[InvokeOnHotReloadLocal]`
([API](HOT_RELOAD.md#api-for-game-code)).

Limits:

- Hot reload needs a session started by this server; `mcp-standalone` has no
  Player and no `hot_reload` tool.
- A patch changes neither `pid` nor `revision`. Compare `hotReload.codeGeneration`
  to know which code an observation ran on.
- `hotReload.fullRecompileRequired` is cleared by an applied patch
  (`code-patched`), by a restart of the Player, or by an `updated` or
  `unchanged` event, or the rejection of a scene edit, in which the runner
  states `fullRecompileRequired: false`, for example after the refused edit was
  reverted. A scene update without that statement does not clear it, and
  neither wait succeeds until it is cleared.
  To keep the Player and its game state for a while, confirm a scene edit with
  `status`: `lastUpdateSequence` and `revision` advance.
- A compile error after a refused edit leaves `fullRecompileRequired` set:
  the newest code that compiled is still not in the Player. `wait_update`
  reports the compile error while it is the newest outcome.
- A change with a scene part and a refused C# part can show
  `fullRecompileRequired: true` with an empty `unsupported` list for a moment:
  the runner stated it on the scene update and lists the reasons right after.
  `updatePending` is true for that moment and no wait returns in it.
- `updatePending` in `status` follows what the runner has printed. It is true
  while an answer was marked as not the last; it is not true yet for a save
  the runner has not answered at all. Only a wait asks the runner.
- While saves by anyone keep arriving, every answer is marked and a wait is
  held; give it `timeoutSeconds` and repeat it on `wait_timeout`.
- A runner whose command input is closed or full cannot be asked whether it
  has seen the last save, so both waits fail with a message that names
  `session_control(reset)`. One that leaves its commands unread holds the
  waits instead, each until its `timeoutSeconds`.
- When a patch leaves the Player unusable, the runner restarts it by itself on
  the newest code. `hotReload.lastRestart` then has `reason:
  "hot_reload_failed"` and the `error`, game state is lost, and the reply of
  the wait says so in `attention`.
- A wait speaks for saves the runner's watcher sees: files written and closed
  below `Assets`, `Packages` and `ProjectSettings`. A save elsewhere (a local
  package outside the project) is not waited for; the reply then says that
  nothing followed the mark.
- A `start` whose arguments spell out the values the Player already runs with
  (`hotReload: true` on a default launch) returns the running session, like a
  start without arguments. Other values are refused with the call that
  applies them, `session_control(reset)`.
- `recompile` has no time limit of its own; the call ends when the client
  cancels it, and the command stays pending in the runner. A pending command
  ends with the runner's answer or with the session: `session_control(reset)`
  replaces a session whose runner does not answer.
- When the runner no longer reads its commands, `recompile` fails with
  `operation_failed` and a message that names `session_control(reset)`. The
  session stays `ready` on its old code until then.
- Changes of packages, assembly definitions or other compiler configuration
  are not hot reload: they queue a new preparation, as described above. So
  does a deleted script file, unlike an added one, which is a refused edit.
- The limits of hot reload itself (a method that cannot be redirected in a
  Player, inlining off, running coroutines, stack traces of patched code) are
  in [the guide](HOT_RELOAD.md#limits-and-known-problems).

## Game arguments and runtime content

`session_control(start)` and `reset` pass launch options in `arguments`:

```json
{
  "action": "start",
  "arguments": {
    "gameArguments": ["-content-url", "http://127.0.0.1:8080/StandaloneLinux64"],
    "contentScenes": ["0123456789abcdef0123456789abcdef=Assets/Scenes/Level.unity"],
    "contentPrefabs": ["Assets/Prefabs/Enemy.prefab"],
    "addressables": true,
    "hotReload": true,
    "onUnsupportedEdit": "report"
  }
}
```

`gameArguments` is the game's own command line, the same as `-- args` of the
`run` command; `--unity-cli-*` names are refused. `contentScenes` and
`contentPrefabs` are the runner's `--content-scene` and `--content-prefab`. A
list that is given replaces the previous one and stays for later launches; the
session state shows the current values as `launch`. A running Player keeps its
command line: `start` with other values is refused, `reset` relaunches with them.
`addressables: false` turns off the source Addressables described below; it is
on by default. `hotReload` and `onUnsupportedEdit` are described under
[Hot reload](#hot-reload).

### Addressables from sources

A project that uses Addressables gets the counterpart of the Editor's "Use Asset
Database" play mode. The runner reads the Addressables settings and groups and
registers every addressable scene and asset as session content, so
`contentScenes` and `contentPrefabs` are not needed for them. In the Player a
locator resolves their address, GUID (`AssetReference`) and labels to the
authored source. A scene is written as a content scene of its own
([NATIVE_IMPORT.md](NATIVE_IMPORT.md#scenes-in-the-player-format)). Any other
asset (a prefab or model, material, texture, sprite, audio clip, animation,
controller, ScriptableObject, text asset and the like) is written by the scene
worker beside the session's scene: a prefab or model is found by its root, any
other asset by its main object, or by its first object of the requested type,
so a texture asked for as a `Sprite` gives its sprite. Addressables content
therefore needs no host rebuild: an edited asset, a new entry, address or label
takes effect in the next session, and the host's catalog only lets
`Addressables.InitializeAsync` succeed. The game calls
`Addressables` as usual. Nothing is downloaded: `GetDownloadSizeAsync` reports 0
for these keys, and no content build is involved. The watcher treats these
scenes and prefabs like the session's other content.

Limits:

- Code and shaders (`.cs`, `.asmdef`, `.dll`, `.shader`, `.compute`, shader
  graphs and includes) are not served from sources: a session publishes
  shaders on its own. Addressed ones stay with the host's built content, which
  is as old as the host; the runner lists them in an `addressables-skipped`
  event.
- Every addressed asset is written beside the session's scene and loaded with
  it, as the Editor's "Use Asset Database" mode keeps them at hand; a project
  with many addressable assets makes the first write of its scene longer and
  the Player's memory larger.
- An addressable scene is loaded beside an empty scene of the host, because
  the package's scene handle has to come from its own scene provider, and gives
  its roots to that scene once they woke. The scene therefore reports the
  host's name and path (`Bootstrap`), not the authored ones, and the content
  scene's own empty scene stays loaded beside it until it is unloaded. Loading
  the same location twice at once, and `LoadSceneMode.Single`, are not
  verified.
- A prefab is requested as a `GameObject`; a component type, a sub-object key
  (`key[sub]`) and a list of an asset's sub-objects (`IList<Sprite>`) are not
  resolved from sources.
- An asset the worker writes no object for (an editor-only one) is dropped
  with an `addressables-skipped` event, and the session is prepared again
  without it.
- An object of an addressed asset whose script class lives in an Editor
  assembly (URP's `AssetVersion` beside a material) is not given to the game: a
  Player has no class for it. An Editor assembly is told by its name
  (`UnityEditor*`, `Assembly-CSharp-Editor*`, `*.Editor`, `*.Editor.*`), as
  Unity's packages and most projects name them; a runtime assembly named so
  would have its objects left out too.
- A scene that cannot be prepared is dropped with an `addressables-skipped`
  event and stays with the host's content; the others are served. The
  addressed prefabs are written with the session's scene, so a preparation
  that fails while they are part of it drops all of them with one
  `addressables-skipped` event and is tried again without them.
- The host needs built Addressables content for `Addressables.InitializeAsync`
  to succeed, as any Player does. A project that builds Addressables with the
  Player has it. Without it the source keys still resolve, but the game sees a
  failed initialization.
- The keys are read when the session starts. A change of the groups produces an
  `addressables-changed` event and takes effect in the next session. The host
  needs no rebuild for it.
- The bridge is compiled with this support when the host contains
  `Unity.Addressables`. A host built before this support existed has to be
  rebuilt (`project_prepare` with `rebuildHost: true`, a new `requestId` and
  `activate: true`; for the first start of a server, `rebuildHost: true` in the
  `arguments` of `session_control(start)`).

## Simulation, input and observations

`simulation_pause` gates game FixedUpdate/PreUpdate/Update/PreLateUpdate through
public PlayerLoop APIs; bridge polling and rendering continue. `simulation_step`
requests one Update, one fixed tick and one LateUpdate, checks actual counters,
and pauses again before replying. This explicit fixed-tick contract extends the
native Editor step behavior observed in the reference. Legacy `pause`/`resume`
control scaled time only. `wait_frames` observes rendered native frames.
`runtime_query(status).simulation` counts game Update, FixedUpdate and LateUpdate
stages (`gameUpdateFrames`, `gameFixedTicks`, `gameLateFrames`) from the frame in
`countersSinceFrame`, normally Player start. The counters are reinstalled when
the game replaces the PlayerLoop; `countersInstalled: false` with `counterError`
means they could not be.

Keyboard/mouse actions queue events on owned virtual Input System devices and
require the target's active Input System profile and matching host package.
Play mode code ([UNITY_RELEASES.md](UNITY_RELEASES.md#play-mode-code)) also
reads them through the legacy Input, as the Editor's Game view gives its mouse
and keys to that API (`legacyInputInjection: true`): each press and release
takes effect on a frame of its own, the frame after it arrived. Other legacy
Input and OS injection are not advertised. UI dispatch targets public
EventSystem pointer handlers; it does not simulate OS hit-testing. Read current
runtime capabilities before choosing the route.

Runtime hierarchy and source inventory are paged; follow `nextOffset` until null.
Source text pages preserve UTF-8 boundaries and full-revision guards. Tool replies
are bounded to 8 MiB, with an explicit error asking for a smaller page. Snapshots
include inactive objects, known public built-in fields and serialized game fields;
they are not a general reflected Inspector. Logs have bounded history and sequence
markers. Screenshots require a graphics device and return a PNG path, hash,
dimensions and frame. Viewport-dependent UI and advancing counters need aligned
checkpoints for comparisons.

### Change the running scene

`runtime_command(edit)` changes the Player's scene through Unity's public runtime API. Nothing is written to the sources: the change lasts until the scene is loaded again (a source edit, a reload, a restart). `arguments.target` names an object by its `path` as `runtime_query(hierarchy)` lists it, or by a `name` that exactly one object has. Objects have no source file IDs at runtime. `arguments.action` is one of:

- `set`: `rename`, `active`, `layer` (number or name), `tag`, `parent` (a reference or `null` for a root, with `worldPositionStays`), `localPosition`, `localEulerAngles`, `localRotation`, `localScale`, `position`, `eulerAngles`, `rotation`.
- `create`: a new object, empty or a `primitive` (`Cube`, `Sphere`, ...), with `name` and the fields of `set`.
- `destroy`.
- `add_component` (`type`, `values`), `remove_component` (`type`, `index` among the components of that class) and `set_component` (`type`, `index`, `values`).

A `type` is a component class by full name, or by name when one loaded class has it. `values` set fields that are public or `[SerializeField]`, and public writable properties, by name: numbers, booleans, text, enums by name or number, vectors and colors as arrays or as objects (`{x,y,z}`, `{r,g,b,a}`; a rotation of three numbers is Euler angles), and objects of the scene as references (`{"path": ...}`), which give the GameObject or its component of the field's class. Assets cannot be named. The reply is the object's snapshot; `destroy` replies with the path it destroyed.

Verified on 2026-10-20 on the sample in a scene the worker wrote: a door renamed and moved, a light added with intensity and color and removed, an obstacle's size and carving set, a cube created under the door from a world position and destroyed by its path.

## Watch and play in a browser

`remote_session` with `action: start` serves the running game to a browser:

```json
{"name": "remote_session", "arguments": {"action": "start"}}
```

The reply's `url` (`http://127.0.0.1:<port>/<secret>/`) opens a page that shows
the Player's viewport, plays its sound and sends keyboard, mouse, wheel and
touch input back. `status` reports the same object with `playerConnected`,
`viewers` and the `video` and `audio` counters; `stop` closes the listener and every viewer. Stopping the MCP
server stops the session too.

Every session binds its own listener, by default `127.0.0.1` and a free port, so
several editors on one machine each return a different address. The session can
be started before the Player: the page waits, connects when the Player is
ready, and follows resets and Player replacements by itself.

| Argument | Default | Meaning |
| --- | --- | --- |
| `listen` | `127.0.0.1:0` | Literal IP and port of the HTTP listener; port `0` picks a free one. |
| `transport` | `websocket` | How viewers get the video: `websocket` or `webrtc`, see below. |
| `webrtcAddress` | listener's IP | `webrtc`: local IP its UDP socket binds and announces. With a listener on every interface the default is this machine's outbound address. |
| `webrtcPort` | `0` | `webrtc`: UDP port; `0` picks a free one. |
| `publish` | none | Command that publishes the listener, see below. `off` disables the server's default. |
| `publishPattern` | none | Regular expression whose first group is the public address in the command's output. |
| `fps` | `60` | Capture rate, 1-60. The Player's own frame cap (`mcp --fps`, default 30) bounds it. |
| `maxWidth` | `1920` | Widest encoded picture; a wider viewport is scaled down. |
| `bitrateKbps` | `12000` | Ceiling of the video bitrate. Below it the encoder spends what the picture needs at its quality floor, so a still or simple picture takes far less. |

A repeated `start` with the same or no settings returns the running session;
other settings need a `stop` first.

How it works: the Player captures its viewport at the end of each frame with
the public `ScreenCapture` API, reads it back asynchronously and sends the raw
frame over a loopback connection that only it can open (the port and a secret
travel through the control directory). The server encodes H.264 on the GPU
through the NVIDIA driver's encoder (NVENC: ultra-low-latency tuning, no
B-frames, keyframes only on request, the `bitrateKbps` ceiling with a
quantizer floor of QP 23) and hands each frame to the viewers as soon as it is
encoded. Nothing is captured or encoded while nobody watches.

The sound is the Player's final mix: the bridge puts a component on the
object of the scene's active `AudioListener` (and moves it when the listener
changes), which receives the mix in `OnAudioFilterRead` and passes it on
unchanged, so the Player still plays it locally. The samples travel over the
same connection; the server folds them to stereo, brings them to 48 kHz and
encodes Opus (libopus, 96 kbit/s, 20 ms packets). `status.audio` gives the
Player's output rate and channels, the packets and bytes sent and
`lastError`. The component is the bridge's own and does not appear in
`runtime_query` snapshots.

The two transports carry the same stream and the same input:

- `websocket` sends the frames and the sound over the page's WebSocket and the
  page decodes them with WebCodecs. Everything is one HTTP port, which any reverse proxy or
  tunnel forwards. Browsers offer WebCodecs only in a secure context, so the
  page must be opened on `localhost`/`127.0.0.1` or through HTTPS.
- `webrtc` sends the video and the sound as RTP tracks (H.264 and Opus) that
  the browser's own media pipeline plays, so the page also works on plain
  `http://<lan-ip>`. Both travel over UDP to `webrtcAddress`, reported by
  `status`, which the viewer must reach directly: an HTTP proxy or tunnel
  carries the page and the input but not the media. Signalling and input stay on the WebSocket. The server
  announces that single local address and uses no STUN or TURN server, so it
  cannot sit behind address translation.

### Publish the address with a command

`publish` names a command that the session starts after its listener and stops
with it, typically a tunnel client:

```json
{"name": "remote_session", "arguments": {"action": "start", "publish": "zrok share public {url} --headless"}}
```

`{url}` becomes the listener's `http://host:port`, `{address}` its `host:port`,
`{host}` and `{port}` its parts. The command is split into words (quotes group,
a backslash keeps the next character) and run directly, without a shell; use
`sh -c '...'` for pipes. The first http(s) address in its output that does not
point at this machine is taken as the public one, or the first group of
`publishPattern` when the command prints several. The reply's `publicUrl` is
that address with the session's secret path; `publish` holds the command, the
tail of its output and whether it still runs. `start` waits up to 20 seconds
for the address; one announced later appears in `status`. A command that cannot
be started fails the `start`; one that exits or stays silent leaves the session
running locally with `publicUrl: null`.

`mcp --remote-publish '<command>'`, or `UNITY_MICRO_EDITOR_REMOTE_PUBLISH`, sets
the command for every `start` that names none, so agents get a public address
without knowing the tunnel.

Beyond the publish command, making the address reachable from another machine
is the caller's step: an SSH forward, a reverse proxy or a tunnel to the
listener, or `listen` on another interface. The secret in the address is the
only access control and the listener speaks plain HTTP, so terminate TLS in
front of it when it leaves the machine.

Limits:

- Encoding needs an NVIDIA GPU with driver support for NVENC API 12.1 (driver
  530.41 or newer on Linux); `libnvidia-encode.so.1` and `libcuda.so.1` are opened at run
  time. Without them the session starts, the page reports
  `video encoder unavailable`, and `status.lastError` says why. There is no
  software encoder. `UNITY_MICRO_EDITOR_NVENC_DEVICE` selects the GPU by CUDA
  ordinal. The driver limits concurrent encode sessions per machine; each
  remote session with a viewer uses one.
- `websocket` viewing needs a browser with WebCodecs H.264 decoding (current
  Chrome, Edge, Safari, Firefox) in a secure context; a plain `http://<lan-ip>`
  address shows an explanation instead of the picture. `webrtc` viewing needs
  H.264 in the browser's WebRTC stack. The stream is High profile; both
  transports are verified in Chrome only.
- Keyboard and mouse use the same virtual Input System devices as
  `runtime_command(input)`, so they need the target's Input System profile
  (`activeInputHandler` 1 or 2), or Play mode code, which reads them through
  the legacy Input as in the Editor. Any other legacy-Input project is shown
  but cannot be played. Text typed into input fields that read IMGUI events is not delivered.
- Keys are matched by physical position (`KeyboardEvent.code`). The browser
  keeps F11, F12 and whatever the operating system intercepts; Escape also
  releases a captured pointer.
- When the game locks the cursor, the page asks for a click and then sends
  relative motion (pointer lock).
- The picture is the Player window's viewport as rendered; frames arrive a few
  frames after they were drawn (asynchronous GPU readback). Row order is
  verified on OpenGL Core, the verified graphics path.
- A picture that changes all over every frame (camera motion over detailed
  surfaces, noise) needs more than the ceiling at the quality
  floor and takes all of `bitrateKbps`; lower it for a narrow link. The
  ceiling is per frame at the session's `fps`, so a Player that renders fewer
  frames (`mcp --fps`, default 30) uses that share of it.
- Sound needs `libopus.so.0` on the server's machine (opened at run time;
  without it `status.audio.lastError` says so and the video goes on), an
  active `AudioListener` in the scene and a host Player with the audio module.
  A Player whose audio output is disabled sends nothing. Browsers start sound
  only after the viewer clicks or presses a key on the page; the page's Sound
  button turns it off and on. Sound is played as it arrives, a few tens of
  milliseconds ahead, and is not synchronized with the picture beyond that;
  sound that falls a quarter second behind is dropped. With `websocket` it
  needs WebCodecs `AudioDecoder` (verified in Chrome, as the picture).

## Limits of a session

These hold for a session with or without hot reload.

- The scene is written by the import worker from the project's Editor
  compilation and the host Player supplies the engine's resources and the
  render pipeline's global settings ([NATIVE_IMPORT.md](NATIVE_IMPORT.md#scenes-in-the-player-format)).
  A project without an Editor compilation cannot run, and a change of the
  pipeline's global settings is not seen until the host is built again.
- A scene, prefab or asset edit loads the written scene again: the scene's
  state starts over. Only C# edits that hot reload patches keep game state.
- Netcode for GameObjects, found in Players with
  `com.unity.netcode.gameobjects` 2.13.2: with scene management on, which is
  NGO's default, the host's connection approval fails because the session's
  scene has no build-settings path, so clients never synchronize; without it,
  clients do not find in-scene placed NetworkObjects. Spawn network objects
  from prefabs, and turn scene management off, for a session.

## Known differences from the Editor

- Rendering is close but not pixel-identical. Geometry and background match in the reference scene; shading and antialiasing differ slightly.
- In the standalone Player, `Transform.GetSiblingIndex()` returns `0` for root objects. MCP hierarchy reports root order from authored order and `Scene.GetRootGameObjects()`. Game code that depends on the native root index can behave differently. Child sibling order uses the native API.
- Legacy `pause` sets scaled time to zero. It does not suspend the whole PlayerLoop the way Editor Pause does; use `simulation_pause` and `simulation_step` for that.
- A C# change is patched into the running Player when the edit is supported; the Player then keeps its state. Other C# changes need `hot_reload(recompile)`, which restarts the Player and resets game state. There is no domain reload equivalent: static state is either kept by a patch or lost by a restart ([C# hot reload](HOT_RELOAD.md)).
- Fresh native import is bound to verified profiles. Baked lighting and navigation, arbitrary shader and importer settings, complete animation behavior and multiplayer state are not implied by a successful preparation.
- General `CustomEditor`/`EditorWindow` execution and an EditMode test runner are out of scope.

## Compare against the Editor

[Integrations/EditorParity](../Integrations/EditorParity/README.md) sets up an isolated Unity Editor with the Coplay MCP server as a reference and provides `parity-*` subcommand clients that compare its readbacks with ours. Use it when changing authoring, the compiler graph or the runtime bridge. A saved source edit, a successful SDK compilation, a readiness acknowledgement and an observed gameplay assertion are separate results; check each one. The required Rust and SDK checks are in [CONTRIBUTING.md](../CONTRIBUTING.md).
