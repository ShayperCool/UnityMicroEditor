# Unity Micro Editor

Unity Micro Editor is an experimental Rust CLI and MCP server for editing and iterating on a Unity project through a reusable Unity Mono Player, without keeping the Unity Editor open. It reads authored scenes, prefabs and metadata, compiles project code, prepares supported imported content, and updates a running scene. Agents can prepare a project, edit source, control simulation and input, read runtime state, and call project methods marked with C# attributes. Unity's runtime supplies rendering, physics, audio and the PlayerLoop.

It runs on Linux with one pinned Unity version. Coverage of Editor features is partial: unsupported content fails explicitly instead of being approximated, and a matching native host and some native content still need a one-time Editor bootstrap.

## How it works

- **Scenes and prefabs.** The tool reads `.unity`, `.prefab`, `.meta`, ScriptableObjects and their dependencies. A C# loader inside the Player builds the scene graph through Unity APIs, restores fields and references, and releases lifecycle callbacks once the graph is ready. Scenes are not baked into AssetBundles.
- **Code.** The tool resolves packages and derives the Player assembly graph from `.asmdef`/`.asmref` files. Roslyn from the Unity installation's SDK compiles the assemblies. In a watched session a saved C# edit is patched into the running Player where that can be done exactly, and the game keeps its state; other edits are named with their reason and need a restart of the Player ([C# hot reload](docs/HOT_RELOAD.md)).
- **Heavy content.** Models, audio, video, textures, text assets and input actions go through Unity's own AssetImportWorker. The coordinator writes a compatible `SourceAssetDB`, verifies the worker's artifacts, and publishes a runtime content cache. A `.shader` of a built-in pipeline project is parsed by the worker and compiled with the engine's own `UnityShaderCompiler`. Some unchanged native content and other compiled shaders come from previously verified Unity output.
- **Host.** A Mono Player built once per engine and package profile runs all of the above.

## Requirements

- Linux x64.
- Unity `6000.6.3f1` (revision `45d8eee7de74`) with Linux Mono build support, installed separately. The tool does not distribute Unity binaries.
- Rust `1.99.0`; `rustup` installs it from `rust-toolchain.toml`. No system `dotnet` is needed: C# tooling uses the .NET SDK `8.0.318` inside the Unity installation.
- A hardware GPU. The tested backend is OpenGL Core on NVIDIA; a Null or software renderer is rejected.

`./unity-micro-editor version` prints the embedded [engine profile](engine-profile.json) with the exact pinned versions.

## Quick start

`unity-micro-editor` in the repository root builds the CLI with `cargo build --release` into a cache and runs it. Call it by path or add the directory to `PATH`.

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
| `project-import`, `native-adopt`, `native-environment` | Import selected assets with the native worker and manage the import cache. |
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

One host serves projects whose engine and package requirements match it. A project using URP, Input System, Netcode or other packages needs a host built with them: package DLLs, native assembly registration, render pipeline resources and compiled shaders must match. `auto-prepare` builds such a host in an isolated copy of the target project, once: later preparations find and reuse it, and `--rebuild-host` forces a new one. Preparation does not edit the target's `Packages/manifest.json` and runs no Editor in the target. It accepts only hosts built this way, because it records a source profile next to the Player; the plain `build-host.sh` output cannot be passed to it as `--host-player`. Building a host is the one step that runs the full Editor, in batch mode.

Preparation and sessions do not need the package in the game project. To use the runtime in a game project directly, or to run `project_build` in the project's own Editor, reference the package from the game's `Packages/manifest.json` yourself, with a path relative to that `Packages` directory:

```json
"com.unitymicroeditor.runtime": "file:../../UnityMicroEditor/Packages/com.unitymicroeditor.runtime"
```

## Limits

- Small GameObject and Transform edits patch the running Player. Wider scene and prefab edits rebuild the scene. C# changes recompile the affected assemblies; changed method bodies, added non-virtual methods and added fields of classes are patched into the running Player. Every other C# change (a new type, generic code, a new Unity message and the like) needs a restart of the Player: `run` restarts it by itself unless told to wait, and an MCP session reports `full_recompile_required` until `hot_reload` recompiles or the edit is reverted (with `onUnsupportedEdit: "restart"` it restarts the Player by itself). Hot reload runs only in a Mono development Player on Linux x86-64, without JIT inlining. [docs/HOT_RELOAD.md](docs/HOT_RELOAD.md) has the rules and the limits.
- Fresh import works for FBX, OBJ, COLLADA and Blender models, VP8 `.webm` video, `.inputactions`, and the audio, texture and text source formats and importer settings listed in [docs/NATIVE_IMPORT.md](docs/NATIVE_IMPORT.md): textures in the desktop formats including BC7, with platform overrides, as sprites, normal maps, cubemaps, arrays and volumes, and importer-based sprite atlases (`.spriteatlasv2`) with the sprites they pack. UI and font paths, and texture formats of other platform families, need exact artifacts from an existing Unity import. Video in any other codec or container is not imported on Linux, by this tool or by the Editor, and a `.blend` is converted by Blender, which has to be on `PATH`. Shaders are compiled independently only for the built-in render pipeline, within the limits listed there; general animation import is not done.
- `build-project` and the MCP `project_build` tool proxy to the real Editor; they are not an independent build pipeline.
- Editor windows, custom inspectors and EditMode test execution are out of scope.

## Repository layout

- `unity-micro-editor`: launcher. `build-host.sh`: host Player build.
- `Cargo.toml`, `rust/`: the CLI crate; [rust/README.md](rust/README.md) describes its modules.
- `rust/src/runner/`, `native/`, `compiler/`, `sourcegraph/`, `agent/`: runner, import coordinator, compiler driver and agent commands.
- `rust/src/project/`: source graph and scene revision preparation. `authoring/` holds revision-checked source edits and `packages/` package resolution.
- `rust/src/mcp/`, `agentcontrol/`, `editorbridge/`, `runtimeartifact/`: MCP server, Player control, Editor bridge client and native artifact readers.
- `engine-profile.json`: the pinned engine profile embedded in the executable.
- `Packages/com.unitymicroeditor.runtime/`: the Unity package with the loader, runtime bridge and Editor utilities.
- `HostProject/`: the minimal host project.
- `tools/`: C# helpers run through the SDK: lifecycle instrumentation, Netcode post-processing, the importer that saves material variants as their inspector leaves them, and the two programs of C# hot reload (`HotReloadPatcher` builds a patch, `HotReloadRuntime` applies it inside the Player).
- `tests/`: standalone C# regression projects. Rust unit tests sit next to their code, integration tests in `rust/tests/`.
- `Samples/RawProject/`: a small authored project used as a fixture.
- `Integrations/EditorParity/`: an isolated Unity Editor reference and the `parity-*` commands for comparing it with the Player.

## Porting to another Unity version

Add a separately named engine profile and keep the current one as a reference. Update the host `ProjectVersion.txt`, the build version guard, SDK requirements and package locks together. Do not reuse native output across engine versions.

The worker protocol, `SourceAssetDB` tables, DataStore layout, serialized file layouts, content delivery and Mono assembly registration are private formats tied to the pinned version. Each needs to be checked again; changing a version string does not establish compatibility. The code has explicit `6000.6.3f1` guards that must be ported with it.

## Development

```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

The tests use synthetic fixtures and run without Unity. Checks that need the installed SDK or a real project are described in [CONTRIBUTING.md](CONTRIBUTING.md).
