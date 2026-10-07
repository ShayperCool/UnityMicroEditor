# Unity Editor and Coplay reference

The [advanced parity fixture](ADVANCED_PARITY.md) covers attributed project tools, game-loop control and actual keyboard, mouse and UI handlers.
The [own preparation comparison](OWN_PREPARATION_PARITY.md) covers independent
Go package/assembly graphs, strict native host facts and source preparation with
`--no-editor`. The [Go prefab comparison](SOURCE_PREFAB_GO_PARITY.md) covers the
current source implementation and cached native defaults.
The [live package comparison](LIVE_PACKAGE_GO_PARITY.md) covers new embedded
packages, GUID asmdef dependencies, automatic Go/SDK configuration recovery and
actual attributed-tool method-body updates without Editor.
The [source authoring comparison](SOURCE_AUTHORING_PARITY.md) covers native
component companions, nested prefabs and variants, apply/revert/unpack,
material and ScriptableObject edits, durable replay and public Editor readback.

This integration prepares an isolated Unity `6000.6.3f1` project and installs the official Coplay MCP package and server at version `10.2.0`, commit `30d22075093d1d35dfb0091c1c7550e9ad948577`. The original `Samples/RawProject` assets remain untouched. Runtime package sources come from this checkout. Coplay's external server uses its own Python environment; the Unity Micro Editor CLI and comparison client use Go.

Build the reusable host before opening the reference Editor. `build-host.sh` rejects another running Editor. Then run:

```bash
./Integrations/EditorParity/start-reference.sh
```

`UNITY_PARITY_EDITOR` selects an explicitly supplied matching Editor installation. Generated Unity files, third-party sources, logs, process identifiers, and evidence live beneath ignored `Temp/AgentParity`. The server listens only at `http://127.0.0.1:8087/mcp`. The script reuses an existing server or its already open project. It does not stop other Editors or publish the endpoint.

The C# helper uses public Unity and Coplay APIs. On first startup it creates `Assets/Scenes/AgentParity.unity`, a cube prefab, an inactive child, a camera, a light, and a lifecycle probe. Existing scenes are reopened without regeneration. The helper connects Coplay through `MCPServiceLocator.Bridge.StartAsync()` and exports initial reference and compiler graph files into the owned project's `Temp/EditorParity`.

`agent_parity_reference` is a registered Coplay custom tool. Its `snapshot` operation uses the same public `AgentRuntimeSnapshot` reader as the Player bridge, including inactive objects, root order, world and local transforms, quaternion rotations, built-in component readbacks, and serialized MonoBehaviour fields. `export_graph` captures the public Unity Player assembly graph and MonoScript GUID/type identities. This graph records real Editor inputs. It is not a fabricated lifecycle proof or a prepared Player baseline.

`scene_proof` captures the exact project, Unity version, authored scene hash, active scene count, dirty/Play Mode state, and public lightmap/light-probe counts. It also records the real lighting-data GUID/local ID using `AssetDatabase.TryGetGUIDAndLocalFileIdentifier`. The initial scene's native built-in `LightingData` has local ID `20201`, zero lightmaps and zero light probes. A scene preparation may use this hash-bound evidence to handle the empty built-in lighting record; it must not assume that arbitrary scenes have empty baked lighting.

The Go client performs a real MCP initialize/initialized handshake, then one request over Streamable HTTP. It supports both JSON and SSE replies. For example:

```bash
go -C Integrations/EditorParity run ./cmd/mcp-call -method tools/list
go -C Integrations/EditorParity run ./cmd/mcp-call -method resources/read \
  -params '{"uri":"mcpforunity://editor/state"}'
go -C Integrations/EditorParity run ./cmd/mcp-call -method tools/call \
  -params '{"name":"agent_parity_reference","arguments":{"action":"snapshot","output":"reference.json"}}'
go -C Integrations/EditorParity run ./cmd/mcp-call -method tools/call \
  -params '{"name":"agent_parity_reference","arguments":{"action":"export_graph","output":"graph.json"}}'
go -C Integrations/EditorParity run ./cmd/mcp-call -method tools/call \
  -params '{"name":"agent_parity_reference","arguments":{"action":"scene_proof","output":"scene-proof.json"}}'
```

The client reports a nonzero exit status for JSON-RPC errors, tool `isError`, and nested operation `success=false`. It prints and saves the complete reply before reporting a failed tool operation so evidence remains available. `-output` saves the reply to a file; use an ignored location. Each invocation closes its MCP session. Unity script changes require `refresh_unity`, waiting for compilation/domain reload, and reading `read_console` before using new types.

