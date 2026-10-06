# Unity Micro Editor

Unity Micro Editor is an experimental CLI for running and iterating on Unity projects through a reusable Unity Mono Player. It reads authored scenes and prefabs, compiles project code, prepares imported content, and updates a running scene. The Unity runtime supplies rendering, physics, audio, and the PlayerLoop.

The current implementation runs Dungeon's SampleScene and its multiplayer ship scene on Linux. Two Players have joined the same relay session, generated matching ships, and replicated movement, doors, and power state. Running an arbitrary fresh Unity project without a prepared baseline is still unfinished.

## How it works

The Go runner reads `.unity`, `.prefab`, `.meta`, ScriptableObjects, assembly definitions, and source dependencies. SDK Roslyn compiles the C# assemblies. A C# loader inside the Player creates the scene graph through Unity APIs, restores fields and references, and releases lifecycle callbacks after the graph is ready.

Heavy content uses Unity's native AssetImportWorker. Go creates a compatible `SourceAssetDB` containing source identities, paths, hashes, and importer settings. The worker reads this database and writes import artifacts into its own `Unity.DataStore`. Go verifies those artifacts and publishes a runtime content cache. The Player consumes prepared manifests, DLLs, and content files. It does not read `SourceAssetDB`.

Scene and authored prefab iteration uses our loader. It does not require rebuilding scene AssetBundles. Some unchanged native content and compiled shaders still come from previously verified Unity output.

## Repository layout

This is a standalone Git repository. The tool, shared runtime package, minimal host project, and regression tests live here. Target games and their asset caches live in separate directories.

- `unity-micro-editor` builds and launches the Go CLI. `unity-play-go` is an alias.
- `GoProject/` prepares source graphs and scene revisions.
- `WorkerProbe/` contains the runner, importer coordinator, native artifact readers, and Go tests.
- `Packages/com.unitymicroeditor.runtime/` is the shared Unity package containing the loader, runtime bridge, Editor utilities, and EditMode tests.
- `HostProject/` is a minimal Unity project that references this package, engine modules, SRP Core `17.6.0`, and Collections `6.6.0`. It contains no Dungeon gameplay or ship assets.
- `LifecycleCompiler/` instruments lifecycle callbacks using SDK Roslyn.
- `NetcodePostProcessor/` runs matching Netcode package code generation outside the Editor.
- `Profiles/dungeon-linux-mono/` records the package versions and dependency resolution used by the tested game.
- `LEGACY_CLI.md` describes the earlier Python prototype. Those scripts remain as research and preparation tools; they are not used by the Go runner.

Assembly names and namespaces such as `Dungeon.CLIPlay` remain unchanged to preserve existing asset GUIDs, compiled references, and runtime contracts. They do not introduce a dependency on Dungeon game assemblies.

## Pinned environment

The machine-readable profile is [WorkerProbe/engine-profile.json](WorkerProbe/engine-profile.json). `./unity-micro-editor version` prints the profile embedded in the executable.

The supported engine is Unity `6000.6.3f1`, revision `45d8eee7de74`, with Linux x64 Mono build support. The tested graphics backend is OpenGL Core on an NVIDIA RTX 4060. The runner rejects a Null renderer and known software renderer names.

Use Go 1.24 or later. The tested toolchain is Go 1.26.0. Both Go modules have `go.mod` and `go.sum`; external dependencies are YAML v3.0.1, XXH3 v1.0.2, and its indirect CPU detection dependency v2.0.9. The launcher builds with `CGO_ENABLED=0`.

C# tooling targets .NET 8 and uses the Unity installation's .NET SDK `8.0.318`, Roslyn, Mono reference assemblies, and Unity source generators. A system `dotnet` installation is unnecessary. Native import also needs the matching `Unity`, `Unity.DataStore`, and inspection tools from that installation.

The verified Dungeon profile includes URP `17.6.0`, Input System `1.20.0`, Netcode for GameObjects `2.13.3`, and VContainer `1.19.0`. The full direct and transitive versions are recorded in the profile snapshots. These are game-profile dependencies; the minimal host does not install all of them automatically.

The tool does not distribute Unity binaries or licensed third-party assets. Install the matching Unity version separately. The minimal host's runtime package requires SRP Core `17.6.0` and Collections `6.6.0`; its package lock also records their resolved dependencies.

## Run a prepared project

From a project that already has a validated host, compiler plan, and native catalog:

```bash
/path/to/UnityMicroEditor/unity-micro-editor run \
  --project /absolute/game-project \
  --baseline /absolute/game-project/Temp/UnityPlay/hybrid-sample/current.json \
  --output /absolute/game-project/Temp/UnityPlay/micro-editor \
  --watch --fps 30
```

`--editor /absolute/Editor/Unity` selects the installed SDK and importer. Its default is `UNITY_EDITOR`, then `$HOME/Unity/Hub/Editor/6000.6.3f1/Editor/Unity`.

