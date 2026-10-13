# Working on Unity Micro Editor

Keep the tool independent of the game checkout. Resolve tool sources through the installation directory and accept target project paths as arguments. Optional tests that need real game assets must take an explicit project path.

The CLI, the runner and the MCP server (on the `rmcp` crate) are one Rust crate: root `Cargo.toml`, sources under `rust/`. `rust-toolchain.toml` pins Rust 1.99.0, which is also what CI uses. Do not add Python to the tool's build, launch, watch, import, scene update or MCP execution path. Coplay's own Python server is confined to the isolated Editor comparison environment.

Persisted files, hashes and replies follow fixed byte formats that existing project caches and receipts depend on; [rust/README.md](rust/README.md) lists them. Change one only deliberately, together with its recorded test cases.

Run these checks before committing:

```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
./unity-micro-editor version
./build-host.sh --dry-run
```

SDK-only runtime checks use the matching Unity installation:

```bash
export UME_LIFECYCLE_SDK="$HOME/Unity/Hub/Editor/6000.6.3f1/Editor/Unity"
export UME_DECLARATION_SDK="$UME_LIFECYCLE_SDK"
cargo test --locked _sdk -- --nocapture
```

The hot reload tests among them (`hot_reload_*_sdk`) build the patch builder and the applier and run the C# harnesses `tests/HotReloadPatcherTests` and `tests/HotReloadRuntimeTests`; with `UME_HOT_RELOAD_HOST` naming the Player executable of a prepared host, the applier is also compiled against that host. None of them starts a Player. A change to `tools/HotReloadPatcher`, `tools/HotReloadRuntime`, `rust/src/hotreload`, the watch loop in `rust/src/runner` or the bridge's `CodeReload.cs` also needs a session with a real Player on a prepared copy of `Samples/RawProject`: `run --watch --hot-reload-fallback report --commands-stdin` with a patched body edit (`code-patched`, the same PID, the new behaviour in the Player log), a refused edit (`update-rejected` with `fullRecompileRequired`) and a `recompile` command; two saves about 100 ms apart (the answer to the first carries `pending: true`) and a `sync` command sent right after a save (`synced` follows the answer to that save); and a `--no-hot-reload` session for the paths that restart. A change to the controller's waits in `rust/src/agentcontrol` needs the same through `unity-micro-editor mcp`: two files saved several seconds apart and one `session_control(wait_update)` with the `lastSequence` from before the first, which must report the second. [docs/RUNNER.md](docs/RUNNER.md#hot-reload-of-c) describes the events.

Native import and Player tests require installed Unity binaries. Without those variables the SDK tests report that they were skipped and pass. The core tests use synthetic fixtures and run without Unity. The GitHub Actions workflow runs the three cargo checks; it does not claim to validate Unity execution. Source tests cover GUID/metadata preservation, stale revisions, concurrent writes, transaction recovery and undo. MCP tests exercise real stdio framing and Streamable HTTP initialization/tool calls, authentication and Origin/Host checks.

For a change to authoring, compiler graph or the runtime bridge, follow [AGENT_MCP.md](docs/AGENT_MCP.md) and [Integrations/EditorParity/README.md](Integrations/EditorParity/README.md). Compare exact authored inputs and complete Player/Editor readbacks, and keep compilation, runtime application and behavior assertions separate. Automatic preparation derives packages and the Player graph from source. Engine/host facts captured through public Editor APIs remain a separate pinned profile; source/native-resource scene proofs explicitly identify their own backend. Do not fabricate or relabel public Editor evidence. Keep the agent target's Editor closed while writing source and use an isolated reference copy for concurrent comparisons. Legacy `pause` stops scaled time; `simulation_pause` gates game stages, while `simulation_step` checks one Update, fixed tick and LateUpdate. Compare these semantics explicitly with native Editor Pause/Step.

Preserve authored `.meta` GUIDs and verify source values when adding scene or importer support. Unsupported profiles must return an error before publishing new content. Keep agent preparation, native staging and controller output under the explicit project's ignored `Library/UnityMicroEditor` subtree. Native import, extraction and adoption accept owned caches under that subtree or legacy `Temp`; existing catalogs are not migrated. Unity clears project `Temp` on restart. Regenerate the scene-hash-bound proof after scene edits and preserve independent public oracle exports in owned Library evidence before restarting the reference Editor. Native staging seeded from an owned stopped Editor cache is a warm copy, not a cold-import result. Other disposable reports and builds stay under ignored `Temp` or `Builds`. Do not commit SDK binaries, resolved Unity package sources, game assets, or credentials.

Engine changes need a new verified profile, matching host build, and import/runtime comparisons. The current pinned version and the remaining compatibility limits are described in the main README.
