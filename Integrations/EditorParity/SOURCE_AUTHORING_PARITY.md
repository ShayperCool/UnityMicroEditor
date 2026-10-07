# Source authoring and public Editor readback

The later [Go prefab authoring comparison](SOURCE_PREFAB_GO_PARITY.md) repeats
the controlled source matrix with `--no-editor`, including added-component
apply/revert and partial/complete unpack. It records the actual native readback
for the source implementation that replaces the staged Editor prefab backend.

Executed on 2026-10-06 with Unity `6000.6.3f1`. The complete source comparison
passed with 13 objects, 26 components, two serialized assets, 13 public prefab
source bindings and 2,807 represented serialized properties. The comparator
reported zero differences and 676 unrepresented public properties.

The target was `Temp/AgentExpansion/source-authoring-project`, an isolated copy
of `Assets`, `Packages` and `ProjectSettings` from the raw-project fixture. All
mutations went through the Go MCP at loopback port 8094. No Player ran in this
scenario. Editor actions used separate staged projects and published their
explicit source changes through revision checks and the multi-file journal.
The independent readback opened a second copy of the final saved source. It
used public `EditorSceneManager`, `PrefabUtility`, `AssetDatabase`,
`GlobalObjectId` and `SerializedObject` APIs, without saving the scene.

| Operation | Observed result | Receipt beneath `Temp/AgentExpansion/source-authoring` |
| --- | --- | --- |
| Native BoxCollider defaults and attachment | Class 65; trigger enabled and size `3/1/1` | `box-schema-restarted.json`, `box-add-final.json` |
| ParticleSystem attachment | Both ParticleSystem 198 and ParticleSystemRenderer 199 attached to the same owner | `particles-add.json` |
| Canvas attachment | Canvas 223 and RectTransform 224; replacing Transform preserved fileID `295488897484103` | `canvas-add.json` |
| Base and nested prefab creation | New GUIDs; nested child retained its base binding | `base-prefab.json`, `nested-instantiate.json`, `nested-create.json` |
| Variant creation and instantiation | Nested variant resolved through all three source identity levels | `variant-create.json`, `variant-instantiate.json` |
| Nested component override and apply | Size changed to `3/7/1`; apply published scene and variant together, preserving both GUIDs | `variant-override.json`, `variant-apply.json` |
| Second override and revert | Temporary `3/7/9` returned to applied `3/7/1` | `variant-second-override.json`, `variant-revert.json` |
| Complete unpack | Ordinary scene objects retained size `3/7/1`; a second linked variant also resolved to `3/7/1` | `variant-unpack.json`, `variant-linked.json` |
| ScriptableObject fields | Revision `11` and tint red `0.4`, with original GUID/fileID and other fields preserved | `tuning-edit.json` |
| Material fields | Color green `0.6` and main-texture offset x `0.25`; shader and texture references preserved | `material-edit.json` |
| Durable operation replay | After MCP restart, identical BoxCollider arguments returned the original confirmed receipt; changed arguments with the same ID returned `request_id_conflict` | `box-replay-after-restart.json`, `box-conflict-after-restart.json` |

All 13 original metadata files match the first isolated stage byte for byte.
The complete `Assets`, `Packages` and `ProjectSettings` hash lists were identical
before and after the replay/conflict checks. Evidence is in `integrity.json` and
`source-hashes-{before,after}-replay.txt`. The final expanded hierarchy has zero
unresolved prefab instances. The raw view reports unresolved root IDs explicitly
instead of returning a fictitious root object `0`.

The final report is
[source-parity.json](../../Temp/AgentExpansion/source-authoring/source-parity.json).
Its inputs are `final-hierarchy.json`, `asset-sources.json` and
`editor-readback.json`. These generated files and Editor logs remain outside Git.

The run found and fixed four implementation issues: an empty startup scene
rejected component schema capture, copied staging markers retained the previous
source project, native SceneRoots referenced PrefabInstance rather than stripped
Transform, and the MCP schema treated raw override JSON as an array. Registration
now also preserves 64-bit integer argument and reply tokens. A transient variant
attempt compiled the runtime package while screenshot functions were being
edited and failed before publication. Known failed jobs remained failed; retries
used new IDs after the fixes. Failed replies and original logs remain available.

