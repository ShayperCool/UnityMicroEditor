# Running a project

`unity-micro-editor run` starts an authored scene in a reusable Unity Mono Player and, with `--watch`, applies source changes to it. The runner has Unity's import worker write the scene in the Player format, and prepares caches and the Player launch; the Player loads the scene with the engine's own loader. SDK Roslyn compiles C#. Unity's runtime does the rendering, physics and audio. The main Unity Editor is not started during a run.

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
| `--editor` | Matching `Editor/Unity` executable, used for its SDK and import worker. Default: `UNITY_EDITOR`, then the project's release under `$HOME/Unity/Hub/Editor`. |
| `--scene` | Authored `Assets/...unity` scene to run on the prepared host. |
| `--watch` | Watch sources and update the running Player. Implies `--interactive`. |
| `--no-hot-reload` | With `--watch`: restart the Player for every C# change instead of patching the running one. |
| `--hot-reload-fallback restart\|report` | With `--watch`: what a C# change that cannot be patched does. `restart` (default) restarts the Player on the new code. `report` keeps the Player on its code, reports the reasons and waits for a `recompile` command; it requires `--commands-stdin`. Cannot be combined with `--no-hot-reload`. |
| `--commands-stdin` | With `--watch`: read commands from standard input, one JSON object per line. See [Commands](#commands). |
| `--interactive` | Keep the Player window open without a watcher. |
| `--frames` | Frame count in finite mode. |
| `--width`, `--height`, `--fps` | Window size and frame cap. Defaults: 960, 540, 60. |
| `--screenshot` | Absolute `.png` path for the last frame of a finite run. Such a run opens a window: a Player in `-batchmode` draws no screen to capture. |
| `--control` | Enable revision-checked agent commands in an interactive Player. |
| `--prepare-only` | Write immutable source, content and managed manifests without starting a Player. |
| `--prepared` | Launch from such a descriptor. Cannot be combined with source preparation or `--watch`. |
| `--content-scene GUID=Assets/...unity` | Register an additional scene the game loads at runtime. Repeatable. |
| `--content-prefab Assets/...prefab` | Register a prefab the game addresses at runtime. Repeatable. |
| `--no-addressables` | Do not serve the project's Addressables keys from sources; leave them to the host's built content. |
| `--developer-console` | Let the development Player open its console of logged errors over the game, which it otherwise keeps closed, as the Editor's Play mode shows errors only in its Console window. The Player log has every error either way. |
| `--builtin-shader` | Verified snapshot of a retained built-in shader. Repeatable. |

The addressable scenes and assets of the project's Addressables groups (everything but code and shaders) are registered automatically and resolved from sources in the Player; see [AGENT_MCP.md](AGENT_MCP.md#addressables-from-sources) for what that covers. So are the enabled scenes of the Build Settings after the first, which the game loads through `SceneManager` by name or index: the host stands in for each with an empty scene that the Player fills from the authored one ([UNITY_RELEASES.md](UNITY_RELEASES.md#scenes-the-game-loads-by-name)).

Arguments after `--` are passed to the game. Through MCP the same options are the `gameArguments`, `contentScenes`, `contentPrefabs` and `addressables` of `session_control(start)`; its `hotReload: false` is `--no-hot-reload` and its `onUnsupportedEdit` is `--hot-reload-fallback`, which the MCP server sets to `report` unless told otherwise. A rejected update whose error needs a new source graph carries `preparationRequired: true` in its `update-rejected` event: the Player keeps its old code until the project is prepared again.

Prefer an output under `Library/UnityMicroEditor`: Unity clears a project's `Temp` when its Editor restarts. One output directory belongs to one running session. A second run with the same output is refused until the first stops; the lock is released on exit or crash. Reusing an output reuses its compiler and native caches.

## Time limits of a start

Every step of a start has a limit of its own, counted from the moment the step begins; no limit spans the run, so a shader that takes ten minutes to build the first time leaves the steps after it their full time. A step that runs out fails under its own name with the limit in the message.

| Step | Limit | When it runs out |
| --- | --- | --- |
| Build of a shader, shader graph or compute shader by the Editor's code in a worker | 30 min a build | the session fails with the worker's error |
| Write of the scene by the scene worker | 3600 s for a new worker's start and imports and the write; 1800 s for a write by a worker that lives on | the preparation fails with the worker's error |
| One run of the import worker that parses a `.shader` of a built-in pipeline project | 45 s for the context, the import, the stop of the worker's store, the extraction and the check that no source changed | the batch is run once more with 90 s and `native-import-repeated` is reported; a second failure is `native import of ...: the import ran out of its 90s budget ...`. The same repetition follows a run whose `Unity.DataStore` did not stop by itself and had to be killed. |
| Player start, from its launch to its ready report (some 5 s) | 45 s for a Player that got to run. One that has had less than 30 s of processor time by then is waited for until it has had as much, at most 300 s from its launch | `Player start: not ready within its 45s budget (it had 31.2s of processor time); see <report>`, or `Player start: not ready within 300s, the limit for a Player that hardly gets to run (budget 45s; it had 0.4s of processor time); see <report>` |
| Reload acknowledgement in a watched session | 20 s | `update-rejected` |
| Code patch acknowledgement in a watched session | 20 s, or the moment the Player exits | the Player is restarted on the newest compiled code: `code-updated` with `reason: "hot_reload_failed"` |

The two extensions exist because these limits were measured on an idle machine and have failed on a busy one: while another build ran on the same machine, the check after an import that takes 10 ms took 31 s and a Player that normally writes the first line of its log within half a second had not written it after 45 s. Neither says anything about the project. The limits still bound a start: an import is repeated once, and a Player that uses its processor time without becoming ready fails after 45 s as before. A Player that is blocked without using processor time is given up only after 300 s; the message then says how little it ran.

The runner may be started without a terminal and with SIGINT ignored, as a shell does for a command it puts in the background (`nohup ... &` in a script, a service manager). It handles SIGINT and SIGTERM itself, and the Player it starts gets the default handling of both whatever the runner inherited, so stopping a session interrupts the Player and does not wait for the escalation to SIGTERM.

The runner asks for OpenGL Core with VSync off. Its readiness check rejects a missing or Null renderer and known software renderers (llvmpipe, softpipe, lavapipe, SwiftShader). This is a check of known names, not proof of hardware rendering on every vendor.

## How changes are applied

| Change | What happens |
| --- | --- |
| Scene, prefab, material, ScriptableObject, model, texture, audio, video, text, UI or any other asset the scene reaches, and their `.meta` files | The scene worker imports what changed and writes the scene again in the Player format ([NATIVE_IMPORT.md](NATIVE_IMPORT.md#scenes-in-the-player-format)); the same Player loads the new scene and unloads the old one. Scene state resets, a moved object or a renamed GameObject included; loaded assemblies remain. A write whose scene and dependency files come out the same (a change of metadata alone) reloads nothing. |
| Shader (`.shader`, `.shadergraph`, `.compute`), a file it includes, a material's keywords | The shader is built again and published as a file of its own ([NATIVE_IMPORT.md](NATIVE_IMPORT.md#shaders)): in a built-in pipeline project the worker parses it and `UnityShaderCompiler` compiles its programs, in a render pipeline project the Editor's build code writes it in a worker; the shaders that fall back on it or use its passes follow. The scene is loaded again in the same Player. [NATIVE_IMPORT.md](NATIVE_IMPORT.md#shaders) lists what is refused. |
| Material (`.mat`) | The scene is written again with the material. In a render pipeline project the shader it names is built with the project's materials, also when it is a shader of a package; a change of keywords builds it again ([NATIVE_IMPORT.md](NATIVE_IMPORT.md#shaders-of-packages)). |
| C# change that can be patched: changed method bodies, added non-virtual methods, added fields of classes | The SDK recompiles the changed assemblies and the new code is patched into the running Player. The process, the scene and all game state stay. See [Hot reload of C#](#hot-reload-of-c). |
| C# change that cannot be patched: a new type, a generic method, a new Unity message and the like | With `--hot-reload-fallback restart` the Player restarts on the new code; process and scene state reset. With `report` the Player keeps its code and the change waits for a `recompile` command. |
| C# change with `--no-hot-reload` | The SDK recompiles the changed assemblies, dependent assemblies as well where an API changed, and the runner restarts the Player. Process and scene state reset. |

The watcher starts before the Player is prepared, so edits made during startup are picked up. Prepared JSON revisions and DLLs are stored apart from the sources. The compiler checks the SDK, source hashes and assembly configuration. A published shader is checked against its source and metadata hashes, its GUID, and the local IDs and types of its objects; a written scene against the hashes of every source it was made of.

An inconsistent or unsupported change produces `update-rejected`, and the last confirmed revision keeps running. A C# compile error, for example, leaves the current Player untouched.

## Hot reload of C#

In a watched session a C# change is applied to the running Player without a restart where that can be done exactly. Nothing is approximated: a change that cannot be patched is named with its reason, and only a restart on the new code applies it. [HOT_RELOAD.md](HOT_RELOAD.md) is the guide to it; this section is the runner's part of the contract.

After a successful compile the runner compares the hash of every game assembly with the one the Player loaded at its start. For those that differ it runs the patch builder (`tools/HotReloadPatcher`), which compares what the two compilations mean and writes one patch assembly per changed assembly. The runner then asks the Player to apply them. The applier (`tools/HotReloadRuntime`), loaded into the Player by the bridge, redirects the changed methods to the patch. A patch always holds every difference from the loaded assemblies, so reverting an edit restores the loaded code and a later edit replaces the patch before it.

| Outcome | What the runner does | Event |
| --- | --- | --- |
| The patch was applied | Nothing else. Same Player, same scene revision. | `code-patched` |
| The new compilation means the same as the loaded assemblies and no patch is applied (a comment, a refused edit that was reverted) | Nothing is sent to the Player. | `unchanged` or `updated` of the scene part, with `fullRecompileRequired: false` |
| No body differs, but the Player does not take over all of the change (the accessibility of a method, a removed `[RuntimeInitializeOnLoadMethod]`) | Nothing is sent to the Player. | The same event, with the builder's `notes`, `notesCount` and `hotReloadReport`, and with `newNotes` in the window that brought them |
| The same while a patch is applied (a patched edit that was reverted) | The Player restores the methods the patch redirected. | `code-patched` with `restoredMethods` and no `assemblies` |
| The change cannot be patched, or the Player refused the patch before it changed anything | The fallback: restart, or report. | `code-updated` with `reason: "hot_reload_unsupported"`, or `update-rejected` with `fullRecompileRequired: true` |
| The Player says its code is in an unknown state, exits during the patch or does not answer in 20 s | Restart on the newest code, in both fallback modes. | `code-updated` with `reason: "hot_reload_failed"` and `error` |

Which edits are patched is decided by the patch builder alone. [HOT_RELOAD.md](HOT_RELOAD.md#edits-and-what-the-reply-says) lists every edit with its outcome and every reason and note code with what to do; `dotnet HotReloadPatcher.dll --codes` prints the builder's codes. In short: changed bodies of non-generic methods of non-generic types, added non-virtual methods, added instance fields of classes and added static fields without an initializer are patched; added, removed or renamed types, generic code, added virtual methods, constructors and Unity messages, changed static constructors, attribute changes, a field whose accessibility or `readonly` changes what Unity serializes, and what Netcode for GameObjects registers at start-up are refused. A refusal lists `{code, member, message}` for every refused member. Three more codes come from the runner: `patch_builder_failed` (the builder failed or its result was unusable; the message is its output), `apply_failed` (the Player refused the patch and changed nothing; the message starts with the applier's stage, `resolve`, `inlining`, `compile` or `write`, or is the bridge's own message when the applier did not run) and `assembly_not_loaded` (the compiler graph has an assembly the running Player did not load).

An applied patch can carry notes, for example `existing_instances_keep_default` for an added field. `callback_failed` says that a `[InvokeOnHotReload]` method of the game threw or could not be called; the patch is applied all the same. A change that needs no patch can have notes as well: a method made `public` compiles to the same bodies, and the Player keeps the old accessibility for reflection until it restarts (`accessibility_changed`). Such notes are on the `unchanged` or `updated` event of that window and on every later one, also on the `update-rejected` of a scene part, until a compile without them, a patch, a refusal or a restart. A field made `public` or private, or given or relieved of `readonly`, so that Unity starts or stops serializing it is no note but a refusal (`serialization_changed`): nothing of that edit is in the Player before a restart.

A scene update can bring a note too. A patch that adds a field Unity would serialize says so once (`serialized_field_not_restored`). When a scene, prefab, asset or animator controller that changes later holds a value for that field, the Player takes the scene and the field keeps what its initializer gave it: the `updated` event of that window carries the note again as `newNotes`, with the file, so a reader is not told "updated" and nothing else. For a field of an engine object the builder names the classes whose documents hold its value (the declaring class and every class of the game it read that derives from it, `holders` in its result): a value is a key of a document whose `m_Script` is the script of one of them (of any document when none of them has a script), or a prefab instance's override whose `propertyPath` begins with the field (`boost`, `boost.Array.data[0]`). A field of a `[Serializable]` class or struct is held inline: a key of any document, or an override that names it after the field that holds the object (`tuning.armor`); an override of `m_LocalPosition.x` is no value of an added field `x`. An override is not traced to the script of its target, so an override of another script's field of the same name counts too. Each changed file is read and split into documents once for all notes. Each save of such a file brings the note again: the Player rebuilt the objects and the field has its initializer's value again. In the same way a patch that adds an instance method to an engine object remembers it, and a later scene, prefab, asset, controller, animation clip or model `.meta` that names it as a UnityEvent's `m_MethodName` or an animation event's `functionName` brings `added_method_named_by_string` with the file: the event looks the method up by name in the loaded class and does nothing. Data saved before the patch that adds the method is not searched. The loader does not apply `[FormerlySerializedAs]`: a renamed field does not take the value saved under its old name, not even after a restart, and the note of such a field says so; a save of the old key brings no note, since the search looks for the new name. A prefab instance's override of a script's field that the prefab's document holds no key for (a field added after the prefab was saved) is applied as Unity applies it: the override's value is the field's.

A patch holds every difference from the loaded assemblies, so `notes` of `code-patched` are those of every edit since the Player process started. What the edit of this window brought is listed on its own, as `newNotes` with `newNotesCount`: the notes that the code the Player ran before did not have, and the notes about a method whose code changed again. They also lead `notes`, so the limit of 16 cuts what stood before. A note of the builder carries a `revision` in the report for this comparison; the events leave it out.

The runner trusts an acknowledgement only as far as it can check it: the Player's process, the generation, and the number of redirects, which must be the number the patch lists. An acknowledgement of success with another number restarts the Player (`hot_reload_failed`). The patch builder gets 60 s for one attempt; the builder the session keeps gets a third of that (at most 15 s) and, when it does not answer, is stopped and the request runs once in a process of its own within the rest. A run that does not end by then is stopped and the change is refused with `patch_builder_failed`.

In a window with both a C# and a scene change the code is patched first and the scene is then updated on the same Player. A scene manifest and the content routes keep naming the assemblies the Player loaded, which the Player checks on every scene reload. Script types and runtime initializers of a newer compile are used only by the process that loads it, so they wait for the next restart.

The events of a window are printed together when its work is done: `code-patched` first, then the scene part (`updated`, or `update-rejected` with its own error), and with `--hot-reload-fallback report` the refusal as the last one. A reader that acts on `code-patched` therefore finds no step of that window still running. An event that is not the window's last carries `pending: true`. So does every event of a window that ends while source changes are queued which it did not take: the next window handles them, and its events are the answer for whoever saved them. A window always ends with at least one event, so a `pending` one is always followed by another, unless the session ends. Every watched session marks its events this way, with or without hot reload.

`pending` says what the runner knows when it prints. A save that is made after the answer to an earlier one is announced by nothing until its own window ends, which is a whole compile later. A reader that must know whether the events it has read cover what it saved asks with the `sync` command (see [Commands](#commands)).

`updated` and `unchanged` carry `fullRecompileRequired: true` while a refused change waits, also for later scene edits, which keep working on the same Player. A scene part that is rejected while a refused change waits, before the Player or by it, says the same on its `update-rejected` and repeats the reasons beside its own `error`: a scene that uses a type only the refused code has fails for that reason alone, and the recompile is what applies it. Without a refusal the rejection of a scene part says `fullRecompileRequired: false`, so that a refusal which the same window took back does not outlive it. The refusal is printed again after every compile that still cannot be patched. It ends with a patch, with a compile that means what the Player runs, or with a restart.

Whatever restarts the Player (the fallback, a `recompile` command, changed content routes) starts it on the newest compiled code. Code generations are counted per Player process and start at 1.

The Player is started with `MONO_ARGUMENTS=-O=-float32,-inline`, because a redirect does not reach the copies an inlining JIT makes of small methods. A value the variable already has in the runner's environment is passed on unchanged; if it leaves inlining on, every patch is refused with `apply_failed` and the stage `inlining`.

Both programs are built from the tool's sources with the SDK of the Unity installation, the first time while the Player starts: the patch builder into `<output>/hot-reload/patcher/<key>/`, the applier into `<output>/hot-reload/applier/<key>/UnityMicroEditor.HotReload.dll`. A Player process keeps the applier it was started with. A session in which either build fails prints `hot-reload-unavailable` with the error, says `hotReload: false` in `ready` and restarts the Player on C# changes.

### Measured

On 2026-10-14, on a copy of `Samples/RawProject` (one game assembly; AMD Ryzen 7 5700X3D; other agents' builds and Players ran on the machine), edits of one method body, each a source state the session had not compiled, took from the write of the file to the runner's answer (`code-patched` read from its standard output), in sessions that alternated between the two binaries:

| Binary | Edits | Median (s) | p90 (s) | Load average while measured (median) |
| --- | --- | --- | --- | --- |
| Before (f84be4d) | 32 | 3.36 | 3.90 | 4.2 |
| Now | 32 | 0.41 | 0.55 | 5.2 |
| Now, with `UME_COMPILER_COLD=1` (no kept processes, no hash cache; measured after the two, not alternated) | 6 | 2.35 | 2.39 | 3.5 |

The same measurement after the latency work was rebased onto the stage 5 fixes of the patch builder, the runner and the controller, against the binary with those fixes and without the latency work (32 edits each, load average 2.4 and 2.6): 0.42 s median and 0.47 s p90 against 3.14 s and 3.31 s.

After the stage 6 fixes, a window whose scene manifest would not change publishes no scene revision, so a patched edit no longer waits for the `fsync` below. Alternating sessions on a copy of the sample (`Temp/e2e/hr`, 32 edits each, load average 3.1 and 3.6, 2026-10-14) against the binary of 999e3cc: 0.33 s median and 0.43 s p90 against 0.44 s and 0.55 s; the step after the patch took 0.006 s against 0.112 s (medians of the last session of each).

Where the time of an edit goes, from `UME_TIMINGS=1` (median of 32 edits of this binary):

| Step | Seconds |
| --- | --- |
| The 35 ms the watcher waits for more changes of the save | 0.035 |
| Roslyn through the compiler server | 0.123 |
| Copy of the Player directory (`cp --reflink`) and the map of the copy | 0.023 |
| The candidate graph and the configuration inputs (three checks, now by `stat`) | 0.037 |
| References of the assembly and the key of the revision | 0.024 |
| Lifecycle rewrite (served) | 0.016 |
| Script type and declaration checks (served) | 0.001 |
| The rest of the compile (sources, snapshots, the DLL into the copy, the revision entry) | 0.007 |
| Patch builder (served) | 0.002 |
| Request to the Player and its acknowledgement | 0.020 |
| After the patch: scene preparation for the running Player, the scene check, the events (0.033 s with the old code). Most of it was the `fsync` of the new scene revision, which waited for the filesystem journal of the Player directory the compile copied a moment before: 0.09 to 0.12 s on XFS. Since stage 6 no revision is written when the manifest does not change, and the step takes about 0.006 s | 0.086 |
| In all (`totalSeconds`, the median, not the sum) | 0.413 |

The first edit of a session also starts the three processes below, and took 1.8 s against 3.6 s with the old code (one session of each): the server's first compile 0.84 s, the helper's first rewrite 0.56 s, the builder's first run 0.23 s. On a new session output the stage 6 review measured 2.40 s, of which 0.47 s hashed the loaded Player directory once. A session that compiled at its start (for example because its preparation was made by another build of the helper) has warm processes, and its first edit took 0.29 s of compile. The applier itself needs 2 to 4 ms for a request and 10 to 30 ms for the first one of a Player process. `compileSeconds` of `code-patched` is the whole compile step of the window; with `patchSeconds` and the 35 ms the watcher waits for more changes it adds up to `sourceToAckSeconds`. `compileSeconds` of `code-updated` is the compiler's own time. Sources that the session output has compiled before (an edit that is taken back, a session that repeats edits) are answered from its assembly cache without the helper and the compiler.

The same steps took 3.3 s before (median of 8 edits of the old code with `UME_TIMINGS=1`, load 4.4): hashing the new Player directory 0.90 s, Roslyn 0.73 s, the lifecycle rewrite 0.53 s, the patch builder 0.19 s, the baseline Player directory and the tools for the revision key 0.19 s, the configuration inputs three times 0.38 s, the two checks of the helper 0.21 s. Single edits of the old code took 35 to 58 s while other builds filled the disk queue (4 of 24 edits at load 18 to 48 on the same day), and 10 to 27 s in earlier tests: the compile hashed every file of the Player directory it had just copied (177 MB that no process had read), the configuration inputs three times (3404 files, 103.5 MB) and the baseline Player directory, and started five .NET processes. Now the baseline Player directory is hashed once per session and checked by `stat`; the map of a new Player directory is that of the directory it was copied from when `stat` finds that directory as it was hashed after the copy and the copy with the size and modification time of every file, with the files the compile wrote hashed as written (anything that moved is read in full, as before, and a revision is restored at a later start only when a full read of its directory equals its map); unchanged inputs are answered from the hash cache; and the compiler, the helper and the patch builder are processes the session keeps.

### Processes a watched session keeps

A watched session (`--watch`) does not start the compiler and its helpers anew for every edit. It keeps three .NET processes of the Unity installation's SDK, each started at its first use on the thread that runs the session:

| Process | What it does | What it keeps between two edits |
| --- | --- | --- |
| The Roslyn compiler server (`dotnet exec VBCSCompiler.dll -pipename:ume-<16 hex of the output>-<runner pid>-<n>`) | compiles every game assembly; `csc.dll` connects to it with `-shared` | the metadata of references and analyzers, by path and modification time |
| The lifecycle helper (`LifecycleCompiler.dll --serve`) | the lifecycle rewrite of an assembly, `--type-definitions`, `--declaration-hashes` | the metadata of references, under the SHA-256 the runner checked before the request |
| The patch builder (`HotReloadPatcher.dll --serve`) | the comparison and the patch of a C# edit | nothing |

Besides them, the compiler keeps the SHA-256 of the inputs that do not change during a session (the configuration inputs, the references, the SDK and generator files, the helper) and answers it while `stat` gives the same device, inode, size, modification and change time. A hash is kept only when the file did not move while it was read and both its times were at least two seconds old when the read began; any other file is read every time. Game sources are always read. The start of a session, the restore of a revision, a cached assembly before it is used and every preparation read everything.

Nothing a process keeps decides a result:

- The compiler server would compile against the old bytes of a reference that was rewritten in place under its old modification time (reproduced with the SDK). The runner hands it every reference and analyzer with the content it checked; a path that arrives with other content than before ends the server, and the compile goes to a new one under a new name.
- The helper takes a reference from its memory only under the hash the runner sent, and checks a reference it reads against that hash.
- The patch builder keeps nothing; the numbers it gives the closure types of one request start anew with every request, as in a run of its own.

What happens when one fails:

- A served compile that fails or reports an analyzer failure (CS8032, CS8784, CS8785) runs again in a process of its own, and that result counts. A helper or builder request that fails in any other way than a compile error of the sources, or a process that died, did not answer in time (120 s for the compiler and the helper, a third of the builder's 60 s and at most 15 s for the builder) or answered out of protocol, is killed and the same request runs once in a process of its own. A compile error is reported only as the process of its own would report it, in the same words.
- The next request starts a new process. After three failures in a row the session runs that program once per request, as before, and says so on standard error.
- A builder run of its own that does not finish within the rest of the 60 s is the timeout of that attempt (`patch_builder_failed`), as without the service.
- No `csc.dll` client starts a server of its own: its `DOTNET_HOST_PATH` names no executable, so a client that reaches no server compiles in its own process. No server of the machine is shared.
- The processes end with the session: the helper and the builder at the end of their standard input, all three by a signal when the runner drops them, and by the kernel (`PR_SET_PDEATHSIG`) when the runner is killed. The socket file of a killed runner's server stays in `/tmp` until the next session on the same output removes it.

The compiler server runs with the workstation collector (`DOTNET_gcServer=0`); the SDK's server asks for the server one, under which it grew by 4.2 MB with every compile in the sample and 12 MB in a Netcode game (stage 6 review, `served6-3`), up to 1.36 GB for the three processes together. A private server fed the response file of a session's compile grew by 0.99 MB per compile with the workstation collector, over 220 compiles, at a median compile time of 0.128 s against 0.155 s with the server one (the same review). The references of a compile are the files of the newest Player directory, a new directory every compile, which the server's metadata cache, kept by path and modification time, does not find again; that adds about 1.8 MB per compile and is not changed. The helper stays between 250 and 390 MB, and the builder near 110 MB. A process that holds more than 1 GiB is ended before its next request and a new one takes it, which costs that request the time of a first one (about 1 s) and is no failure. A larger game holds more in the first two. `UME_COMPILER_COLD=1` in the runner's environment runs a session without any of them and without the hash cache, as before: every Player directory is then also read in full at every compile. `UME_COMPILER_VERIFY=1` compiles every assembly a second time in a process of its own into the same paths and fails the compile when a byte of the DLL or the PDB differs. `UME_TIMINGS=1` prints one JSON line per window on standard error with the time of every step of the compile and of the patch (`timings`, `totalSeconds`, `compile`, `patch`); nothing reads it.

### What a session output keeps

Every start of a session copies the host Player into `<output>/players/<id>/`, and every compile publishes a Player directory of its own under `<output>/compiler/.code-revision-<id>/Player/` (177 MB in the sample, copied with `cp --reflink=auto`: on XFS with reflinks it shares the blocks of its source, on ext4 or tmpfs it is a full copy). The sessions of one `mcp` server share one output across every start and reset.

The runner removes what nothing uses any more: once a started Player is ready, and after every window that compiled or restarted the Player, after its events are printed. It never removes

- the staged host and the prepared baseline of the running session, also when the session started on a compiled revision,
- the Player directory of the running Player, of the newest compile and of the compile the running Player was started on,
- a directory that holds the executable of any running process (a Player whose runner died, a Player of another session),
- a directory that a recorded preparation names (`Library/UnityMicroEditor/preparations/*/*.json`; one that cannot be read keeps everything), or that a `run --prepare-only` on the output recorded (`<output>/prepared/<id>.json`, the newest four records: the Player and the manifest a `--prepared` launch of it needs),
- the directories of the four newest revision entries (`<output>/compiler/code-revisions/*.json`), which let a later start take a compiled revision instead of compiling, and of an older entry whose directory is in use for one of the reasons above,
- the two newest directories of compiles that published nothing (the diagnostics of a compile error).

Everything else among `players/*` and `compiler/.code-revision-*` is removed, older revision entries first. An output therefore holds at most about ten Player directories however many edits and starts it saw. The smaller files are bounded too: the newest eight scene revisions (`revisions/<id>`, one per save that changes what the scene manifest holds; a window whose manifest would be the one before but for its id, a patched C# edit for one, publishes none), and the newest eight run directories (`runs/<id>`, one per Player process, with its log and the patches of its edits) stay, besides the scene and the run of the running Player, the assemblies the compiler uses as the current ones and what a prepare-only record names. A removed scene revision or assembly is made again when it is needed. Native imports and preparations are not bounded.

### A start whose sources did not change

A start does not walk and prepare a scene that nothing changed since its last start, as Unity enters Play Mode with an up-to-date asset database. After publishing a revision, an importer records in `<output>/native/revision-memo.json` what it was made of: the session's configuration (the baseline with the assemblies, script types and initializers, the runtime-addressed prefabs, the native content the session holds after replaying its index), the GUIDs of the project, the metadata (device, inode, size, mode, modification and change time) of every file the preparation read, the published manifest and the program. The first preparation of the next start takes that revision when all of it is as it was, and reports `native-revision-unchanged`; any difference, an edit and its revert included, prepares the scene. A native source that changed is left out by the replay of the index, so it changes the configuration too. Later passes of a watched session always prepare: they follow edits that only the walk of the sources sees. The host is staged once per output for a prepared Player and bridge (`players/staged-<key>`) and keeps its paths, which the manifest names. A run of the prepared benchmark scene starts in 11 s instead of 21 s.

### Hashes of files

A start checks most of what it uses by its SHA-256: sources, metadata, assemblies, imported files. A start of the benchmark project hashed some 90 000 files, 3.5 GB, each time: 4.7 s with the files in the page cache, 20 s without. The hashes are now kept across processes in `$XDG_CACHE_HOME/unity-micro-editor/file-hashes-v1.log` (`~/.cache/...`), and a hash is taken back while the file's device, inode, size, mode, modification and change time are what they were when it was hashed, as Unity's asset database and git do. A hash is kept only for a file neither of whose times was within two seconds of its reading, since a write in the same tick of the clock could leave every field as it was; the change time cannot be set back by a program. `UME_HASH_CACHE=0` turns the log off. A process holds 68 bytes for each hash of the log (128-bit digests of the path and of the metadata, the hash and its line), at most 36 MB for the 350 000 files a workstation's log named after a week of work, where it held about 130 MB with the paths and the hashes as text. Once a day a process writes the log again on a thread of its own without the files that are gone or changed since (`file-hashes-v1.pruned` holds the time); that log went from 359 000 lines to 227 000.

### Memory or speed

`UME_MEMORY` in the environment of any command, or `--memory` before it (`unity-micro-editor --memory low run ...`), chooses what a process and the processes it starts keep to be fast:

| | `low` | `balanced` (default) | `fast` |
| --- | --- | --- | --- |
| Compiler server, lifecycle helper, patch builder, hash cache of a watched session | none (as `UME_COMPILER_COLD=1`) | kept | kept |
| Scene and asset worker of a session after its last write | stopped at once | kept two minutes | kept ten minutes |
| Import workers of a worker run | at most 4 | 2 to 32 | every core |
| Text and parsed YAML of the sources a preparation read | not kept: read again when needed | kept | kept |
| Freed memory given back to the system | after the start and after every edit | after the start | after the start |

A knob of its own decides over the mode: `UME_COMPILER_COLD=0` keeps the processes in `low`, `UME_SCENE_WORKER_IDLE_SECONDS` sets the time a worker is kept. What is kept on disk, the stores of the next sections and the hash log, is the same in every mode: it costs no memory and spares the next process the work.

### The compiler store of a project

What the compiler makes from given inputs is kept for the whole project in `Library/UnityMicroEditor/compiler-store`, which every preparation and session of the checkout shares: compiled assemblies (`assemblies/<key>`, a DLL and a PDB per state of an assembly's sources, references, configuration and tools), the lifecycle helper and the Netcode post-processor built from the tool's sources, the types each DLL defines (`type-definitions`), the types the sources of host packages declare (`source-declarations`), script types, and the assemblies the Play mode linker made (`play-mode-links/<key>`, by the linker, the host, the Editor and the input; [UNITY_RELEASES.md](UNITY_RELEASES.md#play-mode-code)). A preparation therefore compiles only what changed since any earlier preparation or session, and a session after a preparation takes its assemblies. An assembly that does not compile is remembered as well: the errors of an Editor assembly that is left out are kept beside its entry under the same key and given again without a compile. The store is not bounded, and it is not copied to another checkout: an assembly holds the paths of the sources it was compiled from, and hot reload compares it with what that checkout compiles. Measured on the benchmark project, a preparation after an earlier one took 41 s instead of 159 s.

### The native store of a project

Every session of a project, a preparation's included, writes its worker runs and published files to one store, `Library/UnityMicroEditor/native-store` (`imports/<id>` per worker run, `baked` for the published shaders and the written scenes' files, `scene-library` for the imports the scene worker keeps between workers). The written scenes and the shader builds are also kept in `Library/UnityMicroEditor/built-sources` under a key of what they were made from (the import environment, the sources and their metadata, the worker's code, the assemblies and scripts), so a scene or a shader a preparation made is taken from there by the runs after it, by the content scenes of a session and by the next preparation, instead of being made again. A file of the store is written once and appears whole, so sessions may share it at once.

What a session holds stays with its output: `<output>/native/index.json` lists the shaders its scene uses, and the next start of the output replays it. An output without an index starts from a seed: a run from the index of the preparation it was given (`nativeImport.index` of `current.json`), a preparation from that of the newest other preparation of the scene. A seed's entries are checked against the sources as they are now, as the output's own would be; one that no longer replays (its source changed, its file is gone) is built again on demand and reported as `native-cache-rejected`, and `native-cache-seeded` gives how many were taken.

A store may be copied to another checkout of the project, a git worktree for one: copy `Library/UnityMicroEditor/native-store` and the store of shader builds, `Library/UnityMicroEditor/built-sources` (and, to spare the shader compiler, `shader-cache`). Nothing else under `Library/UnityMicroEditor` is meant to be copied: preparations, sessions and the staging copy name the checkout they were made in. An entry of a copied store names the files of the store it was written in; such a path is read as the same file of the copy's store, and every file is checked by its hash where it is used, so a file the copy lacks is made again. The keys do not name the checkout: an import is named by the project-relative source and the import environment, a build also by the scripts the Editor compiled, whose record is hashed with the checkout's paths and the Editor run's socket left out. On a copy of the benchmark project of [HOSTLESS_PLAYER.md](HOSTLESS_PLAYER.md#a-game-project-without-the-editor-2026-10-18), the first preparation and run took 570 s instead of 1884 s, every shader build and every import came from the copied stores, and a new session output of it started in 114 s. A store of shader builds kept by an earlier version of the tool, keyed by the record as it was, is still found, and the build is kept under the new key too.

Shaders the index gives are taken in passes, since one entry can depend on another (a shader on the one it falls back on), until a pass takes none; an entry refused after that is built again and reported as `native-cache-rejected` with the reason.

### Commands

With `--commands-stdin` the runner reads one JSON object per line from its standard input:

```json
{"command":"recompile","requestId":"<ASCII letters, digits or hyphens, at most 64>"}
```

`--hot-reload-fallback report` requires `--commands-stdin`: a change that is reported waits for this command. `recompile` checks every source of the compiler graph, compiles what changed and restarts the Player on the newest compiled code, also when nothing changed. The answer is `code-updated` with `reason: "requested"` and the `requestId`. When the check or a later step fails, the Player is kept and the answer is an `update-rejected` with the error and the `requestId`. The event that names the `requestId` is the answer to its command also when it carries `pending: true`; the mark then only says that more events about the sources follow, for example the refusal that still stands. A command that arrives in the debounce window of a source change is handled with it. One that arrives while the Player is restarting for another reason is answered by that restart, unless a source changed in between.

```json
{"command":"sync","requestId":"<ASCII letters, digits or hyphens, at most 64>"}
```

`sync` asks the runner to say when it has answered every source change that was saved before the command. The answer is the event `{"event":"synced","requestId":"<id>"}`. It is printed between two windows, after the events of all those changes: the runner first reads everything the kernel has noted about the watched directories, which holds every file that was written and closed, renamed or removed when the command arrived, and answers when each of those changes has gone through a window. With nothing saved the answer comes at once. A reader that saved files, sent `sync` and has read `synced` has therefore read the outcome of its saves, whenever they were made; without the command it cannot tell an answer to an earlier save from the answer to its last one. `sync` restarts nothing and takes no place among the `recompile` commands: a `recompile` sent before it can be answered after it. Changes that are saved after the command may or may not be covered.

The watched directories are `Assets`, `Packages` and `ProjectSettings` of the project, with every directory below them, also one that is made during the session. `synced` speaks for saves the watcher sees and for no others. It does not see: a file outside those three directories (a local `file:` package that lies beside the project is compiled where it lies and is not watched); a file that is written and still open, or cut short with `truncate`, until it is closed; anything reached through a symbolic link that is made during the session (a link that exists when the session starts, or inside a directory that is moved in, ends the session with `source watch rejects symlinks`). Such a save is picked up by the next compile that something else causes.

The watcher holds 8192 changes for the loop. When more than that wait at once (a branch switch, a generated tree) no count of them is complete, and a `sync` that arrives then is answered after the window that made room and the ones that follow it, never on a count that left changes out. More than the kernel's own queue holds (`fs.inotify.max_queued_events`) ends the session with `inotify queue overflow`.

A line that is no command is reported as `command-rejected` with `error` and the first 256 bytes of the `line`. Neither it nor the end of the input ends the session. No process the session starts inherits the runner's standard input.

## Limits

- A baseline is required. The runner does not turn an arbitrary folder with `Assets` into a running Player on its own.
- Assets are imported by the scene worker as the engine imports them; what was compared with the Editor's artifacts is listed in [NATIVE_IMPORT.md](NATIVE_IMPORT.md). Video is VP8 in a `.webm` file, the one form the engine reads on Linux. A session needs the Editor compilation its preparation made or found, and takes the host's pipeline global settings and built-in resources ([NATIVE_IMPORT.md](NATIVE_IMPORT.md#scenes-in-the-player-format)).
- Changes to `ProjectSettings.asset`, `GraphicsSettings.asset`, `.asmdef` or package configuration, GUIDs or types need a fresh preparation. Other settings files the Player only reads as settings managers (tags and layers, physics, time, quality, input, audio, 2D physics, NavMesh areas, VFX, multiplayer) are written into the next Player: the session restarts it with `code-updated` and `settingsChanged: true`, without a host build. Asset references in them (a quality level's render pipeline, a physics material) keep the host's value; a render pipeline change needs a fresh preparation. A settings file at an older serialized version than the engine writes keeps the host's values for that manager until the Editor saves it again ([HOSTLESS_PLAYER.md](HOSTLESS_PLAYER.md)).
- Every change that reaches the written scene loads it again: the scene's state starts over. Nothing is patched into the running scene.
- Hot reload exists for a watched session only, and for the verified profile: a development Mono Player on Linux x86-64. The applier refuses other platforms, and the runner then takes the fallback for every C# change.
- A patched Player keeps the script types and runtime initializers of the assemblies it loaded. A new script file is a new type and is refused.
- With `--hot-reload-fallback report` the Player runs its old code until a `recompile` command or another restart. A scene that needs a type only the new code has is rejected, with `fullRecompileRequired: true` on its `update-rejected`, and the old revision keeps running until then.
- A restart that was decided but could not happen, for example because an import of the same window failed, is reported as a plain `update-rejected`. The Player keeps its code, and the next change of any kind restarts it.
- What a patch does to running code is the applier's ground: a method that is on a stack finishes its old body, and a coroutine or `async` method that is running keeps the body it started with. Other threads are not stopped; a first redirect replaces the bytes of one instruction, and a method for which no way is left in a Player is refused with `apply_failed` (stage `write`) for the life of that Player. [HOT_RELOAD.md](HOT_RELOAD.md#limits-and-known-problems) says how often, and `tools/HotReloadRuntime/README.md` has the details and the measured numbers.
- A stack trace of patched code names the patch's copy of the method (`HotReload.G<generation>.<type>.<method>`) with the file and line of the source, as a restarted Player does for the same code: the compiler reads a rewritten copy under `<output>/compiler`, and the rewrite states the source file and its lines with `#line`. A source that sets lines with `#line` itself keeps what it says, and its traces name the copy. The line of a frame is the runtime's: for a statement that logs it is the statement's line, and for a `throw` Mono names the line of the instruction after it, which can be the next statement or the closing brace, in a patched and in a restarted Player alike.
- The rewrite only adds a public start-up bridge for each `[RuntimeInitializeOnLoadMethod]` method, on the line of its class's closing brace, so every statement and member keeps the line and column it has in the source. It checks that for every file it writes: a file in which a line would move is written without directives, like a source with `#line` of its own, and its traces name the copy. Only a `#line` the compiler obeys counts as the source's own; one in a branch that is switched off does not. A path that a directive cannot hold (a quotation mark or a line terminator, the Unicode ones included) also names the copy.
- A preparation records the lifecycle helper that rewrote each assembly (`lifecycleHelper` in its compiler manifest). A session whose helper is another build, or a preparation without the record, compiles those assemblies again before its Player starts, once per session output: otherwise the Player would load one rewrite and the first edit would be compared with another, and the patch builder would find differences nobody made (the path of `[CallerFilePath]`, an attribute that takes it).
- A comment or a blank line saved while a patch is applied compiles to a new patch with the same bodies. It is applied as a new generation (`code-patched`, `codeGeneration` advances), and the game's `[InvokeOnHotReload]` callbacks are not run for it, because nothing the Player runs changed.
- A save that arrives while the compiler reads the sources of an earlier one is rejected with `runtime source changed while snapshotting` or `during compilation`, with or without hot reload; the next window compiles the whole graph and applies the last save. The rejection carries `pending: true` when the watcher holds that save by then, and the controller does not report it to a waiting caller.
- A directory that is made and removed again before the watcher has looked at it (the temporary directory of a tool) is passed over; its removal is reported like any other change. Before, it ended the session.
- `pending` is set from what the watcher has queued at the moment the events are printed. A save that reaches it later is not announced by the events before it; the `sync` command is the way to know that a save has been answered.
- The Player of a session with hot reload runs without JIT inlining: script code that mostly calls small methods is about half as fast as in a session with `--no-hot-reload` ([the measurement](HOT_RELOAD.md#inlining-is-off)). Engine code is not affected.
- A Player process keeps every patch assembly it loaded. Its resident memory grew by 17 MB over the first ten patches of a process, most of it once, and by 2.3 MB over the thirty after them.
- A code patch is acknowledged on the Player's main thread. Game callbacks marked `[InvokeOnHotReload]` run inside that time; one that blocks for 20 s makes the runner restart the Player.
- A session output keeps at most about ten Player directories, eight scene revisions, 64 compiled assemblies and eight run directories besides those in use, whatever number of edits and starts it saw (see [What a session output keeps](#what-a-session-output-keeps)); native imports and preparations are not bounded, and a run directory holds the attempts of its Player process (3 MB for 54 attempts).
- A watched session keeps three .NET processes alive, each replaced when it holds more than 1 GiB (see [Processes a watched session keeps](#processes-a-watched-session-keeps)); `UME_COMPILER_COLD=1` runs it without them.
- `run --prepare-only` records what it prepared in `<output>/prepared/`; a later session on the output keeps the Player and manifest of the newest four records for a `--prepared` launch. An older prepared Player is removed by the next session on that output.
- A Player that logs an error or an exception before it is ready is not taken as started; the error names the counts, the first message the log shows with an error's stack trace and the log. Unity logs such an error for each object of a class with a public field of a 64-bit enum, for one.
- Only one session runs on an output: a second runner fails on the session lock, and an `mcp` server refuses the start of a second session on a project's output with `session_output_in_use`.
- Compatibility with every Unity project, and full Editor Play Mode behavior, are not claimed.

## Events and reports

The runner prints one JSON event per line: `prepared`, `ready`, `updated`, `code-patched`, `code-updated`, `content-updated`, `unchanged`, `synced`, `native-revision-unchanged`, `native-scene-written`, `native-scene-reused`, `scene-worker-wrote`, `scene-worker-restarted`, `scene-worker-stopped`, `scene-library`, `native-cache-rejected`, `native-cache-seeded`, `native-prebuild`, `native-prebuilt`, `native-built`, `native-build-reused`, `native-baked`, `native-import-repeated`, `addressables`, `addressables-skipped`, `addressables-changed`, `hot-reload-unavailable`, `play-mode-asset`, `play-mode-asset-refused`, `command-rejected` and `update-rejected`.

What hot reload adds to them, in a watched session:

| Event | Members |
| --- | --- |
| `ready` | `hotReload`: whether C# changes are patched into the Player of this session. |
| `code-patched` | `paths`, `pid` and `revision` (the scene revision; a code patch changes neither), `codeGeneration` (1 for the first patch of a Player process), `assemblies`, `patchedMethods`, `addedMethods`, `addedFields`, `restoredMethods`, `members`, `notes` (`{code, member, message}`), `notesCount`, `newNotes` and `newNotesCount` (the notes this edit brought), `compileSeconds` (the whole compile step of the window), `patchSeconds` (patch builder to acknowledgement), `sourceToAckSeconds` (from the source change), `report`. `patchedMethods` is the number of redirects the Player acknowledged. `members` and `notes` describe the whole patch, which is every edit since the Player process started. |
| `update-rejected` of a change that cannot be patched (`report` mode) | `error` (one sentence with the number of reasons and the first one), `paths`, `fullRecompileRequired: true`, `unsupported` (`{code, member, message}`), `unsupportedCount`, `report`. |
| `update-rejected` that answers a `recompile` command | `error`, `paths`, `requestId`, and `preparationRequired` where the error needs a new source graph. Never `fullRecompileRequired`. |
| `updated`, `unchanged` | `fullRecompileRequired`: `true` while a change that cannot be patched waits for a restart, `false` when the Player runs the newest compiled code. With `false`, `notes`, `notesCount` and `hotReloadReport` while the newest compile needed no patch and still has notes, and `newNotes` with `newNotesCount` on the event of the window whose compile brought them. An `updated` also has `newNotes` when a changed scene, prefab or asset holds a value for a field the applied patch added (`serialized_field_not_restored`). Absent in a session without hot reload. |
| `update-rejected` of a scene part | `part: "scene"`, its own `error`, and `ack` when the Player rejected it. While a change that cannot be patched waits (`report` mode): `fullRecompileRequired: true` and the `unsupported`, `unsupportedCount` and `report` of the refusal that stands. Otherwise `fullRecompileRequired: false`, with the notes as on `updated`. A compile error stays plain and has no `part`. |
| `synced` | `requestId` of the `sync` command it answers. Nothing else. |
| `code-updated` | `reason`, absent for a restart hot reload did not cause: `"hot_reload_unsupported"` with `unsupported`, `unsupportedCount` and `hotReloadReport`; `"hot_reload_failed"` with `error` and `hotReloadReport`; `"requested"` with `requestId`. A requested restart is `code-updated` also when no newer compile was loaded; it then has no `sourceHashes`, `compiledAssemblies`, `declarationChanged` and `compileSeconds`. `settingsChanged: true` when the new Player holds other settings managers than the one before it. |

Every event that answers a window, in a session with or without hot reload, carries `pending: true` when it is not the last word about the saved sources: another event of the same window follows, or source changes are queued for the next one. It is absent otherwise.

`assemblies`, `members`, `notes`, `newNotes` and `unsupported` hold at most 16 entries; the counts are complete and the report file has every entry. `report` of `code-updated` remains the Player's report, which is why the hot reload report has its own name there. A C# change that is patched or refused ends its window with that one event; an `unchanged` for the scene is not printed after it.

`<output>/runs/<id>/` holds the Player log, the ready and final reports, and reload requests with their acknowledgements. The ready report records the actual GPU and PID. An acknowledgement carries the revision, the native load count and any errors, and refers to a completed frame.

A run directory of a session with hot reload also holds `code-reload-request.json` with its `.ack.json`, and one directory per patch attempt, `hot-reload/<generation>/`: `request.json` and `result.json` of the patch builder, the patch assemblies, `apply.json` (what the Player was asked to apply), `ack.json` and `report.json`. The report has `schemaVersion`, `generation`, `outcome` (`patched`, `unchanged`, `unsupported`, `apply_failed`, `restart_required`, `player_exited` or `timed_out`), `error`, and the builder's `result` and the Player's `ack` in full. An attempt takes a generation of its own, also when it is refused, so `codeGeneration` can skip numbers.

Native import details are in [NATIVE_IMPORT.md](NATIVE_IMPORT.md).
