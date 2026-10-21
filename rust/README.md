# The CLI crates

`unity-micro-editor` is a Cargo workspace: the root `Cargo.toml` holds the
executable, the `agent` module and the integration tests (`rust/src/`,
`rust/tests/`); the layers below it are the crates of `rust/crates/`, each
rebuilt only when it or a layer under it changes:

| Crate | Modules | Uses |
|---|---|---|
| `ume-base` | `yaml`, `cache`, `graph`, `project/`, `packages/`, `editorbridge/`, `parity/`, `lmdb`, `protocol`, `registry`, `runtimeartifact/` and the small helpers | |
| `ume-authoring` | `authoring/` | base |
| `ume-playerbake` | `playerbake/` | base |
| `ume-compiler` | `compiler/`, `hotreload/`, `sourcegraph/` | base, authoring, playerbake |
| `ume-native` | `native/`, `engine/` | compiler |
| `ume-runner` | `runner/` | native |
| `ume-agentcontrol` | `agentcontrol/` | compiler |
| `ume-remote`, `ume-mcp` | `remote/`, `mcp/` | agentcontrol |

Each crate re-exports the modules of the crates below it, so a module reaches
another as `crate::project` wherever it lives, and the root library re-exports
every module under the path it had as one crate (`unity_micro_editor::native`).
Test helpers that tests of other crates share (`sysutil::testing`,
`runner::testing`, `agentcontrol::testing`) are behind each crate's `testing`
feature, which only dev-dependencies turn on.

The executable never invokes Python and has no
fallback: a path that is not supported fails explicitly. The C# runtime package
and SDK helpers remain necessary inside Unity.

`../unity-micro-editor` builds a stripped release binary into the user cache,
sets `UNITY_MICRO_EDITOR_ROOT` to the installation and runs it. `ume-base`
embeds `../engine-profile.json`.

## Layout

| Module | Responsibility |
|---|---|
| `yaml`, `cache`, `graph`, `project/` | Unity YAML, the authored source graph, scene revisions and the `Session` that prepares them |
| `authoring/` | Revision-checked source edits, journals, receipts and undo |
| `packages/` | Package resolution: manifest, registry and Git download, archive extraction |
| `runtimeartifact/` | Readers of the built-in lighting data of native artifacts |
| `editorbridge/`, `agentcontrol/` | Client of the Editor mailbox; Player control, journal and routing |
| `mcp/` | The MCP server over stdio and Streamable HTTP, on the `rmcp` crate |
| `remote/` | `remote_session`: Player link, H.264 encoding, the viewer page, its WebSocket and WebRTC (`str0m`) transports, the publish command. `native/nvenc` holds the C source that drives the NVIDIA encoder, built by `rust/crates/remote/build.rs` |
| `engine/` | The supported Unity releases: a project's `ProjectVersion.txt` selects one, the installation must report the same version, and each release implements its own worker messages, DataStore service and asset database writes (`unity6000_6.rs`) |
| `lmdb`, `protocol`, `registry` | The stopped-LMDB reader, the import worker transport (frames, blobs, decoded results), the owned `SourceAssetDB` |
| `native/` | Native import coordinator: the import worker, the warm scene worker and its project library, DataStore, and the content the Player loads |
| `compiler/` | Roslyn driver, script type inventory, Netcode post-processing, host staging |
| `sourcegraph/` | The Player assembly graph derived from `.asmdef`/`.asmref` and the engine profile |
| `hotreload/` | C# hot reload: builds of the patch builder and the applier, the documents of a code patch, one attempt against the running Player and the code state of a Player process |
| `runner/` | `run`: launcher, prepared revisions, patches, routes, the watcher, the command reader and the decisions of one watch window (`window`) |
| `agent/` | `auto-prepare`, `build-project`, component defaults and the other agent commands |
| `parity/` | The `parity-*` clients of `Integrations/EditorParity` |
| `util`, `paths`, `flags`, `cancel`, `jsondoc`, `sysutil` | Shared helpers, including the encodings described under "Formats" |

## Commands

The public commands are `run`, `mcp`, `auto-prepare`, `build-project`,
`resolve-packages`, `cache-component-defaults`, `project-import`,
`native-environment`, `source-graph`,
`compiler-source-prepare`, `compiler-prepare`, `prepare-project`,
`engine-profile-capture` and `version`. An unknown command prints the usage
line and exits with status 2.

Not listed in the usage line: `prepare` (one scene revision through `Session`),
`mcp-standalone` (source tools only, no runtime session), `shader-extract`,
`builtin-shader`, the `parity-*` clients, the experimental `game-managers`
(a Player's settings managers written from `ProjectSettings`, see
[docs/HOSTLESS_PLAYER.md](../docs/HOSTLESS_PLAYER.md)), and the diagnostics
`parse-yaml` and `authored-graph`, which print the parsed documents and the
authored graph as JSON.

## Formats

Cache keys, receipts and recorded test cases depend on the exact bytes this
crate writes, so several encodings are fixed and implemented here instead of
being left to a library's defaults:

- `jsondoc`: JSON with members in document order, sorted and HTML-escaped
  output, shortest round-trip float text, and fixed decode error texts.
- `authoring/yamlcodec`: the scalar resolver and emitter for authored Unity
  YAML.
- `sysutil`, `project/lexpath`: lexical path cleaning and file error texts.
- `compiler/nilable`: lists and maps that keep absent (`null`) apart from
  empty.

Changing any of them changes persisted hashes; prepared projects are then
prepared again.

## Checks

```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

Unit tests sit beside each module. `recorded-cases.json` (bridge and runner)
and `jsonschema-oracle.jsonl` (MCP argument validation) are recorded reference
cases: each holds inputs with the exact output or error expected for them. SDK
tests are skipped unless `UME_LIFECYCLE_SDK`, `UME_DECLARATION_SDK` or
`UME_NETCODE_SDK` names the matching Unity executable; see
[CONTRIBUTING.md](../CONTRIBUTING.md).

The SDK tests of hot reload (`hot_reload_*_sdk`, with `UME_LIFECYCLE_SDK`)
build the patch builder and compile the applier through the functions a
session uses, run the builder on compiled revisions of a fixture, and run the
C# harnesses `tests/HotReloadPatcherTests` and `tests/HotReloadRuntimeTests`
from a staging copy. The applier is compiled beside a game assembly whose
types without a namespace are named like those of the class library, and the
compiled revisions go through the lifecycle helper with a callback that has
no access modifier and follows `#region`. With `UME_HOT_RELOAD_HOST` naming the Player executable
of a prepared host, the applier is also compiled against a staged copy of
that host. None of them starts a Player.

## Known limits

- Where several faults occur at once, the one reported first follows a fixed
  order.
- Unity YAML containing NEL, LS or PS is refused.
- Comments, anchors and explicit tags on a YAML node that an edit would have to
  re-emit are refused with `unsupported_yaml_reencode`; merge keys are refused.
- Paths are UTF-8. Files with non-UTF-8 names are skipped.
- JSON nesting is limited to 512 levels in the bridge and controller files.
- The MCP wire layer belongs to `rmcp`: object member order outside the reply
  envelope and JSON-RPC error messages are its own. HTTP header and idle
  timeouts are hyper's defaults.
- The package resolver has no injectable HTTP client and refuses PAX sparse
  tar entries. A Git package at the repository root fails on
  `pax_global_header`.
- Replayed receipts lose precision on integers above 2^53.
- A project with an empty native catalog cannot replay its cached engine
  bootstrap.

## Status

Verified with Unity `6000.6.3f1`, one machine, one project:

- The checks above pass.
- The complete cycle was run for real on a private copy of
  `Samples/RawProject`. `auto-prepare` (Editor bootstrap, package resolution,
  host build, native catalog, SDK compilation, baseline) succeeded. An MCP
  stdio session then started the Mono Player, applied a scene edit as a live
  patch, recompiled and restarted on a C# edit (the Player log showed the new
  code), and stopped cleanly.
- Hot reload, without a Player: the SDK tests above pass. The patch builder,
  built by the session's function, patched a compiled fixture, refused an
  added type and restored a reverted edit through a whole attempt whose Player
  was a stand-in; the applier compiled against a staged copy of a prepared
  host with the bridge of this tree. The decisions of a watch window (patch,
  fallback, refusal, recompile, mixed changes) ran with stand-ins for the
  compiler, the importer and the Player. The lines that loop prints are read
  by the controller in its own tests, so the two sides are not tested against
  events written by hand.
- Hot reload with a real Player, on 2026-10-13, on private copies of
  `Samples/RawProject` with added scripts and scene objects. `run --watch` in
  both fallback modes and an MCP stdio session patched edited method bodies
  into the running Player (same PID, game state kept, the new behaviour in the
  Player log), refused edits outside the supported set with their reasons,
  restarted on `recompile`, and replaced a Player that did not answer.
  Patched in a Player: bodies of instance, static and struct methods, property
  accessors, constructors, lambdas, coroutines and the lifecycle methods the
  helper rewrites; added methods; an added instance and an added static field;
  a removed `Update`; a method that another thread called in a loop meanwhile;
  twenty edits in a row and 54 generations in one process. A body that uses
  generics over game and engine types, closures, an iterator, an `async`
  method, string and enum switches, array data and `stackalloc` printed the
  same result patched and in a restarted Player. `[InvokeOnHotReload]` and
  `[InvokeOnHotReloadLocal]` callbacks ran. A copy with a second assembly
  (`.asmdef`) was patched in both assemblies, and a member added to one and
  used from the other in the same change was refused. A session with
  `--no-hot-reload` showed the scene patch, the scene reload, the restart on a
  C# edit and the compile error and its repair as before the watch loop was
  rearranged. After the review of that stage, on the same day: a method made
  `public` gave `unchanged` with the builder's note, in the runner and in the
  state `wait_update` returns; a `[InvokeOnHotReloadLocal]` method ran again
  for the first, second and third edit of its lambda, and one on `OnEnable`
  for an edit of its body; a script with `#region` in front of a lifecycle
  method without access modifier compiled and ran; two files saved 0.4 s
  apart ended with one answer that was not marked `pending`, and one
  `wait_update` after two `script_edit` calls returned with both in the
  Player. After the second review, on the same day: one `wait_update` after
  two saves eight seconds apart returned after the second of them, as an
  error when that one was refused; an edit that lay only in a lambda of a
  generic method was refused (`generic_method`) and the Player kept the old
  lambda, where the builder had answered `unchanged`; a field whose accessibility
  decides what Unity serializes was refused (`serialization_changed`) in both
  directions; the reply to an edit of a running coroutine began with
  `attention` and listed the note under `newNotes`; a stack trace of a patched
  method named the file under `Assets`; a second `.asmdef` added to a ready
  session was prepared without a further call and the session continued on
  it; with the same sources saved again in a later server process, which the
  project was prepared for already, the session continued on that preparation
  after 7 s, and the removal of the `.asmdef` was prepared anew in 18 s.