To repeat the independent readback, copy only the final project's three source
roots into a new isolated reference project. Install
[SourceAuthoringReference.cs.txt](SourceAuthoringReference.cs.txt) as
`Assets/Editor/SourceAuthoringReference.cs` in that reference copy. Run the pinned
Editor in batch mode with these arguments:

```text
-batchmode -nographics -quit -buildTarget Linux64
-projectPath /absolute/path/to/reference-copy
-executeMethod AgentParity.Editor.SourceAuthoringReference.Capture
-sourceAuthoringScene Assets/Scenes/RawSample.unity
-sourceAuthoringAssets Assets/Data/Tuning.asset,Assets/Materials/Checker.mat
-sourceAuthoringOutput /absolute/path/to/ignored/editor-readback.json
-logFile /absolute/path/to/ignored/editor-reference.log
```

Read the entire MCP hierarchy with `includeFields: true`; all pages must be
joined if needed. Read each compared asset with `asset_read` and collect its
complete `structuredContent.data` into a JSON array. Then run:

```bash
go -C Integrations/EditorParity run ./cmd/compare-source \
  -source /absolute/path/to/final-hierarchy.json \
  -assets /absolute/path/to/asset-sources.json \
  -editor /absolute/path/to/editor-readback.json \
  -output /absolute/path/to/source-parity.json
```

The comparator checks complete object paths and parents, active states,
component ownership/order, native component types, ordinary fileIDs, public
prefab source GUID/fileID against resolved provenance, asset GUID/fileID and
represented public serialized properties. Integers compare exactly; floats use
relative tolerance `1e-6` with an absolute floor of `1e-6`. Empty YAML string
slots match public empty strings. Public Material key/value pair paths map to
Unity's single-key YAML entries.

The 676 unrepresented properties include fields absent from source YAML and
references whose expanded composite IDs differ from native Editor scene IDs.
They are counted, not silently treated as equal. Public prefab binding checks
still verify the corresponding object and component source identities. This
scenario proves source authoring on the controlled fixture; it does not prove
ParticleSystem rendering, complete Canvas behavior, broad shader support or
runtime equivalence for arbitrary projects. Runtime and Coplay comparisons are
recorded separately in [README.md](README.md) and
[ADVANCED_PARITY.md](ADVANCED_PARITY.md).

An independent Dungeon Editor check on 2026-10-06 additionally verified the
removed-model override case in `VB_R_ShaftRim.prefab`. Public model traversal
returned 14 objects, `LoadAllAssetsAtPath` returned 17 imported identities, and
`LoadPrefabContents` returned three current MeshRenderers. The saved target
`5efaa903c18e73d47a9f3433b9e4efd7/-5824371076177060002` was absent.
`PrefabUtility.GetPropertyModifications` returned four null targets: two
material overrides and two `m_StaticEditorFlags` overrides. Current renderer
material overrides still had valid targets. Both FBX and prefab source hashes
were unchanged before and after the public readback.

The raw source loader now accepts this case only with complete, hash-bound
current native model evidence and records every ignored override in
`ignoredPrefabOverrideDiagnostics`. Reduced nested-variant regression tests
retain the exact signed IDs, preserve live transform overrides and verify
source bytes, repeated diagnostics, incomplete inventories, existing native
resource targets and changed source/meta rejection. The public Editor receipt
is `Temp/AgentExpansion/dungeon-reference-evidence/shaft-rim-prefab-oracle.json`.
This check establishes stale override behavior; a complete Dungeon preparation
and runtime comparison remain separate validation steps.

The matching read-only raw graph check expanded the actual saved prefab to 16
records, matched all three live renderer source identities and their three
material GUID/fileID references, and recorded exactly four ignored overrides
for the four public null targets. It reported zero differences and unchanged
FBX, prefab and both `.meta` files. Its receipt is
`Temp/AgentExpansion/source-authoring/shaft-rim-source-parity.json`; the exact
expanded graph is `shaft-rim-authored-graph.json` in the same directory.

