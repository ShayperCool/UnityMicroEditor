# Toward a Player without a host build

Status, 2026-10-16. Three steps are live:

- **Settings.** Preparation and watched sessions write the Player's settings
  managers from the project's `ProjectSettings` (see "In preparation and
  sessions" below).
- **Packages.** A module package adds its engine module assembly to the
  compile references, and a package assembly the host lacks is built as the
  Editor builds it, post-processed by Unity's runner (see "Packages" below).
- **Render pipelines.** A URP project ran on a host built without URP (see
  "A URP project on a foreign host").

A host is still built once per project; this document tracks what it is still
needed for.

## What a host build produces

The Player engine is not built per project. `UnityPlayer.so` of every host is
byte-identical to
`Editor/Data/PlaybackEngines/LinuxStandaloneSupport/Variations/linux64_player_development_mono/UnityPlayer.so`
(checked with `cmp` against the Dungeon host). The executable is a 4.8 KB
launcher that calls `PlayerMain(argc, argv)` and finds its data in
`<executable>_Data`. A host build only produces that data folder. For Dungeon
it holds the following parts:

| Part | Depends on the project | Who can write it |
| --- | --- | --- |
| `MonoBleedingEdge`, `Resources/unity default resources`, engine module DLLs (`Variations/mono/Managed`) | no | copied from the installation |
| `ScriptingAssemblies.json`, `RuntimeInitializeOnLoads.json` | yes | already patched by the tool |
| Game and package DLLs | yes | game DLLs already compiled by the tool; packages see below |
| `globalgamemanagers`: settings managers | yes | **this tool, see below** |
| `globalgamemanagers`: MonoManager, ScriptMapper, BuildSettings, ResourceManager | build-derived | not yet |
| `globalgamemanagers.assets` (2091 MonoScripts, 45 shaders, 10 compute shaders, 31 render pipeline objects, 56 textures) | yes | not yet; shaders are the large part |
| `Resources/unity_builtin_extra` (47 built-in shaders) | per engine | not yet |
| `Plugins/lib_burst_generated.so`, `StreamingAssets/aa` | yes | not yet |

## Settings managers

`playerbake::managers` writes the settings managers of `globalgamemanagers` from
`ProjectSettings/*.asset`:

- **Layouts.** The Player type trees of the 13 classes are embedded under
  `rust/src/playerbake/engine/type-<class>.bin`. They were taken from a file the
  Editor wrote with the public `ContentBuildInterface.WriteSerializedFile` over
  the objects that `GetPlayerObjectIdentifiersInSerializedFile` returns for
  each `ProjectSettings` file, with target `StandaloneLinux64`.
  `WriteGameManagersSerializedFile` is obsolete in 6000.6 and throws.
- **Overlay.** Each authored field is written over a base `globalgamemanagers`
  by name. A field the Player has and the source lacks keeps the base value,
  and so does a source written at another serialized version. Editor-only
  source fields are dropped.
- **Build rules restated.** A Standalone build keeps only the quality levels
  that are not excluded for Standalone, starts on that platform's default
  level, and fills `PlayerSettings.qualitySettingsNames`. A host build runs in
  the background. An authored `targetPixelDensity` of 0 is written as 30; this
  was observed, and the rule behind it is not known.
- **Asset references** (preloaded assets, always-included shaders, render
  pipeline assets, splash images) keep the base value, because they need the
  Player files of their assets.

The hidden command `game-managers --project P --base <globalgamemanagers>
--output F [--compare <globalgamemanagers>]` runs it and prints which fields it
kept and why. `--compare` lists the fields that differ from another file.

### Verified on 2026-10-16

- **Same project, same base.** The project's own settings written over its own
  Editor build reproduce the file byte for byte. This held for
  `HostProject` and for the Dungeon host.
- **Settings changed after the host build.** On a copy of `Samples/RawProject`
  the following were changed after `auto-prepare` had built the host:
  - a tag and a layer;
  - gravity;
  - the fixed timestep;
  - a layer collision pair;
  - two quality levels excluded for Standalone, the Standalone default and a
    level name.

  The tool reused the host without noticing these changes, which is a gap
  today. With the written file in the prepared Player, a probe in the Player
  reported every new value. The control run with the Editor's file showed the
  old ones. A host that the Editor then rebuilt with those settings has the
  same bytes in all 13 settings managers.
