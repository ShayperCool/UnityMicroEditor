# Dungeon native Editor and raw Player comparison

The actual `ShipGenDemo` comparison passes all 15 public runtime-state checks and all 11 new-script hot-discovery checks. Its three authored roots match native Play Mode structure and script GUIDs with zero differences. The independent full-field comparison retains three process-local `instanceID` differences. Random generated ships are assessed through layout, scenario, player and rendering-health invariants. All 5,709 original authored files remain unchanged.

`DungeonEditorReference.cs` is a validation helper for an owned copy of Dungeon. It uses public Unity Editor and engine APIs. It opens the explicit saved scene, captures authored state, enters native Play Mode, then captures state after 60 actual `PostLateUpdate` ticks and exits its batch Editor. It does not save the scene or rewrite game scripts.

The verified profile is Unity `6000.6.3f1`. The source scene is `Assets/Dungeon/ShipGen/Scenes/ShipGenDemo.unity`. Its three authored roots are `Global Volume`, `Overview Camera`, and `Ship Bootstrap`. The saved Bootstrap specifies random archetype `-1`, ship seed `0`, scenario seed `0`, one player and `generateOnStart: true`. Native game code selects both seeds through `System.Random`. This reference retains those settings.

Compare authored object identities, component order, script GUIDs and transforms, plus the health and public invariants of the generated ship. Different generated meshes, names, room counts or seeds do not establish a loader difference in this scene. Exact generated-world and image comparisons require a separate explicit deterministic fixture.

The helper captures all runtime object paths, component types, MonoScript GUIDs and transforms in `play.json`. It reads ShipRuntime layout/scenario fields and player properties through public reflection only. It checks layout dimensions against the actual public cells array, records generated seed ranges and player attachment, and counts native missing scripts, errors and exceptions. Shared agent snapshots of the authored paths are recorded separately; they retain all fields rather than silently discarding generated IDs or moving values.

Before Play, `shaft-rim-prefab-oracle.json` records public loading of `Art/Custom/VB/R_ShaftRim.fbx` and `Prefabs/VB/VB_R_ShaftRim.prefab`. It includes the complete `LoadAllAssetsAtPath` inventory, model hierarchy identities, `GetPropertyModifications` target resolution, and loaded MeshRenderer material bindings. Prefab and FBX hashes before and after loading show whether these read-only operations changed authored files. The saved material override targeting model renderer `-5824371076177060002` is an explicit regression case.

## Repeat the reference

Use a fresh destination under this repository's ignored `Temp` directory. The Library seed must be a closed owned project. A separate active native-stage project inside its `Library/UnityMicroEditor` directory is excluded from the copy. The script copies only the source project's authored roots and copies reusable Library asset/package download caches. It resolves packages and recompiles assemblies for the new path. It excludes `UnityMicroEditor`, copied package-resolution state, managed assemblies, Bee state, Search cache and process PID files. The first launch stalled during initial Mono/package loading; a retry retaining warm asset data and rebuilding transient state proceeded. The preserved logs do not isolate which copied cache caused that stall.

```bash
Integrations/EditorParity/run-dungeon-reference.sh \
  /mnt/data/projects/Dungeon \
  "$PWD/Temp/AgentExpansion/dungeon-reference-repeat" \
  "$PWD/Temp/AgentExpansion/dungeon-reference-repeat-evidence" \
  "$PWD/Temp/AgentExpansion/dungeon-project" \
  /home/shaypercool/Unity/Hub/Editor/6000.6.3f1/Editor/Unity \
  Assets/Dungeon/ShipGen/Scenes/ShipGenDemo.unity

cd Integrations/EditorParity
go run ./cmd/check-dungeon-reference \
  -reference ../../Temp/AgentExpansion/dungeon-reference-repeat-evidence \
  -output ../../Temp/AgentExpansion/dungeon-reference-repeat-evidence/assertions.json
```

Only the owned copy receives the local runtime package dependency and the Editor-only validation helper. Original Assets, Packages and ProjectSettings receive no writes. The runner compares SHA-256 inventories of every original authored file before and after the run. Reference outputs live outside the Editor project's `Temp` directory, which Unity can delete. The existing Coplay reference Editor and server remain independent of this batch reference.

## Executed reference

