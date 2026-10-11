# Connect agents to Unity Micro Editor

Unity Micro Editor exposes authored project files and a reusable Mono Player over
MCP. Agents can prepare a project, inspect and edit supported assets, compile C#,
control the simulation, send supported input, and call project tools declared with
C# attributes. Read `project_capabilities` before selecting an operation.

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
Supported native dependencies are prepared in an isolated copy. Scene loading
continues through our source loader; preparation does not bundle authored scenes.

Preparation starts with checks that need no build: authored metadata identities
(the issues `project_inspect` lists: a `.meta` without exactly one GUID,
duplicate GUIDs, symlinks) and the presence of
`ProjectSettings/ProjectSettings.asset`. They fail in the first second with
`invalid_identity` or `project_settings_missing`. A source without `.meta` stops
only a preparation that needs an Editor stage.

Preparation completion and Player readiness are distinct. Reissue the same
`session_control(start)` request while preparation is pending, then call
`session_control(wait)` for actual readiness. A pending reply carries
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
used; when it no longer fits (changed packages or Player settings), the
preparation fails with `host_reuse_failed`, and `rebuildHost: true` with a new
`requestId` builds a new one. `--editor` selects the matching Unity installation;
the default is `UNITY_EDITOR`, then the pinned installation under `$HOME/Unity`.
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
confirmed Player. Public/protected inherited lifecycle callbacks are supported
only for package assemblies whose sources, DLLs and registrations were verified;
private inherited callbacks are refused.

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

Addressables content built by the host build stays with the host. When there is
any, the preparation result lists it in `hostContent`:
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
| `session_control` | Start/stop/status/reset/wait/wait_update. `start`/`reset` accept `arguments`: `scene`, `gameArguments`, `contentScenes`, `contentPrefabs`, `rebuildHost`. |
| `runtime_query` | Public status, hierarchy, inspection, logs, raycast and tool catalog. |
| `runtime_command` | Simulation, input, screenshots and public project tool calls. |
| `project_tools` | Refresh the live catalog and register `project.<name>` MCP tools. |

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
    "target": {"path": "/Counter"},
    "requestId": "set-counter-001",
    "expectedPid": 12345,
    "expectedRevision": "REVISION_FROM_RUNTIME_STATUS"
  }
}
```

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

Before an edit, retain `session_control(status).lastSequence`. After a changed
commit, call `wait_update` with `afterSequence`. A scene `updated` ACK, or confirmed
`code-updated`/`content-updated` readiness, proves application rather than behavior.
Read the current PID/revision, inspect values, check logs, and run an assertion.
C# updates restart the Player and reset game state. Ordinary compile failures
retain the last confirmed Player. A graph/configuration rejection, for example a
new `.cs` in an assembly the prepared graph does not hold, cannot be applied to
the running Player at all. The session state then has `preparationRequired:
true` next to `updateError`, and both `wait_update` and `status` queue a fresh
source preparation automatically (`status` keeps its normal shape and adds
`preparationReason` and `preparation`). The old Player remains available while
it compiles, and replacement occurs only after successful preparation.

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

## Game arguments and runtime content

`session_control(start)` and `reset` pass launch options in `arguments`:

```json
{
  "action": "start",
  "arguments": {
    "gameArguments": ["-content-url", "http://127.0.0.1:8080/StandaloneLinux64"],
    "contentScenes": ["0123456789abcdef0123456789abcdef=Assets/Scenes/Level.unity"],
    "contentPrefabs": ["Assets/Prefabs/Enemy.prefab"],
    "addressables": true
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
on by default.

### Addressables from sources

A project that uses Addressables gets the counterpart of the Editor's "Use Asset
Database" play mode. The runner reads the Addressables settings and groups and
registers every addressable scene and prefab as session content, so
`contentScenes` and `contentPrefabs` are not needed for them. In the Player a
locator resolves their address, GUID (`AssetReference`) and labels to the
authored source, and the raw loader provides the asset. The game calls
`Addressables` as usual. Nothing is downloaded: `GetDownloadSizeAsync` reports 0
for these keys, and no content build is involved. The watcher treats these
scenes and prefabs like the session's other content.

Limits:

- Only scenes and prefabs are served from sources. Other addressable assets
  (materials, textures, audio, ScriptableObjects) stay with the host's built
  content, which is as old as the host; the runner lists them in an
  `addressables-skipped` event.
- An addressable scene is built into an empty scene of the host, because the
  package's scene handle has to come from its own scene provider. The scene
  therefore reports the host's name and path (`Bootstrap`), not the authored
  ones, and has one extra root, `Raw scene imported assets`. Loading the same
  location twice at once, and `LoadSceneMode.Single`, are not verified.
- A prefab is requested as a `GameObject`; a component type or a sub-object key
  (`key[sub]`) is not resolved from sources.
- A scene or prefab the raw loader cannot prepare is dropped with an
  `addressables-skipped` event and stays with the host's content.
- The host needs built Addressables content for `Addressables.InitializeAsync`
  to succeed, as any Player does. A project that builds Addressables with the
  Player has it. Without it the source keys still resolve, but the game sees a
  failed initialization.
- The keys are read when the session starts. A change of the groups produces an
  `addressables-changed` event and takes effect in the next session.
- The bridge is compiled with this support when the host contains
  `Unity.Addressables`. A host built before this support existed has to be
  rebuilt (`rebuildHost: true`).

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
Legacy Input and OS injection are not advertised. UI dispatch targets public
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

## Known differences from the Editor

- Rendering is close but not pixel-identical. Geometry and background match in the reference scene; shading and antialiasing differ slightly.
- In the standalone Player, `Transform.GetSiblingIndex()` returns `0` for root objects. MCP hierarchy reports root order from authored order and `Scene.GetRootGameObjects()`. Game code that depends on the native root index can behave differently. Child sibling order uses the native API.
- Legacy `pause` sets scaled time to zero. It does not suspend the whole PlayerLoop the way Editor Pause does; use `simulation_pause` and `simulation_step` for that.
- A C# change restarts the Player and resets game state. There is no domain reload equivalent.
- Fresh native import is bound to verified profiles. Baked lighting and navigation, arbitrary shader and importer settings, complete animation behavior and multiplayer state are not implied by a successful preparation.
- General `CustomEditor`/`EditorWindow` execution and an EditMode test runner are out of scope.

## Compare against the Editor

[Integrations/EditorParity](../Integrations/EditorParity/README.md) sets up an isolated Unity Editor with the Coplay MCP server as a reference and provides `parity-*` subcommand clients that compare its readbacks with ours. Use it when changing authoring, the compiler graph or the runtime bridge. A saved source edit, a successful SDK compilation, a readiness acknowledgement and an observed gameplay assertion are separate results; check each one. The required Rust and SDK checks are in [CONTRIBUTING.md](../CONTRIBUTING.md).
