# The CLI crate

`unity-micro-editor` is one Rust crate: the manifest is the repository's root
`Cargo.toml`, the library and executable are under `rust/src/`, integration
tests under `rust/tests/`. The executable never invokes Python and has no
fallback: a path that is not supported fails explicitly. The C# runtime package
and SDK helpers remain necessary inside Unity.

`../unity-micro-editor` builds a stripped release binary into the user cache,
sets `UNITY_MICRO_EDITOR_ROOT` to the installation and runs it. The crate
embeds `../engine-profile.json`.

## Layout

| Module | Responsibility |
|---|---|
| `yaml`, `cache`, `graph`, `project/` | Unity YAML, the authored source graph, scene revisions and the `Session` that prepares them |
| `authoring/` | Revision-checked source edits, journals, receipts and undo |
| `packages/` | Package resolution: manifest, registry and Git download, archive extraction |
| `runtimeartifact/` | Readers of native artifacts and serialized files; shader extraction |
| `editorbridge/`, `agentcontrol/` | Client of the Editor mailbox; Player control, journal and routing |
| `mcp/` | The MCP server over stdio and Streamable HTTP, on the `rmcp` crate |
| `remote/` | `remote_session`: Player link, H.264 encoding, the viewer page, its WebSocket and WebRTC (`str0m`) transports, the publish command. `native/nvenc` holds the C source that drives the NVIDIA encoder, built by `build.rs` |
| `lmdb`, `protocol`, `registry` | The stopped-LMDB reader, the import worker wire format, the owned `SourceAssetDB` |
| `native/` | Native import coordinator: worker, DataStore, adoption and the runtime content cache |
| `compiler/` | Roslyn driver, script type inventory, Netcode post-processing, host staging |
| `sourcegraph/` | The Player assembly graph derived from `.asmdef`/`.asmref` and the engine profile |
| `runner/` | `run`: launcher, prepared revisions, patches, routes and the watcher |
| `agent/` | `auto-prepare`, `build-project`, component defaults and the other agent commands |
| `parity/` | The `parity-*` clients of `Integrations/EditorParity` |
| `util`, `paths`, `flags`, `cancel`, `jsondoc`, `sysutil` | Shared helpers, including the encodings described under "Formats" |

## Commands

The public commands are `run`, `mcp`, `auto-prepare`, `build-project`,
`resolve-packages`, `cache-component-defaults`, `project-import`,
`native-adopt`, `native-environment`, `source-graph`,
`compiler-source-prepare`, `compiler-prepare`, `prepare-project`,
`engine-profile-capture` and `version`. An unknown command prints the usage
line and exits with status 2.

Not listed in the usage line: `prepare` (one scene revision through `Session`),
`mcp-standalone` (source tools only, no runtime session), `shader-extract`,
`builtin-shader`, the `parity-*` clients, and the diagnostics `parse-yaml` and
`authored-graph`, which print the parsed documents and the authored graph as
JSON.

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

Not verified: a project with native models, audio, UI or Netcode through the
whole cycle; `build-project` against a real Editor; `project_tools`; attaching
to an interactive Editor; the GitHub Actions workflow itself.

Measured on 2026-10-08 (AMD Ryzen 7 5700X3D, rustc 1.99.0) by a client driving
the MCP server, median of five sessions: `session_control start` to Player
ready 10.06 s; scene edit to patch applied 0.24 s; C# edit to recompiled Player
ready 8.09 s; stop 1.27 s. `auto-prepare` from scratch took 414 s in a single
run that shared the machine with another session. Unity, the Mono Player and
Roslyn dominate these times; the tool's own share is small.