Final receipts are under `Temp/AgentExpansion/dungeon-reference-evidence-final`. Unity Editor PID `3785274` used native NVIDIA OpenGL rendering and completed 60 Play ticks. It produced a Cargo Freighter with ship seed `72215`, scenario seed `74260`, layout `13 × 34 × 2`, 884 cells, 28 rooms and one attached player. The scene had 15,509 objects and 12,203 renderers, with zero missing scripts and zero logged errors. The authored scene hash stayed `7fa954d94bd4f5861ce70abac668cd68afe576f7a3ec1c86cecc161c23c10ea8`.

`assertions.json` records 19 checks. Eighteen pass; the strict all-Editor-errors health check fails because native `UnityEditor.Search.SearchDatabase.GetDefaultSearchDatabase` throws once during `SearchInit.IndexationOnStartup`. Regenerating Search cache did not remove this exception. Its full stack remains in `play.json` and the assertion report. `gameInvariantsSuccess: true` distinguishes the successful game checks from this recorded Editor-only startup issue; it does not change the failed strict health result. An unrecorded exception, a game stack or a failed game invariant cannot receive that classification. The checker intentionally returns failure while the strict health check fails.

The original SHA-256 inventories match for all 5,709 authored files. `isolation.json` records that comparison. The owned batch Editor exited after capture. The independent reference Editor and official Coplay server stayed running.

The public ShaftRim oracle has 17 model assets, 14 model hierarchy objects, and three loaded MeshRenderers. Their material bindings are Shell_Amber, VB_Pilaster and VB_RoomDark. `GetPropertyModifications` retains four null targets, including Technical_Dark and Shell_Wall material overrides and two static-editor-flags overrides. The stale renderer identity is absent from the current imported model. Neither the prefab nor the FBX changed during the oracle. This establishes native Editor handling for the source-loader regression without altering the saved overrides.

The actual saved-source `scene_hierarchy` receipt also matches native Edit Mode authoring: three roots and zero differences in root parent, active state, local transforms, component count/order, built-in component types and MonoScript GUIDs. This includes both URP package components. `dungeon-source-native-authoring-parity.json` records this source-only result. It does not establish runtime parity or infer scene root order from the source API's file-ID ordering.

```bash
go run ./cmd/compare-dungeon -source-only \
  -reference ../../Temp/AgentExpansion/dungeon-reference-evidence-final/authoring.json \
  -source ../../Temp/AgentExpansion/dungeon-source-hierarchy.json \
  -output ../../Temp/AgentExpansion/dungeon-source-native-authoring-parity.json
```

## Compare a raw Player

The final MCP session on `127.0.0.1:8094` started Player PID `4060542`, revision `13f6c9f837bb33eae3058d7285e35b44`, on NVIDIA OpenGLCore. Its confirmed startup receipt has zero errors, zero exceptions and 12,508 native imported objects. `dungeon-final-ready.json` preserves that receipt. The already-created validation-only `[AgentTool("dungeon.reference")]` was discovered and executed automatically. All 15 cold-run assertions pass in `dungeon-final-cold-invariants.json`.

While that session was live, `script_create` added a second, genuinely new source and `.meta`: `Assets/DungeonHotMarker.cs`, GUID `e6d721b8ef2ed99c1d6be89024eb78c6`. The SDK compiled only `Assembly-CSharp` in 2.217 seconds, accepted update sequence 4 and replaced the Player with PID `4067757`, revision `0a62228dc787cb351410dc40b5445f74`. The MCP catalog grew from 21 to 22 tools. `project.dungeon.hot_marker` returned its actual `new-guid-sdk-hot-v1` value at frame 897, and `project_tools` describes both validation methods as public static, read-only tools executed on Unity's main thread, with no discovery errors. All 11 assertions in `dungeon-final-hot-discovery-assertions.json` pass. This proves new GUID discovery and execution through the running SDK/watch path; it does not claim code is replaced inside the same Player process.

The final Player public readback records a Mining Ship with ship seed `36436`, scenario seed `28639`, layout `19 × 30 × 2`, 1,140 cells, 37 rooms and one attached player/crew. It has 16,715 live game objects and 13,003 renderers. The VContainer scope and its public Container are initialized. Missing scripts, material slots, shaders and unsupported shaders are all zero. All 15 assertions pass in `dungeon-final-hot-invariants.json`; the complete runtime log window spans sequences 1–13 and has zero errors or exceptions. Bootstrap's saved random settings and both scene and scene-meta bytes are unchanged. The final Player remains alive with its game simulation paused; `dungeon-final-handoff-status.json` records the handoff.

`dungeon-authored-structure-parity-final.json` records zero differences across the three native Play roots: active state, root order, world/local transforms, component types/order and source script GUIDs all match. The full `dungeon-public-fields-parity-final.json` deliberately fails with exactly three differences. They are process-local `instanceID` values in ShipLifetimeScope's `autoInjectGameObjects[0]`, `bootstrap` and `settings` references. No serialized field is discarded to make that report pass.