- **Settings over another project's base.** With Dungeon's settings written
  over the generic `HostProject` base, these still differ from Dungeon's own
  build:
  - Sources at an older serialized version, which the Editor upgrades on load:
    TagManager 2 → 3, which adds rendering layer names, and DynamicsManager
    23 → 24.
  - Asset references and `GraphicsSettings.m_ShaderDefinesPerShaderCompiler`.
  - `PlayerSettings.AID`.
  - The build-derived managers.

### In preparation and sessions

- **Preparation.** Compiler preparation copies the host Player and keeps the
  host's own `globalgamemanagers` beside it as `UnityMicroEditorHostManagers`.
  It then writes the project's settings over that file. What each manager
  kept of the host is recorded in `settings-managers.json` of the preparation.
- **Watched sessions.** Some settings files are read only by the Player's
  settings managers:
  - `AudioManager`, `ClusterInputManager`, `DynamicsManager`, `InputManager`;
  - `MultiplayerManager`, `NavMeshAreas`, `Physics2DSettings`,
    `PhysicsCoreProjectSettings2D`;
  - `QualitySettings`, `TagManager`, `TimeManager`, `VFXManager`.

  These are no longer part of the compiled graph's configuration. A save of
  one of them makes a new code revision whose Player holds the new managers,
  with no assemblies compiled. The runner restarts the Player on it
  (`code-updated` with `settingsChanged: true`); hot reload is not tried.
  The code revision key includes the managers, so a cached revision never
  brings back old settings.
- **What still needs a fresh preparation.** `ProjectSettings.asset` (defines,
  API level, input handling) and `GraphicsSettings.asset` (the render
  pipeline) stay configuration, as before.
- **Older preparations.** A Player prepared before this change has no
  `UnityMicroEditorHostManagers` and keeps the host's settings.

Verified on 2026-10-16 on the sample copy:

- `auto-prepare --host-player` pointed at the first host, which was built
  with the sample's original settings and without `com.unity.ai.navigation`.
  The Player reported the project's tags, layers, physics, time and quality,
  and the package worked. The host's own files were unchanged.
- A `run --watch` started on that preparation after further settings edits,
  and the first Player had them.
- A save of gravity, a layer name and the fixed timestep while it ran gave
  `code-updated` with `settingsChanged: true`. The revision itself took 0.07 s,
  and the restarted Player reported the new values.
- A method body edited after that was still patched into the same process.

A watched session with a Player window was checked the same way. The
settings edit restarted the Player in 4.6 s from the save to ready, and the
method body edit was patched in 0.32 s.

## Packages

The source graph derives the compile configuration of every assembly,
package assemblies included, from `.asmdef` files. It takes a package assembly
from the host only when the host's build proof matches it.

**Engine module references.** These were frozen at the host build, from the
modules the Editor had enabled then. `package_module_references` adds
`UnityEngine.<Module>Module.dll` for each resolved `com.unity.modules.*`
package. The mapping comes from the platform's `modules.asset`, which lists
the modules a built-in package controls.

**Package assemblies the host lacks** (`packageBuild` in the graph, from
registry, built-in and Git packages) are built as the Editor builds them,
before game code (`compiler/packages.rs`, `build_package_assemblies` in
`compiler/prepare.rs`):

1. Roslyn compiles the package's sources as they are. There is no lifecycle
   rewrite, which a host build never applied to packages either.
2. The packages' IL post-processor assemblies (`*.CodeGen`, such as
   `Unity.Collections.CodeGen`) are compiled as the Editor compiles them:
   - with the Player defines plus `UNITY_EDITOR`;
   - against the Editor's engine and Editor modules;
   - against `Unity.CompilationPipeline.Common`;
   - against the Cecil DLLs of the packages or of `BuiltinILPPs`.
3. Unity's own runner post-processes every package assembly: `Unity.ILPP.Runner`
   with `Unity.ILPP.Trigger`, driven as Bee drives them. It runs those
   post-processors together with the installation's
   `Tools/BuildPipeline/BuiltinILPPs` (Burst), in the runner's own order.
4. The `LifecycleCompiler` helper reads each DLL's
   `[RuntimeInitializeOnLoadMethod]` methods (`--runtime-initializers`). They
   go into the staged Player's `RuntimeInitializeOnLoads.json`, where the
   engine runs them.
5. The DLL goes into `Managed` and `ScriptingAssemblies.json`. From then on the
   preparation treats it as a host assembly: game code compiles against it
   and its scripts are the host's.

Each step is kept for the project under the hash of what it is made from, in
`Library/UnityMicroEditor/package-compiles`, `package-codegen` and
`package-builds`. Embedded and local `file:` packages stay game code, with
the lifecycle rewrite and hot reload.

Verified on 2026-10-16:

