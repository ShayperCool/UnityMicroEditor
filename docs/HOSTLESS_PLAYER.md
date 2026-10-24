# Toward a Player without a host build

## A host assembled without the Editor (2026-10-24, 6000.0)

A preparation of a 6000.0.71f1 project no longer builds its host with the
Editor: it assembles one from the installation (`native::host_assembly`,
stage `assemble_host`), so preparing a project needs no Unity licence.
`UME_EDITOR_HOST=1` keeps the Editor build; `--host-player` still takes a
given host. `unity-micro-editor assemble-host <project> <editor> <output>`
assembles one alone, and `complete-player <project> <editor> <player>`
writes what a preparation adds to it.

What the host is made of:

- **Engine.** The `linux64_player_development_mono` template (launcher,
  `UnityPlayer.so`, `libdecor`, Mono, the engine's modules and default
  resources), copied, and the class library a Mono build copies (20 assemblies
  of `unityjit-linux` and 4 facades, with what the engine's modules refer to).
- **Registrations.** `ScriptingAssemblies.json` lists the engine's modules
  (`UnityEngine.dll` first, the modules by name without `Module`) and the
  tool's bridge, compiled against them; `RuntimeInitializeOnLoads.json`
  starts empty. A preparation registers what it compiles, as before, and
  compiles the bridge again after the packages it builds, so that the bridge
  serves Addressables where a package build brought them.
- **Build proof.** `UnityMicroEditorSourceProfile.json` holds the bridge
  alone, with the symbols and references every Player assembly of the
  project compiles with, which the engine profile is read from: 80 symbols of
  a 6000.0 Linux Mono development Player (an Editor build of Bouncemasters'
  bridge less the project's own symbols), the release's version symbols, the
  API level's (.NET Framework only) and the input handler's; the 17 class
  library assemblies of `unity-4.8-api` with all its facades, and the engine
  modules no built-in package controls with those of the module packages the
  project resolves. Every package assembly is then built from source.
- **Plugins.** The native libraries of the project and its packages whose
  importer enables them for the Linux Player, by the rules of managed plugins
  (both forms of `.meta`), into `Plugins`; the managed ones into `Managed`,
  registered as the build's assemblies. A preparation then keeps them: Odin
  Inspector ships a Player build of its serializer (`NoEditor`) beside the
  Editor's, and the Editor's, which a Player of Play mode code otherwise
  links, throws while `Instantiate` serializes a dictionary
  (`Was not in array when exiting array`), which left Bouncemasters' pool
  manager without its pools.
- **Streaming assets.** `Assets/StreamingAssets`, and an Addressables runtime
  with an empty catalog where the project has the package: the session serves
  addressable assets from their sources, and the package only has to
  initialize. Addressables 2 reads only a binary catalog unless the project
  defines `ENABLE_JSON_CATALOG`, so `aa/catalog.bin` is written in its
  `BinaryStorageBuffer` form: a header stamped with the Player's Mono hash of
  `"ContentCatalogData"` (the 64-bit form of the .NET Framework's
  `String.GetHashCode`, 233015618) and version 2, no keys, the instance and
  scene providers. `aa/settings.json` names the build target the code is
  compiled for (`Android` with `--compile-for Android`), which Addressables
  checks the data against.
- **`globalgamemanagers`** (`playerbake::write_game_managers`), from the
  release's manager pack and the project's `ProjectSettings`.
- **`Resources/unity_builtin_extra`**: the built-in shaders the build needs,
  baked by the tool from the Editor's `unity_builtin_extra`, at their path
  IDs there.
- **`level0`**, a scene of its settings alone, and `globalgamemanagers.assets`
  with what the Player preloads (the default resources' scripts).

Once the preparation has compiled the project into its copy of the host, it
completes it (`complete_player`, a hook of `compiler::prepare`):

- every script that Unity imports a `MonoScript` for gets one in
  `globalgamemanagers.assets`, which `MonoManager.m_Scripts` lists after the
  default resources' scripts: the class a `.cs` source of its name declares
  (in its namespace, after a byte order mark if the file starts with one),
  and the classes of a plugin's assembly. Another class gets none, as in a
  build: a `MonoScript` of UniTask's `AsyncParticleUpdateJobScheduledTrigger`
  (declared in `MonoBehaviourMessagesTriggers.cs`) has the Player check its
  messages and log a script error. The Player loads them at its start on its main
  thread, so content that names a script of any compiled assembly loads on a
  load thread without the crash described in
  [UNITY_RELEASES.md](UNITY_RELEASES.md);
- the sources in `Resources` folders (not below an `Editor` folder) are
  written into `resources.assets` with what they refer to, by the tool's
  writer, and `ResourceManager` names each object by its path below the last
  `Resources` folder, without extension, in lower case (a prefab by its root);
- the shaders they use are imported and baked by the tool into that file
  (`native::project_shaders`), as a build holds them, and `ScriptMapper`
  names them; their scripts are those of `globalgamemanagers.assets`, which
  the file refers to (`playerbake::ScriptsElsewhere`): a `MonoScript` a
  build's other file holds is not found by the Player, whose behaviours
  then have none.

- the sources of the project's scripted importers (`ScriptedImporter`, the
  game's Editor code) are imported by that code, which the Player of Play
  mode code runs (`native::scripted_imports`, `Runtime/ScriptedImports.cs`):
  started with `UME_SCRIPTED_IMPORTS`, the Player makes each importer
  (with its settings from the `.meta`), calls `OnImportAsset` with a stand-in
  import context, and answers with the objects added, as `JsonUtility`
  writes their fields. A stand-in `SerializedObject` made of an object in
  memory reads and writes its fields (the linker has a stand-in constructor
  call the bridge's `Construct`). The tool writes those objects under the
  file IDs the Editor gives an importer's objects: XXH64 (seed 0) of
  `Type:<class>-><identifier><n>`, the class `MonoBehaviour` for a script's
  object and `n` the first from 0 the asset does not use yet (read from the
  6000.0 Editor; Bouncemasters' assets name its certificates by
  `Type:MonoBehaviour->certificate0`). The documents are kept in
  `Library/UnityMicroEditor/scripted-imports`, by the source's and its
  `.meta`'s hashes, for the sessions' writer. Only objects of scripts are
  answered; an importer that adds an engine object (a texture, a mesh) is
  refused.

The host's Build Settings name, after the bootstrap scene, an empty scene of
the same name at the build index of each enabled scene of the project
(`Assets/UnityMicroEditorGenerated/BuildScenes/<index>/<name>.unity`,
`level1`...), into which the session builds the authored scene when the game
loads it, as the Editor-built host does; the preloaded assets are cleared, as
the Editor's host build clears them.

What writing `resources.assets` brought up in Bouncemasters:

- `ProjectContext.prefab` holds an instance of a prefab the project lacks
  (`Facebook`). A build leaves a missing prefab's instance out and keeps the
  rest; references to its objects are empty, and a transform lists no child
  that was not written (the Player follows every entry of `m_Children` and
  crashed in `Transform::SetSiblingIndexInternal`).
- A material's script object whose script the project lacks (URP's
  Editor-only `AssetVersion`) is left out, as a build leaves it out, instead
  of the material.
- SRDebugger's prefabs name uGUI's scripts by the GUID of Unity 5's
  `GUISystem/UnityEngine.UI.dll` (`f5f67c52d1564df4a8936ccd202a3bd8`),
  pointed at the package's scripts as the later one is.

### Verified on 2026-10-24

Bouncemasters (a copy of the project, `--compile-for Android`, `--play-mode`,
`UME_OWN_IMPORTER=only`): `assemble-host` in 21 s (41 built-in shaders baked,
`Hidden/VideoDecodeOSX` refused), then a preparation with it in 236 s
(completing the Player: 1157 scripts, 388 resources, 9 sources of scripted
importers, 79 sources refused) and `run --frames 2400`: the game loads,
draws its main screen with the penguin and its offline earnings, and logs
no exception; its errors are the game's own failing gRPC calls, which a
Player of an Editor-built host logs as well. No Editor and no import worker
ran. What the Player still logs: `AsyncResourceUpload failed` for textures
the session's own writer writes (as with an Editor-built host), and what
the writer refuses (textures with mipmaps, PSD and JPG images, scaled ones;
animator controllers; the classes the release's engine pack lacks).

### The manager pack

The layouts and defaults of the managers come from a reference capture,
`tools/ReferenceCapture/ManagerCapture.cs`, run once per release by the
tool's developers with a licensed Editor in an empty project: it writes the
objects of `ProjectSettings/*.asset` and of `Library/BuildSettings.asset`,
`MonoManager.asset` and `ScriptMapper` with `ContentBuildInterface.WriteSerializedFile`
for `StandaloneLinux64`, with their type trees. The file is shipped as
`playerbake/engine/<release>/managers.bin`. Two of its trees do not describe
the bytes the Player writes, and are corrected when read: 6000.0's
`UnityConnectSettings` has no `InsightsSettings`, and its `PlayerSettings`
ends in four more bytes (zero in every build seen; named
`m_UnnamedPlayerTail`). The managers that hold nothing (`DelayedCallManager`,
`RuntimeInitializeOnLoadManager`, class 1403656975) and `ResourceManager`
are written without a tree, with the type hashes an Editor build gives them.

The project's settings are written over the pack's defaults field by field,
by name, also where a file is of an older serialized version than the
engine's (Bouncemasters' `DynamicsManager` is version 3, the engine writes
23), with what the engine does when it upgrades such a file:

- `TimeManager`'s fixed timestep of version 1, a float of seconds, becomes
  a count of 1/141120000 s, the single's value rounded down;
- `DynamicsManager` below its version takes `m_AutoSyncTransforms` 1 and
  `m_DefaultMaxAngularSpeed` 7 where it lacks them, `Physics2DSettings`
  `m_AutoSyncTransforms` 1;
- numbers written `.001`, as older Editors wrote them, are read.

References to the engine's own objects are kept (`m_AlwaysIncludedShaders`);
references to project assets are written empty for now (see below). What a
build derives: `BuildSettings` names the bootstrap scene and OpenGL Core;
`GraphicsSettings.m_ShaderDefinesPerShaderCompiler` holds, for OpenGL Core,
each tier's platform defines as the baker computes them; `ScriptMapper`
keeps every name of the engine's registry, pointing where the build holds
its shader or nowhere, and adds the shaders the build holds. Against an
Editor build of Bouncemasters, field by field, the managers differ in:

- the references to project assets (splash logos and backgrounds,
  `preloadedAssets`, the default 2D physics material,
  `m_SpritesDefaultMaterial`, the invalidated pattern texture);
- the splash backgrounds' aspects, which a build computes from them;
- `PlayerSettings.AID`, which is not the project's (two Editor builds of
  Bouncemasters have two values, one of them the empty project's);
- `useHDR` of the tiers (an upgrade of `m_TierSettings: []` gives `false`
  in the Editor build);
- `MonoManager.m_ScriptHashes`, left empty.

### Built-in shaders

`unity_builtin_extra` of a build holds the shaders the project always
includes, those a 6000.0 build adds to them (28 path IDs, shipped as a
table: the engine's stencil and blit shaders, UI Toolkit's, the video
decoders'), what they take passes of or fall back on, and those the
project's materials name. Each is baked from the Editor's object with the
variants the project's materials use. Against the Editor build of
Bouncemasters: all 40 shaders both hold are equal to the byte;
`Hidden/VideoDecodeOSX` is refused (its programs warn, and what a warning
leaves in the object is not reproduced). `Legacy Shaders/Diffuse` showed a
rule of the keyword table: the keywords of the snippets of a shader's own
passes come first, by snippet ID, then those of the snippets its fallback
brings (`Legacy Shaders/VertexLit`'s, of lower IDs here), by ID.


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

Since 2026-10-20 every scene is written in the Player format by the import
worker and loaded by the engine ([NATIVE_IMPORT.md](NATIVE_IMPORT.md#scenes-in-the-player-format));
the tool's former scene loader is gone. The dated sections below record what a
preparation needed at the time. What served only that loader no longer
applies: the upgrade of older materials and meshes for the session, the
pipeline's objects loaded as authored ScriptableObjects, the global settings
the session published with a stripped runtime list (the Player now keeps the
host's), the adaptation of models of a pipeline project, the restoration of UI
references, the trust of package ScriptableObjects without lifecycle
instrumentation, and the Addressables prefabs that loader could not adapt.
The settings managers, the package builds, the Editor compilation and the
shader builds still apply.

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
  `rust/crates/playerbake/src/playerbake/engine/type-<class>.bin`. They were taken from a file the
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
  importers and the scene write. `UME_EDITOR_COMPILATION=0` turns the
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

### Verified on 2026-10-17

The check used a fresh copy of `Samples/RawProject` with no `Library`,
prepared with `--host-player` set to the sample's host:

- **Preparation.** It finished in 56 s and started no Editor: no staging
  copy was made.
- **Player.** `run --frames 200` played 200 frames with no error and no
  exception, and exited with code 0.

### A URP project without the Editor (2026-10-17)

The URP copy (`Temp/e2e/nobuild/urp8`) was prepared on a sample host with
`--host-player`, and no Editor ran. `run --frames 200` played 200 frames with
no error and no exception. The Player reported the project's global settings
active, with 17 runtime entries and URP's SSAO resources present. The
preparation took 235 s, with every shader build taken from the project's
store.

What the run needed, besides UI Toolkit imports in the worker
([UI_IMPORTS.md](UI_IMPORTS.md)):

- **Graphics settings as a build serializes them.** A Player reads a global
  settings asset's runtime list (`m_RuntimeSettings`). A build fills that list
  from the Editor's list (`m_SettingsList`), so the authored text holds
  whatever the last save left there. The worker now asks the pipeline's own
  build code which entries a Player build keeps (the `settings` request of
  `tools/WorkerScripts/PlayerObjectImporter.cs`):
  - an entry whose type has active `IRenderPipelineGraphicsSettingsStripper<T>`
    strippers is kept unless all of them remove it;
  - any other entry is kept when it is `isAvailableInPlayerBuild`.

  The session then publishes the asset with that runtime list, without the
  Editor's list, and without the stripped entries of the reference registry
  or what they name. For the URP copy, 21 of 38 entries were stripped,
  among them the Editor shaders and materials, the 2D resources, the probe
  volume baking resources and the rendering debugger's resources. This
  replaces an earlier rule that followed no reference into an `Editor`
  folder. That rule was wrong for assets: URP's default volume profile lies
  in one, and the Player uses it.
- **Older Mesh sources.** SRP Core's sky meshes are `.mesh` files of Mesh
  serialization 11. The worker has the engine load and save them, and the
  session takes the fields at serialization 12, as it does for older
  materials.
- **Missing references.** A reference to a GUID that no source of the project
  or its packages has is published as empty, as Unity reads it, in the asset's
  data as well as in its reference list. The URP copy's renderer names a
  probe sampling mesh that its URP version no longer ships.
- **The pipeline's objects as raw ScriptableObjects.** The global settings,
  the pipeline asset, renderer data, renderer features and `PostProcessData`
  of URP 17.6 are loaded from their authored fields. The runtime trusts these
  types without lifecycle instrumentation, for the reasons given in
  `TrustedHostScriptableObject`. The global settings and the pipeline asset
  get their data before the pipeline takes them. Managed reference entries are
  written with `rid`, `type`, `data` first, as Unity writes them, and
  renderer feature maps (`m_RendererFeatureMap`, little-endian Int64 hex) are
  decoded.
- **Shaders with no program.** URP's stripper removes every variant of the
  post-processing shaders when no renderer uses post-processing. The build
  keeps such a shader where an object names it, and the Player finds it
  unsupported, as a built Player does. These shaders are now marked
  (`programlessShaders`), and the runtime does not require them to be
  supported.

Before those changes, the preparation stopped first at UI Toolkit assets and
then at each of these in turn.

## A game project without the Editor (2026-10-18)

A copy of Dungeon (URP, Netcode, VContainer, KINEMATION, UI Toolkit) was
prepared for `Assets/Dungeon/Scenes/ShootingRange.unity` with
`--host-player`, and no Editor ran. `run --frames 600 --screenshot` played
600 frames with no error and no exception, and the screenshot shows the scene
and the HUD as the Editor's Play Mode does. The preparation stopped at each of
the following in turn.

**Compilation of the Editor assemblies.**

- Unity 6.6 references `UnityEngine.UI` (and `UnityEditor.UI` in the Editor)
  from almost every assembly, and the test framework from Editor and test
  assemblies, without the asmdef naming them. The compilation adds them
  where no cycle results, and reads the reference `Unity.ugui` as those
  assemblies.
- A plugin's "Any Platform" is the unnamed `Any` entry of its `.meta`; the
  legacy `Any:` entry counts only where the unnamed one is absent.
- Netcode's code generator and `Mono.Cecil` are found in the package
  snapshot when the project has no `PackageCache`.
- The engine profile does not prove the tool's bridge against its sources:
  the preparation compiles it.
- Scripted importers load the assemblies immutable packages ship compiled
  (nunit for an assembly the test framework is referenced from), as
  material inspectors already did.

**Sources.**

- A name Unity hides (starting with `.`, ending with `~`, `cvs`, `*.tmp`) is
  not indexed, so a package's `Samples~` holds no asset.
- An `.anim` larger than what is baked from text without an Editor catalog
  is imported instead.
- An authored Mesh may skin a vertex with one to four bones, and one a script
  saved may have no bone name hashes.

**Models of a pipeline project.** A model's generated materials take the
pipeline's Lit shader from URP's `FBXMaterialDescriptionPreprocessor`, and a
part without a material of its own takes the pipeline's default material
(`Lit.mat`, through the Editor entries of the global settings). Such models
(`native/pipeline_models.rs`) are imported in the context of a build of the
Lit shader: the pipeline's assets, global settings and Editor assemblies.

A model's renderers may name a `.mat` source, the default material or one
its `externalObjects` remap to. A ContentFile reaches another file only
through that file's ContentFile, and an authored material is loaded from its
source. The renderers are placed from the model's static graph, which keeps
the material's identity, so the adapted file drops the reference and the
renderers' pointers along it (`runtimeartifact::unbind_source_references`),
and the scene binds the authored material.

The static graph now also takes `SkinnedMeshRenderer` and `Animator`, read
by the artifact's type trees, and a model's clips and `Avatar`.

**UI.** See [UI_IMPORTS.md](UI_IMPORTS.md) for `<Style src>` references
and custom elements. A `PanelSettings` names UI Toolkit's shaders in
`unity_builtin_extra` (path IDs 9001, 9100, 9101, 19011, 19012, 20300–20302)
and its ICU data (20204). The runtime resolves the shaders by name with
`Shader.Find`.

**ScriptableObjects of packages the host provides.** `VContainerSettings`
is trusted without lifecycle instrumentation: it takes its instance in
`OnEnable`, and the runtime makes the root scope of the restored data through
VContainer's public API (`InitializePreloadedRootScopes`), as it did for the
Editor-imported asset.

**Limits of this route:**

- An animation event of a model's clip that passes a script (`MonoScript`)
  as its object parameter passes none. Neither a ContentFile nor the scene
  loader here holds a `MonoScript`. A receiver taking `int`, `float` or
  `string`, as all of Dungeon's do, sees no difference.
- No public API reaches a Player's ICU data, so a panel falls back to basic
  line breaking, as with a panel made at runtime.
- A host keeps the shaders of USS filters and of sprites inside text only when
  its own build used them. Without one, the runtime logs a warning and the
  panel draws no filter, or no sprite in a text. The other panel shaders are
  required.
- `run` prepares the project's Addressables scenes and prefabs before the
  Player starts. One the loader cannot adapt is skipped with an event:
  Dungeon's `Zombie` holds a `NavMeshAgent`, its `VB_*` door prefabs a
  `NavMeshObstacle`, and `ShipGenDemo` and `TestRoom` reach those prefabs.
  The other 427 prefabs are served.

**Start time.** A preparation and the runs after it share one native store
(see [RUNNER.md](RUNNER.md#the-native-store-of-a-project)), and a new output
starts from the preparation's index, so `run` no longer imports again what
the preparation imported. Addressables entries are refused one by one, before
their content is imported where their own sources tell, and a refused scene
is remembered until its sources change. Measured on ShootingRange to the
first frame, one phase at a time: cold (wiped `Library`, preparation and run)
1884 s instead of 2847 s, warm (a new preparation and run) 165 s instead of
439 s, and a run of the prepared scene 33 s instead of 50 s.

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
  Editor (see above). In that route:
  - The Editor compilation is made once per preparation. It is not redone in
    a watched session.
  - `Unity.Collections.CodeGen` is left out, because it names an Editor
    assembly of the package that the compilation lacks.
- **The runtime bridge.** A preparation compiles the tool's bridge
  (`UnityMicroEditor.dll`) from the installation's sources with the SDK, as
  a session does. It puts the bridge in place of the host's, with its
  `RuntimeInitializeOnLoad` methods in place of the host's rows for it. A
  host has to hold an assembly of that name only, so a tool update or a
  change to `Packages/com.unitymicroeditor.runtime/Runtime` needs no new
  host. Checked on 2026-10-17: a host built several bridge revisions earlier
  prepared and ran the URP copy (200 frames, no error). A session prepared
  before such a change still refuses it ("regenerate source graph"), and a
  new preparation takes it.
- **Shader build time.** Each shader is built in its own worker run, about
  20 s each. Since 2026-10-17, a pipeline project's resources are prebuilt
  side by side and kept for the project
  ([NATIVE_IMPORT.md](NATIVE_IMPORT.md#projects-with-a-scriptable-render-pipeline)).
  A preparation of the URP copy took 11 minutes instead of 36.
- **Render pipelines.** URP's shaders are built per project in the import
  worker with URP's own Editor code, and its runtime assemblies are package
  builds. A host without URP therefore runs a URP project. Shaders of a
  package that only a script finds by name (`Shader.Find`) stay those of the
  host, and a host without the pipeline has none of them.


### Unity 6000.6 (2026-10-24)

The release's packs come from the same reference captures:
`engine/6000.6.3f1/managers.bin` (`ManagerCapture.cs` in an empty 6000.6
project; it now leaves out script objects, such as 6000.6's
`ProjectAuditorSettings`, which fail the write), `library.bin` and
`script-templates.bin` (the import worker's dependency files of Dungeon and
the Editor's `unity_builtin_extra`), `editor-type-48.bin` (the shader
fixtures' Editor tree). What an Editor build of Dungeon shows of 6000.6:

- the managers in another order: the cluster input manager is written (ID
  16), IDs 17 and 19 are not given, and `UnityConnectSettings` is not
  written; its capture's tree, like 6000.0's, names an `InsightsSettings`
  the bytes lack; the managers of no tree and `PreloadData` have 6000.0's
  type hashes;
- the engine's assemblies are not in the variation's `Data/Managed` but
  beside the variations, in `Variations/mono/Managed`;
- 85 engine symbols (`PLAYER_DEFINES_6000_6`), and for API compatibility
  level 6 (.NET Standard 2.1) `NETSTANDARD`, `NETSTANDARD2_1`,
  `NET_STANDARD`, `NET_STANDARD_2_0`, `NET_STANDARD_2_1` and the references
  `NetStandard/ref/2.1.0/netstandard.dll`, its `shims/netstandard` and
  `shims/netfx`, `Extensions/2.0.0` and
  `BCLExtensions/TargetingPacks/netstandard2.1/ref`;
- 32 built-in shaders the engine adds to those always included, five of
  them new (20500-20507);
- `Object.GetInstanceID` is an error to compile; the bridge takes an
  object's identity through `EngineCompat.Identity`.

The settings' references to the project's assets (the render pipeline
asset, its global settings, preloaded assets, in the files of the Player's
managers) become roots of `resources.assets`, and the managers point at
them there.

A project with a scriptable render pipeline is not assembled yet: the
built-in shaders are baked with the variants a build keeps, which for a
pipeline its Editor scripts and keyword filters decide.