Compare authoring snapshots separately from Play Mode snapshots. Entering Play Mode changes lifecycle counters; runtime changes are not persistent authoring edits. Normalize source identities and engine-owned objects, retain component type/order, and compare floating-point numbers with a documented tolerance. Never normalize away unsupported components or null references merely to pass a comparison.

The comparator reads direct shared snapshots, Player control responses, and MCP tool response envelopes. A paginated Player hierarchy must contain all objects before comparison. Run:

```bash
go -C Integrations/EditorParity run ./cmd/compare \
  -reference /absolute/path/to/play-shared.json \
  -actual /absolute/path/to/player-hierarchy-response.json \
  -output /absolute/path/to/Temp/comparison.json
go -C Integrations/EditorParity test -race ./...
go -C Integrations/EditorParity vet ./...
```

It checks object order, root sibling indices, component order/types, inactive states, serialized fields, and lifecycle counts. Numbers use an absolute tolerance of `1e-5`. Quaternion signs may differ because `q` and `-q` represent the same rotation. Object/component `source` identities and Editor-only `globalObjectId` and `prefabInstance` provenance differ between implementations and are ignored. A field named `source` inside game component data remains checked. The reserved `Raw scene imported assets` owner is excluded only when it actually contains `Dungeon.CLIPlay.Raw.RawSceneResources`. Missing components and unresolved references remain mismatches. Exit status `0` means equal, `1` means a behavioral difference, and `2` means invalid/incomplete evidence.

The protocol's root `siblingIndex` means its position in the public `Scene.GetRootGameObjects()` enumeration after excluding the runtime resource owner. Child indices use native `Transform.GetSiblingIndex()`. The pinned standalone Player returned `0` for every native root sibling index even when actual root enumeration retained the authored order. The shared snapshot reports that actual enumeration order consistently in Editor and Player. It does not change `Transform.GetSiblingIndex()` inside game scripts; code depending on that native root API can still behave differently in standalone Unity.

`compare-png` measures rendering separately from serialized state. Capture both sides at the same dimensions first. Coplay's regular camera screenshot follows its current Game View dimensions; a public `Camera.Render()` into a temporary `RenderTexture` can capture a fixed size through Coplay `execute_code`, restoring the previous camera target afterward. The Player uses the actual rendered screen. For example:

```bash
go -C Integrations/EditorParity run ./cmd/compare-png \
  -reference /absolute/path/to/editor-960x540.png \
  -actual /absolute/path/to/player-960x540.png \
  -ignore-bottom 24 -output /absolute/path/to/Temp/render-comparison.json
```

The report always includes full-frame differences and file hashes. `-ignore-bottom` adds a separate viewport metric; the full-frame report retains excluded pixels. In this fixture the bottom rows contain the Player's development-build watermark. The initial viewport's mean absolute channel difference was `0.03256` on a `0..255` scale, with matching geometry and background. Cube shading and antialiasing still differed. This rendering check does not claim identical pixels or broad shader compatibility.

Actual Coplay `10.2.0` reference checks on Unity `6000.6.3f1` have exercised MCP initialization, hierarchy, cold Play Mode, lifecycle logs, camera screenshots, scene/object creation, duplication, deletion, prefab creation, component updates, prefab overrides, and scene save/reload. The initial Play Mode probe logged Awake and Start exactly once and all three callback counters were `1`. A separate `CoplayCrud.unity` holds CRUD evidence, preserving the original AgentParity scene.

The post-baseline scenario creates `AgentParity.CoplayAddedProbe` exclusively through Coplay `create_script`, imports its Unity-generated metadata, attaches it through the working GameObject tool, and saves the scene. The new Go MCP independently creates and attaches `AgentParity.MicroAddedProbe`. The Editor then reloads the authored scene before a normal domain/scene-reload Play Mode run, so its in-memory scene cannot conceal stale external edits. Both new types were present with values `17` and `23`; all their Awake, OnEnable, and Start counters were exactly `1`. Evidence is saved as `play-added.json`. Editor Play options are restored afterward and the reference returns to idle Edit Mode. The Go Player comparison must use a fresh post-update hierarchy rather than an old PID/revision or its original baseline snapshot.

The initial strict comparison found three differences rather than masking them: the Player's generated built-in material name and two native root sibling indices. After fixing the default material name and defining the shared root ordering readback, the baseline comparison had zero differences across five objects. The subsequent comparison after both independently created script types also had zero differences across five objects, including the new components and their real lifecycle counts. The actual Player's post-addition receipt recorded PID `2647630` and revision `175736f0f2bebbb0022dc901f1499937`. Reports are under `Temp/AgentParity/baseline-parity-final.json` and `Temp/AgentParity/added-parity.json`. These executed checks cover the controlled fixture; they do not establish support for all Unity projects, packages, shaders, or Editor extensions.