Three final runtime warnings remain recorded: two deprecations of the public `GraphicsSettings.RegisterRenderPipelineSettings` runtime API and one unsupported `GPUResidentDrawer` BatchBufferTarget platform warning. They are visible in `dungeon-final-hot-logs.json` and `dungeon-player-final-hot.log`. The native Editor's separate Search startup exception also remains a strict reference-health failure. Successful public-state and structure checks do not establish equality of every engine warning or of the randomly generated world.

Material presence and `shader.isSupported` are bounded rendering-health observations, not a visual comparison. This run does not establish animation, movement, combat or multiplayer parity. Generated state is captured at a paused checkpoint; the 15 assertions concern that observed state rather than every later game interaction.

The retained initial `dungeon-player-initial.log` had 136 KINEMATION `Bindable Property: Failed to initialize` warnings and 26 BuiltIn-only `Light.shadowResolution` warnings. The final loader's typed native-ScriptableObject source closure and SRP-aware Light setter removed those two groups; cold and hot final logs both show zero occurrences. An earlier hot-refresh attempt also rejected a valid 1,804,405-byte FBX `.meta`. Streaming imported-asset GUID discovery fixed that case while keeping the strict MonoScript metadata limit. Initial failed receipts remain alongside the final successful proof. The initial runtime log window was truncated (`firstAvailableSequence: 48`); final complete logs supersede that health evidence rather than treating the truncated window as complete.

Collect complete `scene_hierarchy` output with `includeFields: true` and `resolvePrefabs: true`. Collect `runtime_query` inspections of the three authored paths with the actual PID and revision preconditions. Put the three live object results in an `objects` envelope. `compare-dungeon` compares their active state, root order, transforms, component types and order, and the script GUIDs from the authored source. Missing metadata or ambiguous paths fail the comparison.

```bash
go run ./cmd/compare-dungeon \
  -reference ../../Temp/AgentExpansion/dungeon-reference-evidence-final/play.json \
  -actual ../../Temp/AgentExpansion/dungeon-runtime-authored-final.json \
  -source ../../Temp/AgentExpansion/dungeon-source-hierarchy.json \
  -output ../../Temp/AgentExpansion/dungeon-authored-structure-parity-final.json

go run ./cmd/compare \
  -reference ../../Temp/AgentExpansion/dungeon-reference-evidence-final/play-authored-fields.json \
  -actual ../../Temp/AgentExpansion/dungeon-runtime-authored-final.json \
  -output ../../Temp/AgentExpansion/dungeon-public-fields-parity-final.json
```

The second report compares all shared public-field readbacks independently of the structural report. Serialized Unity `instanceID` object references are process-local values; they remain visible in the full diff. Report them explicitly rather than claiming all fields match or discarding serialized game fields. The generated world remains random, and its observed health and layout/scenario invariants require separate receipts.

`DungeonAgentReadback.cs` supplies a bounded validation-only project tool, `[AgentTool("dungeon.reference", ReadOnly = true)]`. It reads strongly typed public ShipRuntime fields, player/crew getters, Bootstrap settings and the LifetimeScope container's initialized state. It never calls `Resolve` or a game factory. It also counts live objects, renderers, missing scripts and material/shader health without returning the entire generated hierarchy. `DungeonHotMarker.cs` provides the separate new-source discovery probe. Both templates belong only in the owned validation copy; the original project and random generation settings stay unchanged. They were actually created through MCP and compiled by the SDK; the final tool catalog and execution receipts establish discovery and runtime behavior.

The ordinary component snapshot bounds serialized game JSON at 65,536 characters and reports `serializedTruncated` for larger payloads. The bounded typed readback avoids relying on a complete serialization of the generated ShipRuntime layout; it does not bypass that snapshot limit or expose private members.

After creating this source through `script_create` in the owned copy and observing a successful SDK refresh, call `project.dungeon.reference` with `parameters: {}`. Capture complete `runtime_query` logs from sequence zero separately. The checker rejects a truncated log window and keeps material/shader failures visible:

```bash
go run ./cmd/check-dungeon-runtime \
  -runtime ../../Temp/AgentExpansion/dungeon-final-hot-readback.json \
  -logs ../../Temp/AgentExpansion/dungeon-final-hot-logs.json \
  -output ../../Temp/AgentExpansion/dungeon-final-hot-invariants.json
```