The subsequent read-only native dependency check of the whole Dungeon authored
closure passed in 3.717 seconds with the existing verified catalog. It streamed
the 71,480,608-byte `Unarmed_Overlay.anim`, confirmed exact source and `.meta`
hashes plus its cooked registry pack, and reported no pending imports. It started
no Editor, cooking job or Player. The receipt is
`Temp/AgentExpansion/source-authoring/dungeon-native-discovery.json`. This proves
the bounded discovery step, not successful runtime execution.

In the first deferred native prefab-reference check, a read-only preparation of the
same Dungeon baseline completed in 5.049 seconds. It published 2,151 assets and
three active bindings for `FPS_StopStep`, `FPS_TurnStep` and `VContainerSettings`.
Every owner was imported and each target was present in the actual raw prefab
graph, including RootLifetimeScope component `8107781485188562762`. The other
30 catalog bindings were omitted because their owners were absent. The previous
baseline and both target prefabs and `.meta` files stayed unchanged. The receipt
is `Temp/AgentExpansion/source-authoring/dungeon-imported-binding-closure.json`.

The subsequent finite Player run completed 60 of 60 frames with zero errors and
exceptions. Public VContainer instantiation of `ShipPlayer`, `StartSortie` and
ship lighting completed using that prepared source closure. Its receipt is
`Temp/AgentExpansion/dungeon-project/Library/UnityMicroEditor/DungeonVerified/finite-auto07-bindings/runs/18dc168f369c854c-3ca67f/player-report.json`.
No Editor recooking or authored source change was needed. This initial run still
reported 136 BindableProperty initialization warnings: the imported native
ScriptableObjects' authored references had not yet been traversed.

The final preparation follows hash-verified authored references from imported
ScriptableObjects while preserving their native loading. It traverses references
to other catalogued ScriptableObjects and raw prefabs; opaque cooked references
remain in the native packs. This closes the source dependency graph without
interpreting stale Editor shader references as live raw dependencies. The final
manifest contains 2,247 assets and all 33 applicable prefab bindings. Its finite
Player run completed 60 of 60 frames with zero errors, exceptions or
BindableProperty initialization warnings. The rendering-profile correction also
removed all 26 previous URP shadow-resolution warnings. The final receipt is
`Temp/AgentExpansion/dungeon-project/Library/UnityMicroEditor/DungeonVerified/finite-auto07-typed-closure/runs/18dc1724205ddb19-3db32d/player-report.json`.
The subsequent MCP game and hot-compilation checks are recorded in
[DUNGEON_PARITY.md](DUNGEON_PARITY.md).

The SDK GUID index was checked read-only on the isolated Dungeon clone after a
new script exposed a former 1 MiB limit for unrelated FBX metadata. Its bounded
streaming reader indexed all 2,976 asset metadata files, including three larger
than 1 MiB, reading 38,509,006 bytes in 0.26 seconds. The reported 1,804,405-byte
`Rifle_Down_Idle_Humanoid.fbx.meta` retained GUID
`97697180a8c267a40bec25998434ec75` and SHA-256
`a52aba85ad4d50456e23b4cd0198acac40ce1a9887f4de8ef84027da52a75590`.
The index uses a reused 64 KiB buffer and a 32 MiB per-file limit; it hashes the
complete file and checks file identity before and after reading. Duplicate GUIDs,
invalid identities, symlinks and oversized files fail closed. Full parsing of
MonoScript and compiler configuration metadata keeps its existing limit. This
check proves asset GUID discovery. Its receipt is
`Temp/AgentExpansion/source-authoring/dungeon-metadata-guid-index.json`.
The subsequent live `script_create` created a separate attributed tool and a new
`.meta` GUID. SDK compilation completed in 2.217 seconds, the owned Player
restarted, and the new tool returned its expected marker. All 11 hot-discovery
checks and 15 final game invariants passed; the original Dungeon's 5,709 authored
files remained byte-identical. See the Dungeon report for those independent
runtime receipts and the remaining three platform/API warnings.
