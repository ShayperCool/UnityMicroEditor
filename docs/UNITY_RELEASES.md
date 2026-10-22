# Unity releases and device projects

The tool serves the projects of more than one Unity release. `6000.6.3f1` is the verified release; `6000.0.71f1` is supported for the parts listed below. This guide says how a release is chosen, what differs between the two, and how a game written for a phone (its code for Android or iOS alone) is prepared and run in the Linux Player.

## How a release is chosen

- The project's `ProjectSettings/ProjectVersion.txt` selects the release (`release::of_project` in `ume-base`, `engine::of_project` for the worker). A release the tool does not know is refused with the list of the ones it does: `Unity 2022.3.62f1 is not supported (supported: 6000.6.3f1, 6000.0.71f1)`. A project without the file is taken for the verified release's, as an Editor takes it for its own.
- Without `--editor` (and without `UNITY_EDITOR`), `auto-prepare`, `run`, `mcp` and `build-project` take the Hub installation of the project's release, `$HOME/Unity/Hub/Editor/<version>/Editor/Unity`.
- The installation has to report the project's release (`Unity -version`); the Editor bridge, the worker and the bridge compile check it.
- Every record the tool writes for a project (receipts, engine and source profiles, proofs, manifests) carries the project's release, and a record of another release does not fit it.

## What differs by release

