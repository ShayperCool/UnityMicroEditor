# Agent integration validation

This implementation was exercised against a running Unity Editor and the official
Coplay MCP server, not just synthetic fixtures. The reference projects are disposable
copies of `Samples/RawProject` and Dungeon. The original Dungeon's SHA-256
inventories matched for all 5,709 authored files before and after its native
reference run.

Environment: Unity `6000.6.3f1` (`45d8eee7de74`), Linux x64 Mono, NVIDIA OpenGL Core,
Go 1.26.0 and Unity's .NET SDK 8.0.318. Coplay reference: `unity-mcp` v10.2.0,
commit `30d22075093d1d35dfb0091c1c7550e9ad948577`. Coplay's external Python server
is confined to this reference environment. Our CLI, coordination and MCP server
run in Go; the Player bridge and compiler helpers use C#.

The subsequent 2026-10-07 [Go prefab validation](Integrations/EditorParity/SOURCE_PREFAB_GO_PARITY.md)
moves these source operations to our backend. With `--no-editor`, the new matrix
matched independent public Editor readbacks for the base, partial-unpack and
complete-unpack states. They respectively compare 2,706, 2,772 and 2,778 represented
properties with zero differences. Defaults for 22 native component types were
captured once, keyed by engine/revision and settings/package context, then reused
without Editor. Two real interoperability bugs were detected and fixed through
the oracle: Unity's required block modification-list form and stripped alias
identities when saving a nested prefab. Historical staged-Editor evidence below
remains separate from this new Go source-authoring result.

The [own preparation comparison](Integrations/EditorParity/OWN_PREPARATION_PARITY.md)
records independent package and assembly graphs, pinned native source proofs,
automatic source/SDK preparation and component cache reuse. The
[live package comparison](Integrations/EditorParity/LIVE_PACKAGE_GO_PARITY.md)
passed 25 checks in the owned Dungeon copy with `--no-editor`. New attributed
tools were discovered in an embedded package and a GUID-referencing Assets
assembly; method-body changes and exact restoration returned `22 → 34 → 22`.
All three Dungeon checkpoints passed 15/15 and all 5,711 preexisting source files
remained byte-identical. The nine scoped fixture files remain in that owned copy.
Fresh automatic RawProject bootstrap also captured all 22 native-default
templates. The corrected host then passed two actual 60-frame runs with zero
errors/exceptions, including a fresh process prepared with `--no-editor`.
Its Editor log and all 44 authored files stayed unchanged during source-only
preparation and execution. The final pinned source configuration comparison
matches all 56 Dungeon assemblies.

[Source authoring validation](Integrations/EditorParity/SOURCE_AUTHORING_PARITY.md)
passed native companion attachment, nested/variant prefab
create/instantiate/override/apply/revert/unpack, material/ScriptableObject edits
and durable replay. An independent Unity Editor readback matched 13 objects,
26 components, two assets, 13 prefab bindings and 2,807 represented properties;
676 public properties remain outside source comparison. All 13 original metadata
files remained byte-identical. This is source authoring evidence; it does not
establish Player behavior or rendering. The separate
[advanced runtime report](Integrations/EditorParity/ADVANCED_PARITY.md) records
attribute, simulation, input and capture assertions.

The advanced fixture passed 26 actual Player checks on the final CLI, including
main-thread attributed dispatch, validation before DTO construction, observable
execution failures, gated callbacks, input handlers, screen/camera capture and
persistent instance discovery. Three further steps each advanced Update,
FixedUpdate and LateUpdate once. Its camera PNG matched the Editor's bytes at
960×540. The strict snapshot retained 18 differences, all from the Overlay's
viewport geometry (Editor 1223×455, Player 960×540); full screens are not claimed
identical. These results do not replace the original scene's shading differences
below.

The final executable repeated this matrix after fresh public Advanced08
preparation and the compiler-locator fix. Actual Player PID `4116864` started
without errors/exceptions. `Temp/AgentExpansion/advanced-check-verified/assertions.json`
passes all 26 checks; repeated-step, byte-identical camera and strict snapshot
receipts use the same `verified` suffix. The paused handoff preserves the
persistent receiver and resets fixture counters for inspection.

A separate real project with no gameplay scripts passed native-only preparation
and 60 Player frames with zero errors/exceptions. An actual release
`project_build` MCP request invoked Unity BuildPipeline with development=false,
completed in 27.403 seconds, and returned the same durable receipt on replay.
Its settings, scene and metadata remained byte-identical. Automatic host builds
now run in isolated staging, with all target authored roots and every host output
fingerprinted; the independent Editor test confirmed source preservation and
cached replay. This isolation prevents Addressables build processors from
deleting generated link.xml metadata in the target during automatic preparation.
The actual isolated Dungeon host also passed its source snapshot check and
fingerprinted 333 output files. Fresh automatic preparation subsequently obtained
its public graph and scene proof, compiled 13 gameplay assemblies and published
a baseline while reusing that verified host and the cooked native catalog.