The launcher sets `UNITY_MICRO_EDITOR_ROOT` to its own directory. Runtime and compiler helper sources are resolved there, independently of `--project`. If launching a compiled Go executable directly from another directory, set this variable yourself. `UNITY_MICRO_EDITOR_CACHE` selects the launcher build cache; its default is `$XDG_CACHE_HOME/unity-micro-editor`, or `$HOME/.cache/unity-micro-editor`.

Project revisions, native caches, reports, and Player copies stay under the selected project's `Temp`. One output directory belongs to one running session. Ctrl+C stops the runner and its Player.

Run `./unity-micro-editor` from this repository, or add its directory to `PATH`. Existing target-project `Temp/UnityPlay` and `Builds/CLIPlay*` names are retained so caches and reports remain easy to find. Moving the runtime into a local package changes the project's package configuration. Baselines produced before that change must be refreshed; the runner deliberately rejects stale compiler configuration.

See [GO_RUNNER.md](GO_RUNNER.md) for the detailed Dungeon run instructions and [WorkerProbe/MultiplayerProof/README.md](WorkerProbe/MultiplayerProof/README.md) for the ship validation procedure.

## The shared host project

`HostProject` is the source of a reusable empty Player, rather than a copy of a game project. Its build entrypoint creates only missing bootstrap content and shader retention assets. It builds a Linux Mono development Player with managed stripping disabled, engine stripping disabled, background execution enabled, and an OpenGL Core window.

Inspect the build command without launching Unity:

```bash
./build-host.sh --dry-run
```

Build it once using the pinned Editor:

```bash
./build-host.sh --editor /absolute/6000.6.3f1/Editor/Unity
```

The default output is `HostProject/Builds/Linux/UnityMicroEditorHost.x86_64`, with `host-build.json` next to it. Generated assets and build output are ignored. The script refuses to build while another Unity Editor project process is running. Host construction uses the full Editor in batchmode; normal prepared runs and supported imports do not start the main Editor.

One host can serve projects with matching engine and package requirements. A basic host build alone is not yet a complete baseline for a new game. Package DLLs, native assembly registration, render pipeline resources, compiled shaders, and importer profiles must match that game. The Dungeon proof uses its previously prepared package-aware host, not the minimal template's default build.

## Iteration behavior

Small GameObject and Transform edits use patches in the same Player. Supported wider scene or prefab edits reconstruct the scene. Native asset changes use the importer and content cache, then reconstruct the scene. C# changes compile affected assemblies and restart the Player. Additive ship scene edits currently restart the Player and network session.

Measured small scene patches took about 0.6 to 1.3 seconds from source edit to acknowledgement. The two multiplayer Players became ready in 8.33 and 7.72 seconds. Each Player and launcher used about 1.1 GiB PSS; the driver reported 333 MB framebuffer memory per Player at 960 by 540 and 30 FPS. These are measurements from the tested Dungeon workload, not guarantees for other projects.

Fresh import coverage is incomplete. Verified model and audio profiles work, while some UI/font and compressed texture paths require exact warm artifacts. Independent shader compilation and general animation import need more work. Unsupported changes fail before publishing a replacement.

## Porting to another Unity version

Keep the current profile as a reference and add a separately named engine profile. Update the host `ProjectVersion.txt`, the build version guard, SDK requirements, and package locks together. Do not reuse native output across engine versions without verification.

Check worker initialization and message layouts, `SourceAssetDB` LMDB tables, DataStore artifacts and hashes, serialized file layouts and object IDs, runtime content delivery, Mono assembly registration, and lifecycle behavior. These include private formats tied to the pinned version. Changing a version string does not establish compatibility.

Build a fresh host for the new version. Compare imported meshes, audio, UI, and component values against the Editor, then run scene iteration and multiplayer checks. Cache identities must include the new engine and tool inputs. Publish the new profile as supported only after those checks pass. The current code has several explicit `6000.6.3f1` guards that must be ported with the implementation.

## Development checks

```bash
go -C GoProject test ./...
go -C WorkerProbe test ./...
go -C GoProject vet ./...
go -C WorkerProbe vet ./...
```

For SDK-only runtime checks, set `UNITY_GO_LIFECYCLE_SDK` and `UNITY_GO_DECLARATION_SDK` to the matching `Editor/Unity` path when running the worker tests. These use the installed C# SDK, without starting the Editor. The standalone C# regression projects also live in this directory.

Optional Netcode and real native-model integration checks also need `UNITY_MICRO_EDITOR_TEST_PROJECT` pointing to a separately prepared game project. They do not infer a game directory from the repository's location.

See [CONTRIBUTING.md](CONTRIBUTING.md) for checks and [Integrations/Dungeon/README.md](Integrations/Dungeon/README.md) for connecting the existing game checkout to this repository.
