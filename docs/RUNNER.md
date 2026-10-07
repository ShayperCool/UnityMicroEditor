# Running a project

`unity-micro-editor run` starts an authored scene in a reusable Unity Mono Player and, with `--watch`, applies source changes to it. Go prepares the scene graph, caches and Player launch. SDK Roslyn compiles C#. Unity's runtime does the rendering, physics and audio. The main Unity Editor is not started during a run.

The runner needs a prepared baseline for the project: a compatible native host, a compiler plan and a native asset catalog. `unity-micro-editor auto-prepare` produces one; see [AGENT_MCP.md](AGENT_MCP.md#start-with-an-explicit-project-and-scene).

## Start

```bash
unity-micro-editor run \
  --project /absolute/project \
  --baseline "$baseline" \
  --output /absolute/project/Library/UnityMicroEditor/Sessions/dev \
  --watch
```

`$baseline` is the `current.json` path reported by `auto-prepare`; preparations live under `Library/UnityMicroEditor/preparations/<request-id>`.

`--watch` keeps the Player open and updates it after project changes. Ctrl+C stops the runner and its Player. Without `--watch` or `--interactive`, the Player renders `--frames` frames (120 by default), reports and exits, which is useful as a check.

| Flag | Meaning |
| --- | --- |
| `--project` | Unity project root. Default: current directory. |
| `--baseline` | Validated preparation `current.json` for this project. Required unless `--prepared` is used. |
| `--output` | Session cache. Must be under the project's `Library/UnityMicroEditor` or `Temp`. Default: `Library/UnityMicroEditor/Sessions/default`. |
| `--editor` | Matching `Editor/Unity` executable, used for its SDK and import worker. Default: `UNITY_EDITOR`, then the pinned version under `$HOME/Unity/Hub/Editor`. |
| `--scene` | Authored `Assets/...unity` scene to run on the prepared host. |
| `--watch` | Watch sources and update the running Player. Implies `--interactive`. |
| `--interactive` | Keep the Player window open without a watcher. |
| `--frames` | Frame count in finite mode. |
| `--width`, `--height`, `--fps` | Window size and frame cap. Defaults: 960, 540, 60. |
| `--control` | Enable revision-checked agent commands in an interactive Player. |
| `--prepare-only` | Write immutable source, content and managed manifests without starting a Player. |
| `--prepared` | Launch from such a descriptor. Cannot be combined with source preparation or `--watch`. |
| `--content-scene GUID=Assets/...unity` | Register an additional scene the game loads at runtime. Repeatable. |
| `--content-prefab Assets/...prefab` | Register a prefab the game addresses at runtime. Repeatable. |
| `--builtin-shader` | Verified snapshot of a retained built-in shader. Repeatable. |

Arguments after `--` are passed to the game.

Prefer an output under `Library/UnityMicroEditor`: Unity clears a project's `Temp` when its Editor restarts. One output directory belongs to one running session. A second run with the same output is refused until the first stops; the lock is released on exit or crash. Reusing an output reuses its compiler and native caches.

The runner asks for OpenGL Core with VSync off. Its readiness check rejects a missing or Null renderer and known software renderers (llvmpipe, softpipe, lavapipe, SwiftShader). This is a check of known names, not proof of hardware rendering on every vendor.

## How changes are applied

| Change | What happens |
| --- | --- |
| GameObject name, active state, layer or tag; Transform position, rotation or scale | A small scene patch is published. Changed source fields are applied; other runtime values are kept. |
| New empty GameObject with a Transform, child or root | The patch adds the object and updates the hierarchy in the same Player. |
| Other supported scene, prefab and raw asset changes | The loader rebuilds the scene in the same Player. Scene state resets; loaded assemblies and the native cache remain. |
| Supported WAV, FBX or PNG with an existing GUID | Unity's AssetImportWorker imports the file in an owned context. Go adapts the result and loads it into the same Player, then the scene is rebuilt. |
| C# method body | The SDK recompiles the changed assembly and the runner restarts the Player. Process and scene state reset. |
| C# API change | Dependent assemblies can be recompiled as well. The Player restarts. |

The watcher starts before the Player is prepared, so edits made during startup are picked up. Prepared JSON revisions and DLLs are stored apart from the sources. The compiler checks the SDK, source hashes and assembly configuration. A native replacement checks source and metadata hashes, the GUID, and exact local IDs and types.

An inconsistent or unsupported change produces `update-rejected`, and the last confirmed revision keeps running. A C# compile error, for example, leaves the current Player untouched.

## Limits

- A baseline is required. The runner does not turn an arbitrary folder with `Assets` into a running Player on its own.
- Native overlays cover GUIDs already in the catalog. WAV is verified for PCM/DecompressOnLoad and preloaded Vorbis/CompressedInMemory with synchronous loading. Streaming and background-loaded audio, new native GUIDs and unsupported import profiles are rejected.
- An FBX update is accepted when object IDs, hierarchy, component and reference order and authored scalar values are unchanged. Mesh geometry may change. Skinned and Animator layouts that were not verified are refused.
- Changes to project settings, `.asmdef` or package configuration, GUIDs or types need a fresh preparation.
- C# edits restart the Player. Wider scene reloads and native updates rebuild the game graph.
- Compatibility with every Unity project, and full Editor Play Mode behavior, are not claimed.

## Events and reports

The runner prints one JSON event per line: `prepared`, `ready`, `updated`, `code-updated`, `content-updated`, `unchanged`, `native-discovery`, `native-imported`, `native-cache-hit`, `native-cache-rejected`, `native-adopted`, `native-warm-artifact` and `update-rejected`.

`<output>/runs/<id>/` holds the Player log, the ready and final reports, and reload requests with their acknowledgements. The ready report records the actual GPU and PID. An acknowledgement carries the revision, the native load count and any errors, and refers to a completed frame.

Native import details are in [NATIVE_IMPORT.md](NATIVE_IMPORT.md).