Dungeon's referenced 71,480,608-byte animation exposed the ordinary text-parser
limit during dependency discovery. The added path streams cooked AnimationClips
only after validating their current source, metadata, exact asset identity and
bound native packs. The real discovery completed in 3.717 seconds with no pending
imports or source changes. Other oversized text assets retain their normal bound.
The final raw Dungeon Player completed 60 frames with zero errors, exceptions,
BindableProperty initialization warnings or URP shadow-resolution warnings.
It used 2,247 prepared assets, 33 restored prefab bindings and 12,508 native
objects. Initial runs exposed missing dependencies through imported
ScriptableObjects; the final preparation follows their verified authored
references while retaining native loading for those objects. The Light loader
now applies Built-In shadow-resolution setters only in that rendering profile.
Failed manifests and the initial 136 binding warnings remain in the evidence.

All three authored roots matched the native Editor's component order/types,
MonoScript GUIDs and transforms. The full field comparison retained three
process-local Unity instance-ID differences. Dungeon's saved zero seeds choose
random worlds, so generated-world geometry and image equality are not claimed.
The native Editor also recorded one SearchDatabase startup exception: its strict
health check remains failed, separately from its successful game invariants.
The [Dungeon report](Integrations/EditorParity/DUNGEON_PARITY.md) records the actual
native and Player results and the attributed-tool checks.

The final Dungeon MCP session passed all 15 game invariants. Creating a second
attributed tool in a new `.cs` file with a new `.meta` GUID triggered SDK
compilation in 2.217 seconds and an owned Player restart. All 11 hot-discovery
checks passed; the catalog now contains 20 built-in tools and two project tools.
The final game had 1,140 cells, 37 rooms and one attached player/crew, with no
missing scripts, material slots, shaders or unsupported shaders. Runtime logs
contain zero errors/exceptions and retain three warnings: two deprecated public
render-pipeline registration calls and one unsupported GPU Resident Drawer
platform configuration. The original Dungeon's 5,709 authored files still match
their pre-run hashes. The final receipts are
`Temp/AgentExpansion/dungeon-final-hot-invariants.json` and
`Temp/AgentExpansion/dungeon-final-hot-discovery-assertions.json`.

## What the comparison establishes

The reference scene contains a transformed root with a serialized probe, an
instantiated cube prefab, an inactive child, a camera and a light. The public
Editor helper exports assembly ownership, MonoScript GUID/type bindings and a
scene-hash-bound empty-lighting proof. SDK compilation prepares a reusable native
host; our loader reads the actual authored scene and prefab source.
During the controlled comparison, source writers took turns. The reference
Editor reloaded our saved scene before each cold Play readback and did not save
its earlier in-memory scene over agent edits. External Editor writers do not
participate in the MCP authoring lock.

`Integrations/EditorParity/cmd/compare` checks all five runtime objects, their
component order/types, active state, transforms, supported built-in fields and
serialized game fields. It accepts quaternion sign equivalence and numerical
error up to `1e-5`. It excludes only source identity provenance, Editor-specific
identity markers and the tool-owned `RawSceneResources` object. It rejects partial
hierarchies, duplicate paths and failed MCP/runtime envelopes.

The original scene and the scene extended with scripts created through Coplay and
our MCP have zero differences. Added components have matching GUID/type bindings,
serialized values and one invocation each of `Awake`, `OnEnable` and `Start`.
A fresh preparation under `Library/UnityMicroEditor` reproduces that result.

A further script was created through Coplay while our watch session was already
running. The watcher discovered its new GUID, compiled it and restarted the
Player. Attaching it through our MCP then succeeded in the same Player, with a
runtime acknowledgement. Fresh cold Editor Play again produced zero differences
and matching `41/1/1/1` value/lifecycle fields.

Our MCP also created a child, moved the root from x=10 to x=12, and reparented the
child under an inactive object. The six-object Editor/Player comparison produced
zero differences, including world/local positions and inherited inactivity.
Deletion and three successive source undos received runtime acknowledgements in
the same Player process. Undo restored the scene/metadata revision exactly; the
five-object comparison again had zero differences.
A clean launch of the final MCP executable also discovered the added script from
the earlier preparation and reproduced the final cold Editor snapshot with zero
differences. The stopped server reaped its owned Player before the next launch.

## Reliability exercised

- Authored source writes preserve `.meta` GUIDs, refuse stale revisions, use an
  operation journal and support conflict-checked undo of existing files.
- Runtime commands carry immutable IDs, payload hashes, PID and source revision.
  Repeating the same pause request returns its original receipt.
- A registered project command changes a serialized value through the public
  bridge. After deliberately removing its response and restoring the value with
  another command, retrying the accepted ID returns `outcome_unknown`; the first
  mutation does not run again.
- Stale process/source guards reject commands. Runtime inspection and a physics
  raycast read back the expected prefab cube and hit position.
- A deliberate C# `#error` produced `update-rejected` while the last confirmed
  Player kept its PID, revision and matching hierarchy. Source undo restored the
  exact valid bytes; an `unchanged` acknowledgement cleared the compiler error
  without restarting that Player.
- The controller checks readiness, response identity, process ownership and child
  PID start identity. Tests cover cancellation, malformed output, symlink escapes,
  orphan cleanup, response loss and concurrent retries.
