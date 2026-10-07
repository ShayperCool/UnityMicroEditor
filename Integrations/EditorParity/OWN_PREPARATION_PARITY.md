# Source preparation and package/assembly parity

Executed on 2026-10-07 with pinned Unity `6000.6.3f1` (`45d8eee7de74`),
Linux x64 Mono, Go 1.26 and the installed .NET SDK. Our normal preparation uses
Go package resolution, authored assembly graphs and SDK compilation. The native
host/profile bootstrap and independent public Editor oracle remain separate.

The Rider and Visual Studio packages were inspected locally in the owned
Dungeon package cache. Their `AssemblyNameProvider.cs` implementations call
`CompilationPipeline.GetAssemblies`, `GetAssemblyNameFromScriptPath` and
`ParseResponseFile`; they consume Editor facts. Their folder ownership and
response-file handling informed our implementation. Our Player graph instead
reads `.asmdef`/`.asmref`, nearest-folder ownership, firstpass sources, GUID/name
references, platform/define/version constraints, plugin metadata and compiler
options directly from the resolved roots.

The package resolver's physical roots matched the independent public graph for
all 51 reference packages and all 67 Dungeon packages. Built-in cache copies are
verified against installed engine sources, allowing only Unity's injected
`package.json` fingerprint/formatting. Git cache copies are verified against an
independently fetched exact-commit source tree. An empty-cache registry download
of `com.unity.ai.navigation@2.0.14` from `packages.unity.com` passed, followed by
byte-identical offline replay. Manifest/lock source bytes were preserved; new
resolutions publish an owned snapshot and proposed lock, never an implicit lock
rewrite. Evidence is beneath `Temp/AgentExpansion/package-*.json`.

The old public export mixed release assembly inventory with Development defines.
A fresh public Development export exposed the difference and was retained as a
separate oracle. The final Go/public comparison has zero differences across all
54 Player assemblies: 13 SDK assemblies and 41 native assemblies. It compares
source ownership, defines, dependencies including empty lists, unsafe/noEngine/
override/precompiled settings, plugins, response files and compiler options.
Unity's implicit UI references and managed plugin default/reference semantics
were resolved from this actual comparison. Failed earlier comparisons remain
separate evidence.

The new native host04 was built in the owned oracle shadow. Its public
BuildPipeline report has zero errors, 16 warnings and 122.983 seconds elapsed.
`Player_Data/UnityMicroEditorSourceProfile.json` binds each of the 41 native
assemblies to build-time source/meta/configuration/reference hashes and the
actual emitted DLL. Engine profile capture requires this proof and an explicit
public Linux Mono Development export. A current Editor source graph plus an old
DLL is insufficient evidence. The first host03 hit the validation helper's
12-minute limit during shader compilation and remained unconfirmed; host04 used
the retained shader cache and an increased bound. The closed owned source seed
remained byte-identical; Addressables-generated files were confined to the shadow.

The exact final differential is under the oracle project's owned
`Library/UnityMicroEditor/go-source-tests/TestGoSourceGraphActualComparison-5af0e282c4dfd9d7b9773a8c4e98f908/`.
`comparison.json` is null, denoting no differences. Host receipts are under
`Temp/AgentExpansion/dungeon-development-host-evidence-04`. SDK preparation and
compiler restart with the real 13/41 graph passed independently in 112.55 seconds.
The full actual graph/SDK/restart check subsequently passed under race in
302.54 seconds in the same oracle's
`TestGoSourceGraphActualComparison-c9ad84d59e86ec580e818564cef25d51`
directory, again with zero configuration differences.

Both `dungeon-go-source-01` and the final `dungeon-go-source-02` automatic
preparations completed with `--no-editor` on the owned Dungeon clone. The final
result reports `go_package_resolver`, `go_authored_source`, `dotnet_sdk`,
`raw_source` and `go_source_native_resource`; build remains explicitly
`unity_editor_proxy`. It reused host04 and the verified native catalog.
The result, source graph, compiler, scene proof and artifact inventory live under
`Temp/AgentExpansion/dungeon-project/Library/UnityMicroEditor/preparations/dungeon-go-source-02`.
Its frozen generated-artifact inventory contains 378 files, including exact
assembly configuration descriptors, Player files, scene manifest, native catalog
and content. Corruption/missing-file regressions verify rejection before stopping
an accepted Player. Requested options and resolved seed facts are stored
separately so receipt retries survive changes to the active baseline.

The source scene proof parses the pinned native `unity_builtin_extra` record
for LightingData fileID 20201 and checks exact type-tree/payload hashes. It is
explicitly our source/native-resource evidence. It does not claim Editor opened
the scene. Baked lightmaps, probes and unsupported saved baked dependencies fail
explicitly. Separate public Editor/Coplay readbacks confirmed the empty built-in
lighting identity on the reference scene.

A meaningful SDK-only regression derives a new embedded pure C# package, a new
constrained/version-defined asmdef and a GUID-referenced game MonoBehaviour,
registers the new slots, checks emitted GUID/type/assembly binding, restarts the
compiler and recompiles a changed package method body. It passed under race in
43.54 seconds without any Editor/Player process or `Library/ScriptAssemblies`.
Its synthetic host is labeled as a fixture and is not production Editor evidence.

Native defaults are captured once for 22 supported types. Context hashes include
the settings reference closure and source/meta bytes. The owned Dungeon closure
contains 667 inputs and 12 explicit absent-GUID facts; importing one of those
GUIDs invalidates the previous profile. A failed capture gets a new source-bound
attempt, and the profile/catalog is revalidated inside the publication boundary.
The separate source-only cache restart test passed 17 assertions covering fresh
schema/add, companion attachment, replay, missing/corrupt cache rejection and
source preservation. Prefab/component native readbacks are documented in
[SOURCE_PREFAB_GO_PARITY.md](SOURCE_PREFAB_GO_PARITY.md).

