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
- the stage's Linux build gets the same symbols, and takes the device's assemblies: every asmdef of the stage's `Assets`, and of an embedded copy of each package that has one, is a Linux assembly exactly when it is one of the device. The host is then a Linux build of the device's code, and what its Resources hold is serialized with the game's scripts (Zenject's `ProjectContext` and the like are loaded from there).

The engine stays a Linux Player: code that calls the device's native plugins under those symbols (JNI, Objective-C bindings) fails at run time unless it also checks for the Editor, and native libraries for the device are not loaded. In the Editor's Play mode such code is skipped by `#if UNITY_EDITOR`; [Play mode code](#play-mode-code) takes the same paths in the Player.

The Player also gets the class library assemblies its plugins use that the host lacks (`System.Web` of `nunit.framework`), from the Player's `unityjit-linux` profile, as a build copies them; and a plugin below an `Editor` folder is never a Player's.

## Play mode code

Games keep what they do in the Editor under `#if UNITY_EDITOR`: an ads, store or consent SDK answers at once there instead of calling its device's native library. A Player compiled as a Player build compiles takes the device's path and waits on a library that is not there. `--play-mode` compiles the Player's code as the Editor compiles it for Play mode, so that the Player runs what the Editor runs:

```bash
unity-micro-editor auto-prepare --project /absolute/project --scene Assets/Scenes/Loading.unity \
  --compile-for Android --play-mode --request-id play-mode
```

It is remembered beside `--compile-for` in `Library/UnityMicroEditor/compile-for.json`; `--play-mode=false` turns it off. The host stays the same Player build; the code changes:

- every assembly the Editor compiles that is not for the Editor alone, with the symbols the Editor of the release defines (`UNITY_EDITOR`, `UNITY_EDITOR_64`, `UNITY_EDITOR_LINUX` and the others of its `Library/Bee` record) and those of the active platform: for a device group the Editor's symbols of that platform replace the Linux Player's (`UNITY_STANDALONE*` go; `UNITY_ANDROID`, `ENABLE_EGL` and the rest of what 6000.0.71f1's Editor defines with Android active come; for `iPhone` and `WebGL`, of which no record was taken, the platform symbols of a Player); the Editor's own assemblies that one of them refers to come along (uGUI's `UnityEditor.UI`);
- the packages' assemblies as well, in place of the host's builds of them, and the plugins the Editor takes (Odin Inspector's Editor assemblies rather than its `NoEditor` ones), except those below an `Editor` folder, which are the Editor's tools;
- against the Editor's builds of the engine modules and the Editor's own modules, as the Editor compiles them.

Such code names what a Player does not have, and the tool links it into the Player (`tools/PlayModeLinker`, on the Cecil that ships with the installation's IL2CPP):

- The Editor's assemblies the code names (`UnityEditor.CoreModule`, the platforms' extensions) are written into the Player as stand-ins with exactly the types and members it names. A member answers what an Editor answers when it finds nothing: an empty array or text, `0`, `false` or `null`, unless Play mode answers otherwise, which the runtime bridge provides (`Runtime/PlayModeEditor.cs`): `EditorApplication.isPlaying` and `isPlayingOrWillChangePlaymode` are true, `timeSinceStartup` is the real-time clock, `EditorPrefs` and `SessionState` keep values for the run, what the code adds to `EditorApplication.update` and `delayCall` is called every frame and once, `EditorUserBuildSettings.activeBuildTarget` is the platform of `--compile-for`, and what the code adds to `playModeStateChanged` hears `EnteredPlayMode` once the first scene woke and `ExitingPlayMode` as the Player quits. The Player runs in the project's directory, as the Editor does, so a path the code names (`Assets/...`) is the file itself: `AssetDatabase.IsValidFolder`, `AssetPathToGUID`, `GUIDToAssetPath` and `GetAllAssetPaths` answer from the project's files and their `.meta` files, and `LoadAssetAtPath` gives an asset of a `Resources` folder, which the Player holds; no other asset is found, and `EditorUtility.SetDirty` and the like do nothing.
- As the Editor does when it enters Play mode with a domain reload, the Player runs, before the first `[RuntimeInitializeOnLoadMethod]`, the static constructor of every `[InitializeOnLoad]` class of the Play mode assemblies, then every `[InitializeOnLoadMethod]`, then every `[InitializeOnEnterPlayMode]` method (the lifecycle compiler gives each a bridge, as it gives runtime initializers); an exception is logged and the next one runs. In Bouncemasters these were 8, among them DOTween's, Spine's and the game's own build checks.
- A member of the engine that only the Editor's engine has (uGUI's `CanvasRenderer.onRequestRebuild`, `Texture2D.alphaIsTransparency`) is called through a stand-in that does nothing. For the game's code, not the engine's or the bridge's, `Application.isEditor` is true, `Application.platform` is `LinuxEditor`, and `Application.Quit` is ignored, as the Editor ignores it in Play mode; so is a request of the game's Editor code to leave Play mode (`EditorApplication.isPlaying = false`). The Player logs each with the code that asked.
- The Player reads what the host build serialized (its Resources, the assets of its managers) without type trees, in the layout of the host's builds of the scripts. The preparation keeps those builds beside the Player (`<Player>_Data/UnityMicroEditorLayout`) and aligns every Play mode assembly with them: a serialized field only the Editor's build declares (`[SerializeField]` under `#if UNITY_EDITOR`) is not serialized in the Player, and one only the Player's build declares is added. Scenes and content the worker writes carry type trees and are read either way, but the value of an Editor-only field is not.

Every compile links what it builds, so a session starts and restarts on linked code; the stand-ins are written again for each Player, the same stand-ins in the same bytes. A watched session patches Play mode code as it patches any other ([HOT_RELOAD.md](HOT_RELOAD.md)), the linked assembly against the one the Player loaded; an edit that names a type or member of the Editor the running Player's stand-ins lack is refused with `editor_stand_ins_changed`, since only a new Player gets new stand-ins. Limits:

- The Editor's own work around Play mode beyond the above does not happen: no inspector or Editor window, and the `[InitializeOnLoad]` code of assemblies shipped compiled (precompiled plugins) does not run.
- Native libraries the Editor loads for the desktop (Firebase's `FirebaseCppApp`) are loaded where the host build carries them for Linux. Such a library may carry what another of the game's does (Firebase's gRPC and the gRPC of the game's own services): the Player opens the plugins of `<Player>_Data/Plugins` with `RTLD_DEEPBIND`, through a preloaded library the system's C compiler builds once (`tools/PluginDeepbind`, as zavod's Editor images do), and the session's copy of each plugin has its `STB_GNU_UNIQUE` symbols made weak, which the loader would otherwise share between the two (the tables of gRPC's metadata). Without a C compiler the plugins share their symbols, as before.
- What the code reaches only through the Editor (an asset that is not a `Resources` one by its path, a type of an Editor assembly by reflection) is not there; the game's own error handling decides what follows.

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

## Verified on 2026-10-22

A copy of a large 6000.0.71f1 mobile game (Android/iOS, Addressables, Zenject, Odin Inspector, a private registry and private Git packages, 1,500 assets in its main scene) prepared with `--compile-for Android`: the host (a Linux build of the device's code, about 12 minutes with a warm stage), the source graph, the Player and Editor compilations, and the Loading scene written by the 6000.0 worker; `run` then wrote the Build Settings scene `Main` (1,546 imports) and started the Player. The game's own loading ran its Zenject `ProjectContext` and its loading steps until a consent SDK that answers only through Android JNI. With a scratch change to the game's copy that gives up a step after 10 s (not a tool change), the loading finished, `SceneManager.LoadSceneAsync("Main")` loaded the stand-in and the bridge built `Main` into it, and the game drew its first gameplay screen. The game's SDKs for the device (ads, store, social, consent) report exceptions in such a Player. A 6000.6.3f1 copy of `Samples/RawProject` prepared and ran unchanged.

The same copy, with the scratch change removed, prepared with `--play-mode` (193 assemblies, every one the Editor compiles, about five minutes with a warm store): the consent SDK took its Editor path as in the Editor's Play mode ("Usercentrics CMP doesn't work in editor"), the game showed its own terms window, and after a click on its button (through the session's control directory) loaded its first level and drew the penguin on its tower, waiting for a tap. On the way, Firebase's desktop library and the game's gRPC services crashed the Player until the plugins kept their own symbols, and the Editor 6000.0.71f1 of this machine itself crashes in Play mode of this project where it loads Firebase's library. What the game's Editor paths need of the Editor beyond the stand-ins (AppLovin MAX's stub banner from a prefab outside `Resources`, a splash from the Player settings through Hive's Editor reflection) logs exceptions there, which the game survives.

## Limits

- The verified release's checks against engine-built bytes (playerbake corpora, shader builds, hot reload's detours) were not repeated for 6000.0.
- Hot reload in a 6000.0 Player is not verified; run with `--no-hot-reload` if a patch misbehaves.
- The Editor-free host assembly, component defaults capture and `project_build` were not tried on 6000.0.