- Source and metadata reads have UTF-8-safe pagination and full-revision guards.
  Hierarchy reads are paged. Tool and resource frames exceeding 8 MiB fail safely.
- Restarting the real Editor deleted the target project's `Temp`, including an
  earlier Player preparation. Agent preparations and sessions now support the
  strict owned `Library/UnityMicroEditor` subtree. Tests remove `Temp` during a
  session and verify that control still works. Native importer paths retain their
  existing temporary context rules.
  A second real Editor restart confirmed that the Player retained its PID and
  revision, runtime status/registered commands remained available, and the owned
  Library preparation survived.
- A stale native host lacked the new public command API. Rebuilding the host fixed
  real SDK compilation; preparation now checks exported public bridge types before
  instrumentation and reports a missing or stale host explicitly.
- A fresh public preparation can retain an unchanged gameplay DLL while writing
  its accepted compiler descriptor elsewhere in the owned preparation. Restart
  now follows the wrapper's explicit manifest locator after checking its project,
  schema, canonical owned path and file identity. Regression tests reject foreign
  projects, arbitrary cache locations, symlinks and unsupported manifests; source
  and DLL hashes remain mandatory.

## Graphics and API limits

Camera output was compared at 960×540. The initial full-frame comparison had
5,404 changed pixels out of 518,400, with mean absolute channel difference
0.186779 on a 0–255 scale. An explicitly reported exclusion of the bottom 24 rows
containing the development watermark left 4,299 changed pixels, maximum channel
difference 30 and mean difference 0.032556. Geometry and the background agree;
shading/silhouette differences remain. These images are not pixel-identical.

The native Player's root `Transform.GetSiblingIndex()` does not reproduce the
Editor's indices in this profile. MCP hierarchy uses authored root order and
public `Scene.GetRootGameObjects()` enumeration; the gameplay API limitation is
not hidden. Child sibling order uses the native API. `pause` sets scaled time to
zero; it does not suspend the complete Editor PlayerLoop.

The pinned Coplay component-property route calls a removed Unity Editor API and
fails on this Unity version. Its alternate GameObject modification route works.
The reference plugin was not patched to manufacture parity.

This first comparison validates the supported raw scene, source authoring and
runtime-control subset. Broader component/prefab operations, attributed tools,
simulation and input have separate integration reports and must not be inferred
from this original five-object result. It does not establish arbitrary Editor
extensions, baked lighting/navigation, multiplayer state, complete input emulation
or domain reload equivalence. Inventory paging is now available. Assembly/package
configuration changes use fresh Go source preparation with captured native facts;
ordinary C# errors preserve
the running Player.

## Reproduction and evidence

Follow [AGENT_MCP.md](AGENT_MCP.md) for preparation and client configuration and
[Integrations/EditorParity/README.md](Integrations/EditorParity/README.md) for the
actual Editor/Coplay fixture and comparison commands. Required Go race/vet checks
and the SDK checks are listed in [CONTRIBUTING.md](CONTRIBUTING.md).

All three Go modules passed their full race suites and vet checks after the
final compiler-locator, native source-closure and metadata changes. The SDK and declaration suite
passed again with race detection in 46.3 seconds; native mesh,
UI and DontDestroyOnLoad checks passed 26, 29 and 22 assertions respectively.
The standalone C# raw-runtime checks passed 86 assertions. Optional warm-import,
prepared-game and Netcode tests without explicitly supplied profile inputs were
skipped; they are not counted as new integration validation. CLI version,
host build/dry-run, shell syntax and `git diff --check` also passed.

Generated evidence from this machine is under ignored `Temp/AgentParity`:
`baseline-parity-final.json`, `added-parity.json`,
`persistent-baseline-parity.json`, `initial-render-parity.json`,
`hot-added-parity.json`, `crud-reparent-parity-agent.json`,
`crud-undo-parity.json`, `compile-error-parity.json`,
`final-build-parity.json`, `final-build-render-parity.json`,
`player-after-editor-restart.json`, `editor-final.png`, `player-final-build.png`,
`graph-before-hot.json`, `scene-proof-before-hot.json`, runtime receipts and
the Editor/Coplay logs. Preparations and live Player output are under the isolated
target's `Library/UnityMicroEditor`. These outputs are not portable checked-in
fixtures and are not committed to Git.

The 2026-10-07 source-only fixture is available at `http://127.0.0.1:8095/mcp`.
The independent Editor/Coplay reference was restarted at
`http://127.0.0.1:8087/mcp` and successfully returned the saved five-object scene
snapshot through a real MCP call. Earlier 8093/8094 handoff processes are historical
and are not asserted live here. A new final Dungeon host06 session is now at
`http://127.0.0.1:8094/mcp`, using source preparation `dungeon-go-source-04` with
`--no-editor`. Its final package-body sequence returned `22 → 34 → 22`, with
15/15 game checks before and after. The final Player is paused and has zero
errors/exceptions; the nine fixture files remain in the owned copy.
These are local control endpoints, not public
previews. Configure other clients using the stdio or HTTP examples in
`AGENT_MCP.md`; a new project requires its own validated preparation.