The actual Dungeon MCP session then loaded 12,508 native objects with zero
errors/exceptions and passed all 15 game invariants. A scoped fixture added an
embedded package and a GUID-referencing Assets assembly, using nine new files
inside the owned copy. The old accepted Player remained available during
automatic Go/SDK configuration preparation. The replacement discovered both
attributed tools and returned value 22 through both assemblies. A package method
body change compiled only `AgentParity.LivePackage` in 3.9498 seconds, confirmed
new Player readiness and returned 34. Exact source restoration returned 22 again;
all 15 invariants passed after replacement and after restoration. All 5,711
preexisting authored files remained byte-identical. These are actual runtime
results, separate from the earlier SDK-only fixture. Evidence is under
`Temp/OwnAuthoring/dungeon-live-package`. The package fixture uses a scoped CAS
writer; it does not claim an MCP package-editing API.

An actual fresh RawProject with no Library, supplied host, engine profile or
native catalog completed automatic bootstrap and cached defaults for all 22
types. A new process subsequently prepared it with `--no-editor`, without
changing its Editor log or spawning its target Editor. Native bootstrap now
captures host/profile facts separately from target SDK compilation. A defaults
failure after profile publication retains its warming obligation on retry.
External verified catalogs are snapshotted byte-for-byte into the target cache;
the source identity and digest are retained and all content identities still
pass the normal native/source guards. Bounded runner stderr is now returned
through MCP status/wait when startup fails.

The first finite Player check exposed a valid source material with an omitted
empty `m_Ints` group. Both validation and material construction now treat that
absence as an empty list, retaining strict rejection of malformed present
values. The material source was not rewritten. Runtime bridge source changes
invalidate the previous native host proof; fresh hosts and runtime evidence are
required for this corrected bridge.

That finite check then exposed a missing builtin Shader 46 binding. Exact pinned
engine binary inspection and an independent public Coplay/AssetDatabase readback
both identified it as supported `Standard` at
`0000000000000000f000000000000000/46`. Source material validation and resolution
now permit this exact binding alongside the previously supported Shader 108
`Skybox/Panoramic`, checking retained shader type/name/support. Unknown identities
still fail. Evidence is under `Temp/OwnBootstrap/evidence`.

The host profile cache key now includes the canonical host path and compiled
source-proof hash as well as the executable digest. Unity's identical launch
executable cannot cause a fresh host to select a profile for a previous build.
The regression covers identical launchers at different paths, changed source
proofs, stable retries and missing-proof rejection. Strict profile loading is
unchanged.

The corrected fresh Raw host completed 60 actual Player frames with zero errors,
exceptions or captured logs on NVIDIA OpenGL Core. All 44 authored source files
were preserved. Its report is under
`Temp/OwnBootstrap/raw-project-cold-02/Library/UnityMicroEditor/validation/shader-finite60`.
A fresh process then prepared request `raw-no-editor-06` and completed another
60-frame run, with the same source hashes and unchanged target Editor log.
That report is under the same project's
`Library/UnityMicroEditor/validation/no-editor-finite60`.
The missing/empty/populated material integer-group contract also passed three
positive and five malformed-value rejection checks against the real source file.

Fresh Dungeon host06 rebuilt the corrected runtime in 43.338 seconds with zero
errors and 16 warnings. Its build-time proof is
`a2b49c17f4c2b452f8983c842516718ffd6348a74f6133f9c9106f4967f4ba0a`.
The current profile is the oracle's owned
`Library/UnityMicroEditor/source-profile-06/engine-profile.json`.
Host05 had already passed the complete actual graph/SDK/restart race check in
348.77 seconds with 56 configurations and zero differences. Current target
configuration comparison includes the same 56 assemblies. Routing can be
15 SDK/41 native in the target versus 14/42 in the initialized oracle: generated
package metadata makes the target's validation package use its own SDK assembly.
Configuration comparison retains its complete source/define/reference checks.
The current owned target's host06 comparison is
`Temp/OwnAuthoring/dungeon-profile06-graph-comparison.json`, with differences null
and 15 SDK/41 native assemblies. Its automatic `dungeon-go-source-04` preparation
completed with `--no-editor`, reusing the native catalog and exact host06 profile.
The SDK/declaration suite passed again after the final shader change in
114.085 seconds, and all three Go module race/vet suites passed.

The final actual Dungeon session used host06, profile06 and
`dungeon-go-source-04`, with main Editor execution disabled. It passed 15/15
before and after the final package-body sequence. Both the package tool and its
GUID-linked consumer returned `22 → 34 → 22`; SDK compilation touched only
`AgentParity.LivePackage`, taking 3.083 seconds for the change and 4.245 seconds
for exact restoration. Each update confirmed a replacement Player. Final PID
2872667 retained 12,508 native objects, zero errors/exceptions and paused gameplay
and audio. All 5,711 preexisting authored files remained byte-identical, with
exactly the nine expected validation additions. Current receipts are under
`Temp/OwnAuthoring/dungeon-live-package-final06`, with the detailed sequence in
[LIVE_PACKAGE_GO_PARITY.md](LIVE_PACKAGE_GO_PARITY.md).

These results establish the pinned supported graph/source preparation paths.
Native engine/plugin changes and uncovered content still require new verified
facts; changing a version string or relabeling cached evidence is refused.
Runtime behavior remains a separate check from successful compilation and
preparation. Original Dungeon changes observed from other work during this phase
were retained and never reverted; whole-original equality from the earlier
5,709-file run is historical evidence.
