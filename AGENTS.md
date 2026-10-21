# Repository guidance

- The CLI, MCP server and importer coordinator are Rust (`Cargo.toml`, `rust/`). Keep Python out of their execution path. C# implements the Unity runtime bridge and SDK compiler helpers.
- Resolve tool sources from `UNITY_MICRO_EDITOR_ROOT` or the installation directory. Target game projects must be supplied explicitly. Do not infer a game project from this repository's parents.
- Unity `6000.6.3f1` is the verified profile. Engine upgrades require matching host, package locks, protocol checks, and runtime comparisons.
- Authored scenes, prefabs, materials, and `.meta` GUIDs are the source of truth. Do not silently overwrite them. Use the engine's own means without starting the Editor: Unity's standalone import worker writes scenes and content in the Player format, and the Player loads them with its own loader. Write our own code only for what the worker cannot do.
- Use public Unity/package APIs for runtime access. Do not reach into foreign private members. Validate imported identities and dependencies before publishing content.
- Build with `cargo build --profile fast` (`target/fast`) to check, run and measure; it rebuilds in seconds. Build `--release` (`target/release`, which game projects link to) only when the work is done.
- Keep caches, builds, reports, and SDK binaries out of Git. Run the relevant cargo and SDK checks from `CONTRIBUTING.md` before committing.
- Document limitations explicitly when a path depends on prepared game content or retained shader output.
- When working commit your progress using conventional commits, and push it to github