- **The runner.** Driven with Bee's own inputs (Dungeon's `Library/Bee`),
  the runner wrote a post-processed `Unity.Collections.dll` equal byte for
  byte to the one Bee wrote.
- **The sample.** On the sample with `com.unity.ai.navigation` and
  `com.unity.mathematics` added, the host no longer proved Collections and
  the render pipeline core. They were built as above, post-processed by
  `JobsILPostProcessor` and `BurstILPostProcessor`. Their
  `RuntimeInitializeOnLoads.json` rows equal those of the Editor-built host
  row for row, including the generated `__JobReflectionRegistrationOutput__…`
  and `$BurstDirectCallInitializer` classes with the same names. The
  navigation package's rows equal those an Editor build of Dungeon has. The
  probes passed, and a method body edit in a watched session was patched.

## A URP project on a foreign host (2026-10-16)

The experiment: a copy of the sample with URP 17.6.0 was prepared with
`--host-player` set to the sample's host, which was built without URP. The
copy had the pipeline assets and settings of `Temp/e2e/asm-merge/RefP`, a URP
`Lit` material on the cube and a probe that renders the camera. It did not
reach a Player. What stood in the way, in order:

1. **Host proofs are exact per assembly.** A package assembly is taken from
   the host only when its sources, references and configuration equal those
   the Editor would use for this project. Enabling one more engine module
   (Terrain here, AI in the sample after the navigation package) or adding a
   package another asmdef names (`com.unity.mathematics`) changed them for
   every package assembly of the host, although the host's DLLs stayed
   valid.
   - Now an assembly the host does not prove is compiled from source instead
     of failing the preparation.
   - An engine module reference the host's build lacks no longer counts as a
     difference.
   - A changed dependency list still does.
2. **Missing asmdef references.** The built-in packages of 6000.6 still name
   `Unity.Mathematics`, which became the engine module
   `UnityEngine.MathematicsModule`. The Editor leaves out a reference to an
   assembly the project lacks. The source graph now does the same for
   registry, built-in and Git packages; code that needs the missing assembly
   fails to compile.
3. **BCL extensions.** The Editor gives every assembly
   `Data/BCLExtensions/TargetingPacks/netstandard2.1/ref`, where
   `System.Runtime.CompilerServices.Unsafe` lives. The engine profile now
   takes them into the base references.
4. **Package code through the lifecycle rewrite.** Compiled from source, a
   package assembly went through the lifecycle rewrite of game code, which
   refuses SRP Core's `DisallowGPUDrivenRendering`. A host build never
   rewrote package assemblies. They are now built as described under
   "Packages".
5. **IL post-processing.** The host's `Unity.Collections` holds code that the
   Burst and job IL post-processors generate:
   `__JobReflectionRegistrationOutput__…` and `$BurstDirectCallInitializer`,
   registered in `RuntimeInitializeOnLoads.json`. Package builds now run
   those post-processors.

With all five resolved, the copy prepared on the sample's host in 88 s. The
eleven assemblies of URP, SRP Core and Shader Graph that the host lacked
were built for it. The session built URP's `Lit` for the project's material
in the import worker, which took 130 s on a cold cache. In the Player the
probe saw:

- `UniversalRenderPipelineAsset` active and `Universal Render Pipeline/Lit`
  supported;
- no magenta pixel, 1,031 pixels of the cube and the centre pixel
  `RGBA(29, 120, 128, 255)`.

The same copy prepared with its own host, which the Editor built with URP,
gave the same probe line and the same three engine errors about URP's probe
volume debug shaders on this GPU.

Even with `--host-player`, preparing a new project ran the Editor in its
staging copy for about 7 minutes. That run exports the engine graph and
imports the native content; the URP shader builds in the import worker use
the Editor's compilation of the project's Editor assemblies.

## Preparing without the Editor (in progress)

With `--host-player`, a cold preparation now runs no Editor:

- **Graph.** The engine graph comes from the host's build proof
  (`agent/host.rs`, `proof_graph`) instead of an `export_graph` run.
- **Native catalog.** The native catalog is the identity catalog of the
  sources. Every native asset is imported in the standalone import worker on
  demand; there is no `native_prepare` run.
- **Component defaults.** These are taken only when the host was built here
  (`host_evidence`).
