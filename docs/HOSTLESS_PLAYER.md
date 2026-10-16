# Toward a Player without a host build

Status, 2026-10-16. Two steps are live:

- **Settings.** Preparation and watched sessions write the Player's settings
  managers from the project's `ProjectSettings` (see "In preparation and
  sessions" below).
- **Packages.** A module package adds its engine module assembly to the
  compile references (see "Packages" below).

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

The source graph already derives the compile configuration of every assembly,
package assemblies included, from `.asmdef` files. It takes a package assembly
from the host only when the host's build proof matches it; otherwise the SDK
Roslyn compiles it like game code. The one missing piece was the engine module
references. They were frozen at the host build, from the modules the Editor
had enabled then. `package_module_references` now adds
`UnityEngine.<Module>Module.dll` for each resolved `com.unity.modules.*`
package. The mapping comes from the platform's `modules.asset`, which lists the
modules a built-in package controls.

Verified on the same sample copy, with no host rebuild. `com.unity.ai.navigation`
2.0.14 was added to the manifest. It was compiled by the tool, and in the Player
its `NavMeshSurface` built a NavMesh that `NavMesh.SamplePosition` found.
`com.unity.mathematics` 1.4.0 also loaded, but in 6000.6 that package is only a
small forwarding assembly.

## Limits and what remains

- **Settings.** Writing the managers needs a base `globalgamemanagers` of the
  same engine for the build-derived managers. Per-class upgrades of older
  serialized versions are missing: such a manager keeps the host's values,
  and only `settings-managers.json` says so. Binary-serialized settings files
  are not read.
- **Packages.** Compiling package assemblies without a host proof works only
  for packages without IL post-processors or source generators that matter at
  runtime (Netcode, Entities, Burst-compiled jobs). Native plugins, Burst AOT
  output and Addressables catalogs still come from the host.
- **Render pipelines.** A scriptable render pipeline still needs its compiled
  shaders and resources from a host build. Compiling them with
  `UnityShaderCompiler`, including variant enumeration and stripping, is the
  largest remaining part of a Player without a host build.