A second addition ran while the Go Player watch was active. Coplay created and imported `AgentParity.HotAddedProbe`, value `41`, with new Unity GUID `3ee6e6d5891319faea064a15e0a87e44`. The live watch compiled the new source and restarted the Player from PID `2769727` to `2773815` before attachment. The Go MCP then attached the component to the authored root and the same Player acknowledged the scene update. After reloading the saved scene and entering cold Editor Play Mode, the strict comparator again reported zero differences across five objects. All four game probes had each lifecycle counter equal to `1`. Evidence is retained outside the Editor's temporary folder as `Temp/AgentParity/{coplay-play-hot-added.json,play-hot-added.json,player-hot-added.json,hot-added-parity-agent.json}`. The graph kept all 18 existing assembly references and added a sixth script mapping; no dependency was removed to make compilation pass.

The Go MCP also created a sixth object, moved the root from `(10,2,3)` to `(12,2,3)`, and reparented the new child below the inactive `ParityChild` while retaining local position `(0,0,0)`. The native Editor and watched Player both reported the prefab cube at `(13,2,3)` and the nested child at `(12,3,3)` with `activeInHierarchy=false`. A fresh cold Play snapshot compared equal across all six objects, with component order and lifecycle counts still checked. Receipts are `Temp/AgentParity/{coplay-play-crud-reparent.json,player-crud-reparent.json,crud-reparent-parity-agent.json}`. A read-only snapshot requested during the Play Mode domain reload first disconnected; its failed reply was preserved and the read was repeated after Coplay reconnected. Mutations must be verified before retrying after a lost reply.

The failure check appended an explicit `#error` through the Go source tool. The SDK reported C# diagnostic `CS1029` and rejected the update while keeping Player `2773815` and its accepted revision alive. Undo restored the source and cleared the rejection without restarting that Player. The rejected update and recovery are retained in `Temp/AgentParity/{script-rejected-update.json,script-undo-update.json}`. This checks the Go SDK failure path; no separate native Editor failure run is claimed.

After deletion and three undo operations restored the exact scene and metadata, the reference Editor was stopped cleanly and restarted as PID `2806439`. The Go MCP still queried Player `2773815` successfully while the Editor recreated its `Temp` folder; the compiler baseline, accepted runtime and control files were under `Library/UnityMicroEditor`. A new cold Editor snapshot again contained five objects with all lifecycle counters equal to `1`. The latest rebuilt MCP started Player `2816634` and its fresh strict comparison had zero differences (`Temp/AgentParity/final-build-parity.json`). The final Editor screenshot is `Temp/AgentParity/editor-final.png`; `final-render-parity.json` retains full-frame and separately cropped metrics, with the same small shading/antialiasing difference as the initial image. The final Player screenshot hash also matched its earlier post-restart capture.

Three reference limitations were observed and are recorded rather than patched in third-party sources:

- `manage_components` resolves even name/path targets through Coplay's reverse object-ID shim. On Unity `6000.6.3f1`, its reflected `EditorUtility.InstanceIDToObject(int)` throws `NotImplementedException`. Coplay's alternate `manage_gameobject(action="modify", component_properties=...)` successfully updates the same BoxCollider. The saved prefab instance retains trigger, size, scale, and inactive overrides.
- Changing a custom tool's parameter schema during a domain reload does not replace its first registered server definition. Restarting the owned Coplay server after completing the helper schema restored the correct definition; the Editor reconnected automatically.
- A scene edited by the external Go MCP can leave Unity's native scene-reload dialog open during a domain reload. In one run the dialog blocked Coplay's reconnect and its controls did not respond to targeted input. Restarting only the owned reference Editor restored the connection and reopened the saved scene. The original log and dialog capture were retained as `unity-editor-before-hot-recovery.log` and `editor-external-scene-dialog.png`; the Go MCP and Player processes were left untouched.

Screenshot capture uses `manage_camera(action="screenshot")` in this release. Its output folder must stay inside the reference project, for example `Temp/EditorParity/Screenshots`. The C# scene handler still implements an old screenshot action, but the registered MCP `manage_scene` schema does not expose it.

To stop the reference, terminate only the Editor and Coplay processes recorded by this setup. Both are deliberately left running after a comparison so further scenarios can use the same reference environment.