- **Editor assemblies.** A new stage, `compile_editor_assemblies`
  (`native/editor_compilation.rs`), compiles the assemblies the Editor would
  compile for itself. It uses the Editor platform, defines and references,
  and Unity's analyzers. It lays the result out as the Editor does
  (`Library/ScriptAssemblies`, `Library/PackageCache/<name>@<hash>`, a Bee
  `E-inputdata.json` record) under
  `Library/UnityMicroEditor/editor-compilation`. The import worker runs the
  Editor code of packages from there: shader build callbacks, scripted
  importers and material inspectors. `UME_EDITOR_COMPILATION=0` turns the
  stage off.

What the worker needs from such a compilation:

- package cache directories named as the Editor names them, with the first
  12 characters of `BuiltInPackages/<name>.sha1` for a built-in package;
- Unity's `Unity.Analyzers.Common` and `LongLiving` analyzers;
- sources compiled from the compilation's directory with
  `-pathmap:<dir>=.`, so that the generated `MonoScript` tables record
  `./Library/PackageCache/...` paths.

A shader that names an Editor built-in shader only for `UsePass` (URP's
`TerrainLit` names `Hidden/Nature/Terrain/Utilities`) is left to the worker.
The names come from the Editor's `Data/Resources/unity_builtin_extra`. A
built-in `Fallback` is still refused, because the host Player may not hold
that shader.

The authored closure follows references found in the authored text,
including fields that exist only in the Editor:

- A reference to an asset in an `Editor` folder is not followed.
- A GUID that no asset has is read as missing, as Unity reads it.

**Materials of an older serialization.** URP's global settings name package
materials that are still `serializedVersion: 6`, such as
`Sprite-Unlit-Default.mat`. The Editor upgrades such a material when it
loads it. The upgrade depends on the shader: keywords are split into those
the shader declares and those it does not. The Editor-made catalog delivered
these materials already imported; this route reads authored text, which has
to be version 8.

So the session now asks the importer for such a material. The worker:

1. imports the material's shader and what that shader is made of;
2. loads the material again from its source, as `MaterialInspector` does;
3. saves it as the Editor writes it (the `save` statement of
   `PlayerObjectImporter`).

The session takes those fields while the material, its shader and their
`.meta` files are unchanged.

For a 3D project, a Player build would strip `Renderer2DResources` with
URP's own stripper and never need these materials. That stripping API is
internal.

### Where it stops (2026-10-17)

A cold preparation of the URP copy on the sample's host gets through:

- the package builds;
- the Editor compilation;
- 52 shader builds;
- the two material upgrades its closure needs (about 19 s each).

It then stops at SRP Core's runtime debug UI:
`RenderingDebuggerRuntimeResources` names `RuntimeDebugWindow.uxml`, its
style sheets and its panel settings. A development build keeps these: the
stripper removes them only without diagnostic checks.

UXML, USS and fonts are taken only from the import databases an Editor left
in the project's `Library` (`docs/UI_IMPORTS.md`). A project no Editor opened
has none. The standalone worker reaches UI Toolkit's own importers. Their
`AddObjectToAsset` refuses `VisualTreeAsset` and `StyleSheet` ("Failed to
add object of type `VisualTreeAsset`"), because the worker lacks the engine's
registration of its built-in scripting classes. Linking the Editor's
`unity default resources` into the context does not change that, and no
public API sets it.

Until that gap is closed, a project whose closure holds UI Toolkit assets
needs one Editor run, or a project `Library` an Editor already filled.

## Limits and what remains

- **Settings.** Writing the managers needs a base `globalgamemanagers` of the
  same engine for the build-derived managers. Per-class upgrades of older
  serialized versions are missing: such a manager keeps the host's values,
  and only `settings-managers.json` says so. Binary-serialized settings files
  are not read.
- **Packages.** Post-processor assemblies that name an Editor-only assembly
  of a package are left out with a note. Game code is still post-processed
  only by Netcode's processors, not by every package's. Native plugins, Burst
  AOT output and Addressables catalogs still come from the host. A package
  build is not redone within a watched session: packages nobody edits do not
  change there.
- **The Editor in preparation.** Without `--host-player`, a new project still
  runs the Editor once in its staging copy, to build its host, export the
  engine graph and import native content. With it, preparation runs no
  Editor. It stops at UI Toolkit assets the project never imported in an
  Editor; see above. In that route:
  - The Editor compilation is made once per preparation. It is not redone in
    a watched session.
  - `Unity.Collections.CodeGen` is left out, because it names an Editor
    assembly of the package that the compilation lacks.
- **Render pipelines.** URP's shaders are built per project in the import
  worker with URP's own Editor code, and its runtime assemblies are package
  builds. A host without URP therefore runs a URP project. Shaders of a
  package that only a script finds by name (`Shader.Find`) stay those of the
  host, and a host without the pipeline has none of them.