- Hot reload with Netcode for GameObjects 2.13.2, on 2026-10-14, on a copy of
  `Samples/RawProject` with the package and a small networked game, as a host
  and a client Player on localhost, each in its own `run --watch` session. In
  both Players, without a restart and with the network session kept: ordinary
  bodies of a NetworkBehaviour and of plain classes, `Update`, `OnNetworkSpawn`
  (with `spawned_objects_keep_state`), an `OnValueChanged` handler, a lambda,
  an added method and field, and the bodies of a ServerRpc, a ClientRpc, two
  `Rpc` (one taking an `INetworkSerializable` struct) were patched, and every
  patched RPC was sent, received and run across the two processes, also
  between a patched and an unpatched Player in both roles. RPCs, and classes
  with registered serializers, declared in another order gave `unchanged`.
  Added, removed, renamed and re-attributed RPCs, changed parameters, added,
  removed and retyped network variables, a new serialized type, a class that
  became a NetworkBehaviour and a `__` method added to one were refused with
  their `netcode_*` codes while both Players kept running and replicating the
  old code, and `recompile` applied them. Through MCP: a patched RPC body, the
  refusal with `netcode_rpc_changed` and the call to make, the recompile, and
  the spawn note as `attention`.

Not verified: a project with native models, audio or UI through the whole
cycle; `build-project` against a real Editor; `project_tools`; attaching to an
interactive Editor; the GitHub Actions workflow itself. Of hot reload: content
routes or Addressables. Known: the fixed island address of a game method can
lie in a mapping of the Player. A `sub rsp, imm8` body is then reached through
the gate to an island anywhere in a 16 MiB window: with the fixed island of one
method forced taken, 38 of 40 fresh Players took its edit that way (the
applier before the change refused all 40), and without forcing 79 of 80 took
the edit (the fixed island was free in nearly all of them; one found its
whole window taken) (2026-10-14). The rest, a `sub rsp, imm8` body whose whole window is taken
(that method's window lay in a large mapping of the Player) and a `push rbp`
body or a body of an unrecognised first instruction whose fixed island is
taken (under 0.03 % of the methods of a running Player), is refused for the
life of that Player with `apply_failed` and `fullRecompileRequired`; a
recompile applies the edit. Of
Netcode: another package version, a dedicated server and distributed
authority. A stage 5 review ran host and two clients (Temp/e2e/nc5, 121
checks): NetworkList, a NetworkBehaviour whose base class is in another game
assembly, every universal RPC target and a first use of Netcode in a body
behaved in every Player as a restart on the new code would. Known not to
work in a session, with or without hot reload: a generic NetworkBehaviour
cannot be prepared unless it declares no lifecycle callback and a base class
declares `Awake` (the lifecycle rewrite's two refusals);
with scene management on (NGO's default) clients never synchronize, because
the session's scene has no build-settings path; without it, clients do not
find in-scene placed NetworkObjects.

Measured on 2026-10-08 (AMD Ryzen 7 5700X3D, rustc 1.99.0) by a client driving
the MCP server, median of five sessions: `session_control start` to Player
ready 10.06 s; scene edit to patch applied 0.24 s; C# edit to recompiled Player
ready 8.09 s; stop 1.27 s. `auto-prepare` from scratch took 414 s in a single
run that shared the machine with another session. Unity, the Mono Player and
Roslyn dominate these times; the tool's own share is small. These are the
times of the watch loop before hot reload. On 2026-10-14 a C# body edit in a
`run --watch` session reached the running Player in 0.33 s (median of 32),
and `hot_reload(recompile)` restarted it in 4.3 to 4.8 s;
[docs/HOT_RELOAD.md](../docs/HOT_RELOAD.md#measured) has these and the other
timings, and [docs/RUNNER.md](../docs/RUNNER.md#measured) says where the time
of an edit goes.
