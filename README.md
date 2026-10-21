# Unity Micro Editor

Unity Micro Editor is an experimental Rust CLI and MCP server for editing and iterating on a Unity project through a reusable Unity Mono Player, without keeping the Unity Editor open. It reads authored scenes, prefabs and metadata, compiles project code, prepares supported imported content, and updates a running scene. Agents can prepare a project, edit source, control simulation and input, read runtime state, and call project methods marked with C# attributes. Unity's runtime supplies rendering, physics, audio and the PlayerLoop.

It runs on Linux with one pinned Unity version. Coverage of Editor features is partial: unsupported content fails explicitly instead of being approximated, and a matching native host and some native content still need a one-time Editor bootstrap.

## How it works

- **Scenes and prefabs.** The tool follows a scene's `.unity`, `.prefab`, `.meta`, ScriptableObject and other dependencies by GUID and has Unity's standalone AssetImportWorker import them and write the scene in the Player format with the Editor's own build code, without the Editor. The Player loads that scene with the engine's loader, and the engine runs the lifecycle. Scenes are not baked into AssetBundles. A scene edit writes the scene again and reloads it in the running Player ([docs/NATIVE_IMPORT.md](docs/NATIVE_IMPORT.md#scenes-in-the-player-format)).
- **Code.** The tool resolves packages and derives the Player assembly graph from `.asmdef`/`.asmref` files. Roslyn from the Unity installation's SDK compiles the assemblies. In a watched session a saved C# edit is patched into the running Player where that can be done exactly, and the game keeps its state; other edits are named with their reason and need a restart of the Player ([C# hot reload](docs/HOT_RELOAD.md)).
- **Content and shaders.** Models, audio, video, textures, UI and the rest are imported by that worker in the scene's context. The coordinator writes a compatible `SourceAssetDB`, keeps the worker alive between edits and keeps its imports for the project. Shaders are built on their own and published beside the scene: a `.shader` of a built-in pipeline project is parsed by the worker and compiled with the engine's own `UnityShaderCompiler`, and in a render pipeline project the Editor's build code writes shaders, shader graphs and compute shaders in a worker.
- **Host.** A Mono Player built once per engine and package profile runs all of the above.

## Requirements

- Linux x64.
- Unity `6000.6.3f1` (revision `45d8eee7de74`) with Linux Mono build support, installed separately. The tool does not distribute Unity binaries.
- Rust `1.99.0`; `rustup` installs it from `rust-toolchain.toml`. No system `dotnet` is needed: C# tooling uses the .NET SDK `8.0.318` inside the Unity installation.
- A hardware GPU. The tested backend is OpenGL Core on NVIDIA; a Null or software renderer is rejected.

`./unity-micro-editor version` prints the embedded [engine profile](engine-profile.json) with the exact pinned versions.

## Quick start

`unity-micro-editor` in the repository root builds the CLI with `cargo build --profile fast` (optimized, rebuilt in seconds after an edit) into a cache and runs it. Call it by path or add the directory to `PATH`.

Prepare a project and scene. This resolves packages, builds a package-aware host if none matches, compiles the code and prepares native content:

```bash
unity-micro-editor auto-prepare \
  --project /absolute/project \
  --scene Assets/Scenes/Main.unity \
  --request-id initial
```

Run the scene and keep it updated while you edit, using the baseline path the previous command reports:

```bash
unity-micro-editor run \
  --project /absolute/project \
  --baseline "$baseline" \
  --output /absolute/project/Library/UnityMicroEditor/Sessions/dev \
  --watch
```

Or connect an agent over MCP. Without `--baseline`, the server prepares the scene when the session starts:

```bash
unity-micro-editor mcp \
  --project /absolute/project \
  --scene Assets/Scenes/Main.unity \
  --output /absolute/project/Library/UnityMicroEditor/Sessions/agent
```

Guides:

- [docs/RUNNER.md](docs/RUNNER.md): the `run` command, how each kind of change is applied, events and limits.
- [docs/HOT_RELOAD.md](docs/HOT_RELOAD.md): C# hot reload: what is applied live and what is refused, the API for game code, the agent flow, measured timings and limits.
- [docs/AGENT_MCP.md](docs/AGENT_MCP.md): MCP configuration, tools, source edits, project tools, simulation and input.
- [docs/NATIVE_IMPORT.md](docs/NATIVE_IMPORT.md): the import worker coordinator and its commands.
- [docs/HOSTLESS_PLAYER.md](docs/HOSTLESS_PLAYER.md): what a host build produces, and the experiment toward a Player without one.

## Commands

| Command | Purpose |
| --- | --- |
| `run` | Start a prepared scene in the Player, optionally watching sources. |
| `mcp` | Serve the agent tools over stdio or local HTTP. |
| `auto-prepare` | Prepare a project and scene end to end. |
| `build-project` | Build the project through the real Unity Editor's `BuildPipeline`. |
| `resolve-packages` | Resolve the project's Unity packages without the Editor. |
| `cache-component-defaults` | Capture native component defaults once, for Editor-free component attachment. |
| `project-import`, `native-environment` | Import selected assets with the native worker; print the import environment fingerprint. |
| `source-graph`, `compiler-source-prepare`, `compiler-prepare`, `prepare-project`, `engine-profile-capture` | Individual preparation steps, for diagnostics. |
| `version` | Print the pinned engine profile. |

Every command takes an explicit `--project`. `--editor` selects the Unity installation; its default is `UNITY_EDITOR`, then `$HOME/Unity/Hub/Editor/6000.6.3f1/Editor/Unity`.

## Where output goes

The tool writes preparations, native staging and session state under the target project's `Library/UnityMicroEditor`, and builds under `Builds/UnityMicroEditor`. A project's `Temp` is also accepted for session output, but Unity clears it when the Editor restarts. Other locations, parent traversal and symlink escapes are refused. Authored files and `.meta` GUIDs are changed only by explicit source edits.

The launcher sets `UNITY_MICRO_EDITOR_ROOT` to its own directory; runtime and compiler helper sources are resolved from there, independently of `--project`. Set the variable yourself if you run a compiled executable directly. `UNITY_MICRO_EDITOR_CACHE` selects the launcher's build cache (default `$XDG_CACHE_HOME/unity-micro-editor`, or `$HOME/.cache/unity-micro-editor`); `CARGO_TARGET_DIR` overrides it.

## The host Player

`HostProject` is the source of a minimal reusable Player. It references the runtime package, engine modules, SRP Core `17.6.0` and Collections `6.6.0`, and contains no game content. The build is a Linux Mono development Player with stripping disabled, background execution enabled and an OpenGL Core window.

```bash
./build-host.sh --dry-run                                    # show the command
./build-host.sh --editor /absolute/6000.6.3f1/Editor/Unity   # build
```

The output is `HostProject/Builds/Linux/UnityMicroEditorHost.x86_64` with `host-build.json` beside it. The script refuses to run while another Editor has `HostProject` open.

One host serves projects whose engine and package requirements match it. A package assembly the host lacks, from a registry, built-in or Git package, is built for it during preparation as the Editor builds it, post-processed by Unity's own IL post-processor runner; a URP project was run this way on a host built without URP ([docs/HOSTLESS_PLAYER.md](docs/HOSTLESS_PLAYER.md)). Native plugins, Burst AOT output and Addressables content still come from the host. `auto-prepare` builds such a host in an isolated copy of the target project, once: later preparations find and reuse it, and `--rebuild-host` forces a new one. Preparation does not edit the target's `Packages/manifest.json` and runs no Editor in the target. It accepts only hosts built this way, because it records a source profile next to the Player; the plain `build-host.sh` output cannot be passed to it as `--host-player`. Building a host is the one step that runs the full Editor, in batch mode. A host's settings do not have to match: preparation and watched sessions write the Player's settings managers (tags and layers, physics, time, quality, input and the rest) from the project's `ProjectSettings` over the host's, and a package missing from the host is compiled like game code when it needs no IL post-processing ([docs/HOSTLESS_PLAYER.md](docs/HOSTLESS_PLAYER.md)).

Preparation and sessions do not need the package in the game project. To use the runtime in a game project directly, or to run `project_build` in the project's own Editor, reference the package from the game's `Packages/manifest.json` yourself, with a path relative to that `Packages` directory:

```json
"com.unitymicroeditor.runtime": "file:../../UnityMicroEditor/Packages/com.unitymicroeditor.runtime"
```

## Limits

- Scene, prefab and asset edits write the scene again and reload it in the running Player; the scene's state starts over. C# changes recompile the affected assemblies; changed method bodies, added non-virtual methods and added fields of classes are patched into the running Player. Every other C# change (a new type, generic code, a new Unity message and the like) needs a restart of the Player: `run` restarts it by itself unless told to wait, and an MCP session reports `full_recompile_required` until `hot_reload` recompiles or the edit is reverted (with `onUnsupportedEdit: "restart"` it restarts the Player by itself). Hot reload runs only in a Mono development Player on Linux x86-64, without JIT inlining. [docs/HOT_RELOAD.md](docs/HOT_RELOAD.md) has the rules and the limits.
- Assets are imported by the engine's own importers in the worker; [docs/NATIVE_IMPORT.md](docs/NATIVE_IMPORT.md) lists which formats and settings were compared with the Editor's artifacts. Writing a scene needs an Editor compilation of the project, which a preparation makes or finds. The Player keeps the host's render pipeline global settings and built-in resources. Video other than VP8 `.webm` yields no clip on Linux, by this tool or by the Editor, and a `.blend` is converted by Blender, which has to be on `PATH`. Shaders are delivered within the limits listed there.
- `build-project` and the MCP `project_build` tool proxy to the real Editor; they are not an independent build pipeline.
- Editor windows, custom inspectors and EditMode test execution are out of scope.

## Repository layout

- `unity-micro-editor`: launcher. `build-host.sh`: host Player build.
- `Cargo.toml`, `rust/`: the CLI crate; [rust/README.md](rust/README.md) describes its modules.
- `rust/crates/runner/src/runner/`, `native/`, `compiler/`, `sourcegraph/`, `agent/`: runner, import coordinator, compiler driver and agent commands.
- `rust/crates/native/src/engine/`: the supported Unity releases and each one's worker, artifact storage and asset database protocols.
- `rust/crates/base/src/project/`: source graph and scene revision preparation. `authoring/` holds revision-checked source edits and `packages/` package resolution.
- `rust/crates/mcp/src/mcp/`, `agentcontrol/`, `editorbridge/`, `runtimeartifact/`: MCP server, Player control, Editor bridge client and the reader of built-in lighting data.
- `engine-profile.json`: the pinned engine profile embedded in the executable.
- `Packages/com.unitymicroeditor.runtime/`: the Unity package with the runtime bridge that loads the worker-written scene, the agent controls and Editor utilities.
- `HostProject/`: the minimal host project.
- `tools/`: C# helpers run through the SDK: the lifecycle compiler (start-up bridges for `[RuntimeInitializeOnLoadMethod]`, type and declaration facts), Netcode post-processing, the importer the import worker runs to write scenes and shaders (`WorkerScripts`), and the two programs of C# hot reload (`HotReloadPatcher` builds a patch, `HotReloadRuntime` applies it inside the Player).
- `tests/`: standalone C# regression projects. Rust unit tests sit next to their code, integration tests in `rust/tests/`.
- `Samples/RawProject/`: a small authored project used as a fixture.
- `Integrations/EditorParity/`: an isolated Unity Editor reference and the `parity-*` commands for comparing it with the Player.

## Porting to another Unity version

Add a separately named engine profile and keep the current one as a reference. Update the host `ProjectVersion.txt`, the build version guard, SDK requirements and package locks together. Do not reuse native output across engine versions.

The worker protocol, `SourceAssetDB` tables, DataStore layout, serialized file layouts, content delivery and Mono assembly registration are private formats tied to the pinned version. Each needs to be checked again; changing a version string does not establish compatibility.

The worker side is already split by release: `rust/crates/native/src/engine/` holds one implementation of the worker messages, the artifact storage (the DataStore of 6000.6, the files a 6000.0 worker writes itself) and the asset databases per supported release, a project's `ProjectVersion.txt` selects it, and the installation has to report the same version ([docs/NATIVE_IMPORT.md](docs/NATIVE_IMPORT.md#unity-releases)). `6000.0.71f1` has one, verified with `project-import`. A new release gets a module of its own there. The other formats still have explicit `6000.6.3f1` guards that must be ported with it.

## Development

```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

The tests use synthetic fixtures and run without Unity. Checks that need the installed SDK or a real project are described in [CONTRIBUTING.md](CONTRIBUTING.md).
