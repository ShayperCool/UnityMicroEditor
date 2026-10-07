# Historical notes

These documents record the research and the Python prototype that preceded the Go runner. They are kept for the measurements and the reasoning behind the current design. They do not describe how to use the tool today; start from the [main README](../../README.md).

The prototype itself is no longer in the tree. `unity-play`, the `*.py` modules, their `tests/`, and the probes they drove (`CompatibilityProbe`, `ContentProbe`, `HybridProbe`, `HybridBenchmark`, `SceneReloadBenchmark`, `ImportedAssets`) can be read at commit `fa9c16f`. Commands in these documents that call them only work from that commit.

Links into `Logs/`, `Temp/` and `Assets/` point at the Dungeon checkout the measurements were taken in. Those files were never part of this repository.

- [RESEARCH.md](RESEARCH.md): running a Unity Player from a CLI and replacing managed code.
- [NO_EDITOR_CONTENT.md](NO_EDITOR_CONTENT.md): options for producing content without the Editor.
- [NATIVE_IMPORTERS.md](NATIVE_IMPORTERS.md): reusing Unity's native importers and import workers.
- [LEGACY_CLI.md](LEGACY_CLI.md): the Python `unity-play` CLI.
- [RAW_PROJECTS.md](RAW_PROJECTS.md): the first raw scene loader prototype.
- [BENCHMARKS.md](BENCHMARKS.md) and [RAW_BENCHMARKS.md](RAW_BENCHMARKS.md): measurements of the prototype on Dungeon.
