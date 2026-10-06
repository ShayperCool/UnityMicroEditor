# Dungeon integration

Dungeon is a separate game project used for the SampleScene and multiplayer ship checks. It is not bundled with Unity Micro Editor.

Its Unity package manifest must reference this repository's runtime package. UPM resolves local `file:` dependencies relative to the game's `Packages` directory. For sibling checkouts named `Dungeon` and `UnityMicroEditor`, the entry is:

```json
"com.unitymicroeditor.runtime": "file:../../UnityMicroEditor/Packages/com.unitymicroeditor.runtime"
```

Worktrees can have another relative path. Use a path that resolves from that worktree's `Packages` directory. The existing `Dungeon-wt/unity-cli-play` worktree uses `file:../../../UnityMicroEditor/Packages/com.unitymicroeditor.runtime`.

Both projects must use the pinned engine and matching package profile. Changing the package location invalidates the previous compiler configuration, so refresh the game's prepared baseline before a source run. An immutable `--prepared` launch is a separate mode and does not prepare current project sources.

Run from any directory with explicit target paths:

```bash
/path/to/UnityMicroEditor/unity-micro-editor run \
  --project /path/to/Dungeon \
  --baseline /path/to/Dungeon/Temp/UnityPlay/hybrid-sample/current.json \
  --output /path/to/Dungeon/Temp/UnityPlay/micro-editor \
  --watch --fps 30
```

The historical Dungeon benchmarks and multiplayer proof describe their required prepared content and retained shader snapshots. They remain integration evidence, not a guarantee that a fresh game can boot without preparation.
