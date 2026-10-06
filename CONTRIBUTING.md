# Working on Unity Micro Editor

Keep the tool independent of the game checkout. Resolve tool sources through the installation directory and accept target project paths as arguments. Optional tests that need real game assets must take an explicit project path.

The Go runner is the current implementation. Do not add Python to its build, launch, watch, import, or scene update path. Historical Python prototypes remain for reference.

Run these checks before committing:

```bash
go -C GoProject test -race ./...
go -C WorkerProbe test -race ./...
go -C GoProject vet ./...
go -C WorkerProbe vet ./...
./unity-micro-editor version
./build-host.sh --dry-run
```

SDK-only runtime checks use the matching Unity installation:

```bash
export UNITY_GO_LIFECYCLE_SDK="$HOME/Unity/Hub/Editor/6000.6.3f1/Editor/Unity"
export UNITY_GO_DECLARATION_SDK="$UNITY_GO_LIFECYCLE_SDK"
go -C WorkerProbe test -run 'SDK|Declaration' -v
```

Native import and Player tests require installed Unity binaries. The core Go tests use synthetic fixtures and run without Unity. The GitHub Actions workflow checks both Go modules; it does not claim to validate Unity execution.

Preserve authored `.meta` GUIDs and verify source values when adding scene or importer support. Unsupported profiles must return an error before publishing new content. Store reports, Player copies, caches, and screenshots under ignored `Temp`, `Library`, or `Builds` directories. Do not commit SDK binaries, resolved Unity package sources, game assets, or credentials.

Engine changes need a new verified profile, matching host build, and import/runtime comparisons. The current pinned version and the remaining compatibility limits are described in the main README.