| | 6000.6.3f1 | 6000.0.71f1 |
| --- | --- | --- |
| Import worker, artifact storage, asset databases | `engine/unity6000_6.rs`, with `Unity.DataStore` | `engine/unity6000_0.rs`, artifacts in `Library/Artifacts` ([NATIVE_IMPORT.md](NATIVE_IMPORT.md#unity-releases)) |
| C# compiler | Roslyn of `Data/DotNetSdk` with its .NET 8 SDK | Roslyn of `Data/DotNetSdkRoslyn`; the tool's own helpers need a .NET 8 SDK, which comes from `UME_DOTNET` or from another Unity installation of the Hub that ships one (6000.6) |
| Build tools (`Unity.ILPP.*`, source generators) | `Data/Tools/BuildPipeline/...` | `Data/Tools/...` |
| Built-in IL post-processors | `BuiltinILPPs`, run on every package assembly the tool compiles | none; as Bee does, the IL post-processor runner is not started without one |
| Player reference assemblies | `NetStandard` and `MonoBleedingEdge` | `UnityReferenceAssemblies/unity-4.8-api` |
| Packages the runtime package is built with | SRP Core `17.6.0`, Collections `6.6.0` | none (SRP Core of this line brings Burst along) |
| Runtime package APIs | `UnityEngine.Assemblies`, `EntityId` | `AppDomain`, `Assembly.LoadFrom`, `GetInstanceID` (`Runtime/EngineCompat.cs`, selected by `UNITY_6000_6_OR_NEWER`) |
| Settings managers of the Player | written from `ProjectSettings` over the host's (`playerbake::managers`) | the host's: the manager layouts are 6000.6's. A host is built from the project's own stage, so its settings are the project's at that time; a changed `ProjectSettings` file needs a new host (`--rebuild-host`) |
| Shaders of a built-in pipeline project | the tool's baker and `UnityShaderCompiler` client | the Editor's build code in the worker, as for a pipeline project |
| Native content the Editor exports during a host build | used | not used; the worker imports all of it |

The tool compiles its bridge, and the importer script it runs in the worker, with the version symbols the Editor of the release defines (`UNITY_6000_0_71`, `UNITY_6000_0`, `UNITY_6000`, every `UNITY_X_Y_OR_NEWER` up to its own; `release::version_defines`).

## A game written for a device

A mobile game is usually edited with Android or iOS as the active build target, and its code may compile only for that platform: its main assembly can be limited to `Android`, `iOS` and `Editor` in its asmdef, and code under `#if UNITY_ANDROID || UNITY_IOS` can have no other branch. Its Standalone scripting define symbols are often stale. The Player of the tool is a Linux one, so such a project is prepared with:

```bash
unity-micro-editor auto-prepare \
  --project /absolute/project \
  --scene Assets/Scenes/Loading.unity \
  --compile-for Android \
  --request-id initial
```

`--compile-for <group>` (a build target group as `ProjectSettings.asset` names it: `Android`, `iPhone`, `WebGL`) is remembered for the project in `Library/UnityMicroEditor/compile-for.json`; later preparations, `run` and `mcp` sessions use it, and `--compile-for Standalone` removes it. It makes the Player's code compile as the Editor's Play mode compiles it with that group active:

- the group's scripting define symbols instead of the Standalone ones, with the group's platform symbols (`UNITY_ANDROID`, `UNITY_ANDROID_API`, `PLATFORM_ANDROID`; `UNITY_IOS`, `UNITY_IPHONE`, `PLATFORM_IOS`);
- the group's platform in asmdef `includePlatforms`/`excludePlatforms` and in the platform settings of managed plugins;
- the stage's Linux build gets the same symbols, and the stage's own asmdefs under `Assets` stay out of its Player builds (the define constraint `UNITY_EDITOR || !UNITY_MICRO_EDITOR_HOST_BUILD`): the host leaves the game's assemblies out anyway, and with the device's symbols they need assemblies a Linux build does not have.

The engine stays a Linux Player: code that calls the device's native plugins under those symbols (JNI, Objective-C bindings) fails at run time unless it also checks for the Editor, and native libraries for the device are not loaded.

## Packages only the project's Editor could fetch

A project may take packages from a scoped registry that needs credentials, or from private Git repositories, which the user's Editor fetched with its own. The resolver then works from what that Editor's Package Manager left in `Library/PackageCache`:

- A registry package found there is taken by its manifest, as before; a copy the resolver downloaded and verified by its receipt wins over UPM's copy of the same version, whose `package.json` UPM rewrites.
- A Git package whose remote cannot be read is taken from the PackageCache copy whose `_fingerprint` is the commit `packages-lock.json` names (proof `upm_fingerprinted_cache_copy`), online and offline alike.
- The stage's `Packages/manifest.json` names every package of a scoped registry other than Unity's, and every Git package, as a `file:` dependency on the copy the resolver took, so the stage's Editor fetches nothing it could not authenticate and builds with exactly the packages the tool compiles.

## Scenes the game loads by name

A game that loads the scenes of its Build Settings through `SceneManager` (`LoadSceneAsync("Main", LoadSceneMode.Additive)`, by name, path or build index) gets them from sources: the host is built with an empty scene of the same name at each build index after the first, the runner adds each of those scenes to the session's content (`buildScenes` in the content routes), and when the engine loads the stand-in the bridge builds the authored scene into it. Adding a scene to the Build Settings needs a new host. `LoadSceneMode.Single` of such a scene is not verified.

## What the Editor accepts and the tool now accepts too

- an asmdef or asmref saved with a UTF-8 byte order mark, and a version define with blanks around it;
- a reference, by name or GUID, from a project asmdef to an assembly the project does not have, and a reference to an assembly the platform or its constraints leave out (`UnityEngine.Purchasing.SecurityStub` beside its device counterpart): both are left out of the compile;
- a `.meta` that still holds a committed merge conflict: the first `guid:` line counts, as in the Editor's `SourceAssetDB`;
- a plugin that a project's build callbacks enable for the Player being built (Odin Inspector's `NoEditor` assemblies): it counts when the host, which is such a build, holds the same file; a Mono class library facade the build keeps over a package's plugin of the same name (`System.Buffers.dll`) stays;
- two copies of an Editor tool's assemblies (External Dependency Manager in `Assets` and as a package) in the Editor compilation: the project's copy is taken;
- the host build packs no sprite atlases (the host carries no game content), and takes the precompiled references after the project's build callbacks ran; any other change of the host's assemblies during the build is named in the error.

## Limits

- The verified release's checks against engine-built bytes (playerbake corpora, shader builds, hot reload's detours) were not repeated for 6000.0.
- Hot reload in a 6000.0 Player is not verified; run with `--no-hot-reload` if a patch misbehaves.
- The Editor-free host assembly, component defaults capture and `project_build` were not tried on 6000.0.
