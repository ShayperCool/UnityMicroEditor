# Running a project

`unity-micro-editor run` starts an authored scene in a reusable Unity Mono Player and, with `--watch`, applies source changes to it. The runner prepares the scene graph, caches and Player launch. SDK Roslyn compiles C#. Unity's runtime does the rendering, physics and audio. The main Unity Editor is not started during a run.

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
| `--no-hot-reload` | With `--watch`: restart the Player for every C# change instead of patching the running one. |
| `--hot-reload-fallback restart\|report` | With `--watch`: what a C# change that cannot be patched does. `restart` (default) restarts the Player on the new code. `report` keeps the Player on its code, reports the reasons and waits for a `recompile` command; it requires `--commands-stdin`. Cannot be combined with `--no-hot-reload`. |
| `--commands-stdin` | With `--watch`: read commands from standard input, one JSON object per line. See [Commands](#commands). |
| `--interactive` | Keep the Player window open without a watcher. |
| `--frames` | Frame count in finite mode. |
| `--width`, `--height`, `--fps` | Window size and frame cap. Defaults: 960, 540, 60. |
| `--control` | Enable revision-checked agent commands in an interactive Player. |
| `--prepare-only` | Write immutable source, content and managed manifests without starting a Player. |
| `--prepared` | Launch from such a descriptor. Cannot be combined with source preparation or `--watch`. |
| `--content-scene GUID=Assets/...unity` | Register an additional scene the game loads at runtime. Repeatable. |
| `--content-prefab Assets/...prefab` | Register a prefab the game addresses at runtime. Repeatable. |
| `--no-addressables` | Do not serve the project's Addressables keys from sources; leave them to the host's built content. |
| `--builtin-shader` | Verified snapshot of a retained built-in shader. Repeatable. |

The addressable scenes and prefabs of the project's Addressables groups are registered automatically and resolved from sources in the Player; see [AGENT_MCP.md](AGENT_MCP.md#addressables-from-sources) for what that covers.

Arguments after `--` are passed to the game. Through MCP the same options are the `gameArguments`, `contentScenes`, `contentPrefabs` and `addressables` of `session_control(start)`; its `hotReload: false` is `--no-hot-reload` and its `onUnsupportedEdit` is `--hot-reload-fallback`, which the MCP server sets to `report` unless told otherwise. A rejected update whose error needs a new source graph carries `preparationRequired: true` in its `update-rejected` event: the Player keeps its old code until the project is prepared again.

Prefer an output under `Library/UnityMicroEditor`: Unity clears a project's `Temp` when its Editor restarts. One output directory belongs to one running session. A second run with the same output is refused until the first stops; the lock is released on exit or crash. Reusing an output reuses its compiler and native caches.

## Time limits of a start

Every step of a start has a limit of its own, counted from the moment the step begins; no limit spans the run, so a shader that takes ten minutes to build the first time leaves the steps after it their full time. A step that runs out fails under its own name with the limit in the message.

| Step | Limit | When it runs out |
| --- | --- | --- |
| Build of a shader, shader graph or compute shader by the Editor's code in a worker | 30 min a build | the session fails with the worker's error |
| One run of the import worker (a batch of up to 32 models, textures and the like; some 7 s) | 45 s for the context, the imports, the stop of the worker's store, the extraction and the check that no source changed | the batch is run once more with 90 s and `native-import-repeated` is reported; a second failure is `native import of ...: the import ran out of its 90s budget ...`. The same repetition follows a run whose `Unity.DataStore` did not stop by itself and had to be killed. |
| Player start, from its launch to its ready report (some 5 s) | 45 s for a Player that got to run. One that has had less than 30 s of processor time by then is waited for until it has had as much, at most 300 s from its launch | `Player start: not ready within its 45s budget (it had 31.2s of processor time); see <report>`, or `Player start: not ready within 300s, the limit for a Player that hardly gets to run (budget 45s; it had 0.4s of processor time); see <report>` |
| Reload acknowledgement in a watched session | 20 s | `update-rejected` |
| Code patch acknowledgement in a watched session | 20 s, or the moment the Player exits | the Player is restarted on the newest compiled code: `code-updated` with `reason: "hot_reload_failed"` |

The two extensions exist because these limits were measured on an idle machine and have failed on a busy one: while another build ran on the same machine, the check after an import that takes 10 ms took 31 s and a Player that normally writes the first line of its log within half a second had not written it after 45 s. Neither says anything about the project. The limits still bound a start: an import is repeated once, and a Player that uses its processor time without becoming ready fails after 45 s as before. A Player that is blocked without using processor time is given up only after 300 s; the message then says how little it ran.

The runner may be started without a terminal and with SIGINT ignored, as a shell does for a command it puts in the background (`nohup ... &` in a script, a service manager). It handles SIGINT and SIGTERM itself, and the Player it starts gets the default handling of both whatever the runner inherited, so stopping a session interrupts the Player and does not wait for the escalation to SIGTERM.

The runner asks for OpenGL Core with VSync off. Its readiness check rejects a missing or Null renderer and known software renderers (llvmpipe, softpipe, lavapipe, SwiftShader). This is a check of known names, not proof of hardware rendering on every vendor.

## How changes are applied

| Change | What happens |
| --- | --- |
| GameObject name, active state, layer or tag; Transform position, rotation or scale | A small scene patch is published. Changed source fields are applied; other runtime values are kept. |
| New empty GameObject with a Transform, child or root | The patch adds the object and updates the hierarchy in the same Player. |
| Other supported scene, prefab and raw asset changes | The loader rebuilds the scene in the same Player. Scene state resets; loaded assemblies and the native cache remain. |
| Shader (`.shader`), a file it includes, a material's keywords | The worker parses the shader, `UnityShaderCompiler` compiles its programs and the runner publishes the Player's object; the shaders that fall back on it or use its passes follow. The scene is rebuilt in the same Player. [NATIVE_IMPORT.md](NATIVE_IMPORT.md#shaders) lists what is refused. |
| Material (`.mat`) | The scene is rebuilt with a material made from the source. In a render pipeline project the shader it names is built with the project's materials, also when it is a shader of a package; a change of keywords builds it again. [NATIVE_IMPORT.md](NATIVE_IMPORT.md#materials-in-a-session) says what is set and what is refused. |
| Supported model, audio, video, texture, text or input actions asset | Unity's AssetImportWorker imports the file in an owned context. The runner adapts the result and loads it into the same Player, then the scene is rebuilt. |
| C# change that can be patched: changed method bodies, added non-virtual methods, added fields of classes | The SDK recompiles the changed assemblies and the new code is patched into the running Player. The process, the scene and all game state stay. See [Hot reload of C#](#hot-reload-of-c). |
| C# change that cannot be patched: a new type, a generic method, a new Unity message and the like | With `--hot-reload-fallback restart` the Player restarts on the new code; process and scene state reset. With `report` the Player keeps its code and the change waits for a `recompile` command. |
| C# change with `--no-hot-reload` | The SDK recompiles the changed assemblies, dependent assemblies as well where an API changed, and the runner restarts the Player. Process and scene state reset. |

The watcher starts before the Player is prepared, so edits made during startup are picked up. Prepared JSON revisions and DLLs are stored apart from the sources. The compiler checks the SDK, source hashes and assembly configuration. A native replacement checks source and metadata hashes, the GUID, and exact local IDs and types.

An inconsistent or unsupported change produces `update-rejected`, and the last confirmed revision keeps running. A C# compile error, for example, leaves the current Player untouched.

## Hot reload of C#

In a watched session a C# change is applied to the running Player without a restart where that can be done exactly. Nothing is approximated: a change that cannot be patched is named with its reason, and only a restart on the new code applies it.

After a successful compile the runner compares the hash of every game assembly with the one the Player loaded at its start. For those that differ it runs the patch builder (`tools/HotReloadPatcher`), which compares what the two compilations mean and writes one patch assembly per changed assembly. The runner then asks the Player to apply them. The applier (`tools/HotReloadRuntime`), loaded into the Player by the bridge, redirects the changed methods to the patch. A patch always holds every difference from the loaded assemblies, so reverting an edit restores the loaded code and a later edit replaces the patch before it.

| Outcome | What the runner does | Event |
| --- | --- | --- |
| The patch was applied | Nothing else. Same Player, same scene revision. | `code-patched` |
| The new compilation means the same as the loaded assemblies and no patch is applied (a comment, a refused edit that was reverted) | Nothing is sent to the Player. | `unchanged` or `updated` of the scene part, with `fullRecompileRequired: false` |
| No body differs, but the Player does not take over all of the change (the accessibility of a field or method, a removed `[RuntimeInitializeOnLoadMethod]`) | Nothing is sent to the Player. | The same event, with the builder's `notes`, `notesCount` and `hotReloadReport` |
| The same while a patch is applied (a patched edit that was reverted) | The Player restores the methods the patch redirected. | `code-patched` with `restoredMethods` and no `assemblies` |
| The change cannot be patched, or the Player refused the patch before it changed anything | The fallback: restart, or report. | `code-updated` with `reason: "hot_reload_unsupported"`, or `update-rejected` with `fullRecompileRequired: true` |
| The Player says its code is in an unknown state, exits during the patch or does not answer in 20 s | Restart on the newest code, in both fallback modes. | `code-updated` with `reason: "hot_reload_failed"` and `error` |

Which edits are patched is decided by the patch builder alone; its README lists them and `dotnet HotReloadPatcher.dll --codes` prints every reason and note code. In short: changed bodies of non-generic methods of non-generic types (including property and event accessors, instance constructors, lambdas, iterators and `async` methods), added non-virtual methods, added instance fields of classes and added static fields without an initializer are patched. Added, removed or renamed types, generic code, added virtual methods and Unity messages, changed static constructors, attribute changes and assemblies processed by Netcode are refused. A refusal lists `{code, member, message}` for every refused member. Three more codes come from the runner: `patch_builder_failed` (the builder failed or its result was unusable; the message is its output), `apply_failed` (the Player refused the patch and changed nothing; the message starts with the applier's stage, `resolve`, `inlining`, `compile` or `write`, or is the bridge's own message when the applier did not run) and `assembly_not_loaded` (the compiler graph has an assembly the running Player did not load).

An applied patch can carry notes, for example `existing_instances_keep_default` for an added field. `callback_failed` says that a `[InvokeOnHotReload]` method of the game threw or could not be called; the patch is applied all the same. A change that needs no patch can have notes as well: a field made `public` compiles to the same bodies, and the Player keeps the old accessibility for reflection and Unity serialization until it restarts (`accessibility_changed`). Such notes are on the `unchanged` or `updated` event of that window and on every later one, also on the `update-rejected` of a scene part, until a compile without them, a patch, a refusal or a restart.

The runner trusts an acknowledgement only as far as it can check it: the Player's process, the generation, and the number of redirects, which must be the number the patch lists. An acknowledgement of success with another number restarts the Player (`hot_reload_failed`). The patch builder gets 60 s for one run; one that does not end by then is stopped and the change is refused with `patch_builder_failed`.

In a window with both a C# and a scene change the code is patched first and the scene is then updated on the same Player. A scene manifest and the content routes keep naming the assemblies the Player loaded, which the Player checks on every scene reload. Script types, behaviour layouts and runtime initializers of a newer compile are used only by the process that loads it, so they wait for the next restart.

The events of a window are printed together when its work is done: `code-patched` first, then the scene part (`updated`, or `update-rejected` with its own error), and with `--hot-reload-fallback report` the refusal as the last one. A reader that acts on `code-patched` therefore finds no step of that window still running. An event that is not the window's last carries `pending: true`. So does every event of a window that ends while source changes are queued which it did not take: the next window handles them, and its events are the answer for whoever saved them. A window always ends with at least one event, so a `pending` one is always followed by another, unless the session ends.

`updated` and `unchanged` carry `fullRecompileRequired: true` while a refused change waits, also for later scene edits, which keep working on the same Player. A scene part that is rejected while a refused change waits, before the Player or by it, says the same on its `update-rejected` and repeats the reasons beside its own `error`: a scene that uses a type only the refused code has fails for that reason alone, and the recompile is what applies it. Without a refusal the rejection of a scene part says `fullRecompileRequired: false`, so that a refusal which the same window took back does not outlive it. The refusal is printed again after every compile that still cannot be patched. It ends with a patch, with a compile that means what the Player runs, or with a restart.

Whatever restarts the Player (the fallback, a `recompile` command, changed content routes) starts it on the newest compiled code. Code generations are counted per Player process and start at 1.

The Player is started with `MONO_ARGUMENTS=-O=-float32,-inline`, because a redirect does not reach the copies an inlining JIT makes of small methods. A value the variable already has in the runner's environment is passed on unchanged; if it leaves inlining on, every patch is refused with `apply_failed` and the stage `inlining`.

Both programs are built from the tool's sources with the SDK of the Unity installation, the first time while the Player starts: the patch builder into `<output>/hot-reload/patcher/<key>/`, the applier into `<output>/hot-reload/applier/<key>/UnityMicroEditor.HotReload.dll`. A Player process keeps the applier it was started with. A session in which either build fails prints `hot-reload-unavailable` with the error, says `hotReload: false` in `ready` and restarts the Player on C# changes.

### Measured

On 2026-10-13, on a copy of `Samples/RawProject` with a few more scripts (one game assembly of ten source files; AMD Ryzen 7 5700X3D; other builds ran on the machine), eight edits of one method body in a row took 3.1 s at best and 3.4 s in the median from the save to the Player's acknowledgement (`sourceToAckSeconds`): 3.1 s of it the compile step (`compileSeconds`) and 0.22 s the patch (`patchSeconds`). In an earlier run of eight, one edit took 13.3 s, 11.9 s of it in the compile. The same edit with `--no-hot-reload` took 6.6 s to a ready Player (`sourceToReadySeconds`, median of five) and lost the game's state.

Where the 3.6 s of an edit went in a traced run, mean of six:

| Step | Seconds |
| --- | --- |
| From the save to the first compiler process: the 35 ms the watcher waits for more changes, the source hashes and the hash of the Player directory that keys the code revision | 0.54 |
| Lifecycle rewrite (`tools/LifecycleCompiler`) | 0.70 |
| Roslyn | 0.96 |
| Script type and declaration checks, two more runs of the helper | 0.22 |
| Last check of the sources and the copy of the Player directory | 0.13 |
| Publishing the DLLs and hashing the new Player directory for the revision cache | 0.85 |
| Patch builder | 0.22 |
| Request, acknowledgement and event | 0.03 |

The applier itself needs 2 to 4 ms for such a request and 10 to 30 ms for the first one of a Player process. `compileSeconds` of `code-patched` is the whole compile step of the window, from the source hashes to the revision cache; with `patchSeconds` and the 35 ms the watcher waits for more changes it adds up to `sourceToAckSeconds`. `compileSeconds` of `code-updated` is the compiler's own time, as before: it ends before the new Player directory is hashed. Sources that the session output has compiled before (an edit that is taken back, a session that repeats edits) are answered from its cache in 0.6 s, and such an edit took 1.9 s. Nearly all of the time is the compile every C# change goes through; the patch adds a quarter of a second to it and saves the restart of the Player. The hash of the new Player directory reads 177 MB that no process has read before: while another build filled the disk queue it took 10 to 27 s instead of 0.7 s, and a restart of the Player, which reads the same files, took 36 s instead of 4 s in one test and more than 160 s in another.

### Commands

With `--commands-stdin` the runner reads one JSON object per line from its standard input:

```json
{"command":"recompile","requestId":"<ASCII letters, digits or hyphens, at most 64>"}
```

`--hot-reload-fallback report` requires `--commands-stdin`: a change that is reported waits for this command. `recompile` checks every source of the compiler graph, compiles what changed and restarts the Player on the newest compiled code, also when nothing changed. The answer is `code-updated` with `reason: "requested"` and the `requestId`. When the check or a later step fails, the Player is kept and the answer is an `update-rejected` with the error and the `requestId`. A command that arrives in the debounce window of a source change is handled with it. One that arrives while the Player is restarting for another reason is answered by that restart, unless a source changed in between.

A line that is no command is reported as `command-rejected` with `error` and the first 256 bytes of the `line`. Neither it nor the end of the input ends the session. No process the session starts inherits the runner's standard input.

## Limits

- A baseline is required. The runner does not turn an arbitrary folder with `Assets` into a running Player on its own.
- Native overlays cover the source formats and importer settings listed in [NATIVE_IMPORT.md](NATIVE_IMPORT.md). Audio covers DecompressOnLoad, CompressedInMemory and Streaming clips, preloaded or not, with or without background loading. Video is VP8 in a `.webm` file, the one form the engine reads on Linux. Unsupported import profiles are rejected.
- An FBX update is accepted when object IDs, hierarchy, component and reference order and authored scalar values are unchanged. Mesh geometry may change. Skinned and Animator layouts that were not verified are refused.
- Changes to project settings, `.asmdef` or package configuration, GUIDs or types need a fresh preparation.
- Wider scene reloads and native updates rebuild the game graph.
- Hot reload exists for a watched session only, and for the verified profile: a development Mono Player on Linux x86-64. The applier refuses other platforms, and the runner then takes the fallback for every C# change.
- A patched Player keeps the script types, behaviour layouts and runtime initializers of the assemblies it loaded. A new script file is a new type and is refused; a changed field initializer of a ScriptableObject or StateMachineBehaviour reaches baked content at the next restart.
- With `--hot-reload-fallback report` the Player runs its old code until a `recompile` command or another restart. A scene that needs a type only the new code has is rejected, with `fullRecompileRequired: true` on its `update-rejected`, and the old revision keeps running until then.
- A restart that was decided but could not happen, for example because an import of the same window failed, is reported as a plain `update-rejected`. The Player keeps its code, and the next change of any kind restarts it.
- What a patch does to running code is the applier's ground: a method that is on a stack finishes its old body, and a coroutine or `async` method that is running keeps the body it started with. Other threads are not stopped; a first redirect replaces the bytes of one instruction. `tools/HotReloadRuntime/README.md` has the details and the measured numbers.
- A stack trace of patched code names the patch's copy of the method (`HotReload.G<generation>.<type>.<method>`) with the file and line of the compile that made it, as a restarted Player does for the same code. The file is the compiler's copy of the source under `<output>/compiler`, not the path under `Assets`.
- A save that arrives while the compiler reads the sources of an earlier one is rejected with `runtime source changed while snapshotting` or `during compilation`, with or without hot reload; the next window compiles the whole graph and applies the last save. The rejection carries `pending: true` when the watcher holds that save by then, and the controller does not report it to a waiting caller.
- `pending` is set from what the watcher has queued at the moment the events are printed. A save that reaches it a moment later is not announced by the events before it.
- The Player of a session with hot reload runs without JIT inlining. Script code that mostly calls small methods is about half as fast as in a session with `--no-hot-reload`: 200 million calls of a one-line static method took 365 ms against 179 ms, and 20 million `Vector3` additions and scalings 582 ms against 249 ms (2026-10-13). Engine code is not affected.
- A Player process keeps every patch assembly it loaded. Its resident memory grew by 17 MB over the first ten patches of a process, most of it once, and by 2.3 MB over the thirty after them.
- A code patch is acknowledged on the Player's main thread. Game callbacks marked `[InvokeOnHotReload]` run inside that time; one that blocks for 20 s makes the runner restart the Player.
- Nothing of a session's hot reload is removed while it runs: every attempt leaves its small directory under the run (3 MB for 54 attempts), as every compile leaves its Player directory. A Player directory of the sample is 177 MB, copied with `cp --reflink`: 60 compiles of one session were 9.4 GB of file sizes and about 2 GB of disk on XFS with reflinks, and are the full size on a filesystem without them. Hot reload makes many compiles a session ordinary; a new session output starts empty.
- Compatibility with every Unity project, and full Editor Play Mode behavior, are not claimed.

## Events and reports

The runner prints one JSON event per line: `prepared`, `ready`, `updated`, `code-patched`, `code-updated`, `content-updated`, `unchanged`, `native-discovery`, `native-imported`, `native-cache-hit`, `native-cache-rejected`, `native-adopted`, `native-warm-artifact`, `native-built`, `native-build-reused`, `native-baked`, `native-import-repeated`, `addressables`, `addressables-skipped`, `addressables-changed`, `hot-reload-unavailable`, `command-rejected` and `update-rejected`.

What hot reload adds to them, in a watched session:

| Event | Members |
| --- | --- |
| `ready` | `hotReload`: whether C# changes are patched into the Player of this session. |
| `code-patched` | `paths`, `pid` and `revision` (the scene revision; a code patch changes neither), `codeGeneration` (1 for the first patch of a Player process), `assemblies`, `patchedMethods`, `addedMethods`, `addedFields`, `restoredMethods`, `members`, `notes` (`{code, member, message}`), `notesCount`, `compileSeconds` (the whole compile step of the window), `patchSeconds` (patch builder to acknowledgement), `sourceToAckSeconds` (from the source change), `report`. `patchedMethods` is the number of redirects the Player acknowledged. |
| `update-rejected` of a change that cannot be patched (`report` mode) | `error` (one sentence with the number of reasons and the first one), `paths`, `fullRecompileRequired: true`, `unsupported` (`{code, member, message}`), `unsupportedCount`, `report`. |
| `update-rejected` that answers a `recompile` command | `error`, `paths`, `requestId`, and `preparationRequired` where the error needs a new source graph. Never `fullRecompileRequired`. |
| `updated`, `unchanged` | `fullRecompileRequired`: `true` while a change that cannot be patched waits for a restart, `false` when the Player runs the newest compiled code. With `false`, `notes`, `notesCount` and `hotReloadReport` while the newest compile needed no patch and still has notes. Absent in a session without hot reload. |
| `update-rejected` of a scene part | Its own `error`, and `ack` when the Player rejected it. While a change that cannot be patched waits (`report` mode): `fullRecompileRequired: true` and the `unsupported`, `unsupportedCount` and `report` of the refusal that stands. Otherwise `fullRecompileRequired: false`, with the notes as on `updated`. A compile error stays plain. |
| every event that answers a window | `pending: true` when it is not the last word about the saved sources: another event of the same window follows, or source changes are queued for the next one. Absent otherwise. |
| `code-updated` | `reason`, absent for a restart hot reload did not cause: `"hot_reload_unsupported"` with `unsupported`, `unsupportedCount` and `hotReloadReport`; `"hot_reload_failed"` with `error` and `hotReloadReport`; `"requested"` with `requestId`. A requested restart is `code-updated` also when no newer compile was loaded; it then has no `sourceHashes`, `compiledAssemblies`, `declarationChanged` and `compileSeconds`. |

`assemblies`, `members`, `notes` and `unsupported` hold at most 16 entries; the counts are complete and the report file has every entry. `report` of `code-updated` remains the Player's report, which is why the hot reload report has its own name there. A C# change that is patched or refused ends its window with that one event; an `unchanged` for the scene is not printed after it.

`<output>/runs/<id>/` holds the Player log, the ready and final reports, and reload requests with their acknowledgements. The ready report records the actual GPU and PID. An acknowledgement carries the revision, the native load count and any errors, and refers to a completed frame.

A run directory of a session with hot reload also holds `code-reload-request.json` with its `.ack.json`, and one directory per patch attempt, `hot-reload/<generation>/`: `request.json` and `result.json` of the patch builder, the patch assemblies, `apply.json` (what the Player was asked to apply), `ack.json` and `report.json`. The report has `schemaVersion`, `generation`, `outcome` (`patched`, `unchanged`, `unsupported`, `apply_failed`, `restart_required`, `player_exited` or `timed_out`), `error`, and the builder's `result` and the Player's `ack` in full. An attempt takes a generation of its own, also when it is refused, so `codeGeneration` can skip numbers.

Native import details are in [NATIVE_IMPORT.md](NATIVE_IMPORT.md).
