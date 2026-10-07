# Public Unity Editor bridge

The Go client attaches to an idle Editor for an explicit project or starts and owns a batch Editor. The runtime package supplies the mailbox through public Unity APIs, independently of Coplay. Unity `6000.6.3f1` and Linux Mono are the verified profile.

```go
client, err := editorbridge.New(lifetime, editorbridge.Options{
    Project: absoluteProject,
    Editor: absoluteUnityExecutable,
})
if err != nil { return err }
defer client.Close()
connection, err := client.OwnProcessOrAttach(ctx)
response, err := client.Do(ctx, editorbridge.Request{
    ID: stableRequestID,
    Action: "scene_proof",
    Arguments: json.RawMessage(`{"scene":"Assets/Scenes/Main.unity"}`),
    ExpectedPID: connection.PID,
})
```

The package must already be installed. Installation and authored publication belong to the project coordinator. `Prepare` refreshes the project and exports its Player assembly graph and saved-scene proof. `SaveResult` publishes successful JSON into a new owned cache file and refuses replacement.

| Action | Public Editor operation |
| --- | --- |
| `status` | Process/profile, active target, compilation and dirty-scene state |
| `refresh` | `AssetDatabase.Refresh` |
| `export_graph` | `CompilationPipeline`, registered package roots, `MonoScript` GUID/type identities, source/meta/DLL hashes |
| `scene_proof` | Saved scene lighting identities and lightmap/probe counts |
| `component_schema` | Actual native component defaults and saved YAML template |
| `build_project` | Explicit saved scenes through `BuildPipeline.BuildPlayer` |
| `build_host` | Empty bootstrap Player with the project's native packages/graphics profile |
| `prefab_prepare` | Allowlisted public prefab/component operations in an isolated batch staging copy |
| `native_prepare` | Heavy asset bundles and cooked identity/type verification in an isolated batch staging copy |

Every response identifies `backend: unity_editor`. Build results include the native `BuildReport` summary and output path. Builds require an explicit saved scene list, idle Edit Mode, installed Linux64 support and a new output directory inside `Builds/UnityMicroEditor`. Source scenes are preserved; dirty or unsaved populated scenes prevent preparation/build operations. Public project import/build callbacks still execute as part of Unity's normal pipeline.

`build_host` creates exclusive temporary bootstrap/resources assets, filters gameplay assemblies from native build outputs through `IFilterBuildAssemblies`, retains the project's package assemblies and restores settings/scene setup afterward. Hosts always use the development agent profile. Authored preload identities are cleared only during host construction and restored before preparation continues. Settings persistence uses the public save callback to restrict writes to the PlayerSettings file. The SDK compiler subsequently supplies the instrumented gameplay DLLs and native Mono slots.

Automatic host preparation invokes `build_host` in a private project copy under the target's persistent owned cache. It snapshots all authored files and metadata in `Assets`, `Packages` and `ProjectSettings`, checks those roots afterward even on failure, and never publishes build-preprocessor changes back to the target. A warm Library copy is used only after the coordinator's own source Editor stops; an attached Editor stays running. The copy excludes tool stages, `PackageManager`, `ScriptAssemblies`, `Bee`, and top-level PID/lock files. Unity rebuilds that source-specific process state while retaining imported assets, package sources and shaders. An older seed policy requires a new operation ID. Native output files and the staging package profile are checked again after the stage Editor exits. Immutable receipts bind the source snapshot and output hashes for later replay. Explicit `build_project` is the separate normal Editor build proxy.

The graph includes public Player and Editor compilation defines, source digests and Editor MonoScript candidates. Public `AssemblyBuilder.defaultDefines` supplies the development profile without building an assembly. The SDK's Roslyn parser proves which exact declarations are active in each profile. Editor-only candidates are excluded, and their source/meta fingerprints remain tracked. Every active runtime candidate must then exist in the host's independently hashed native DLL and registered Mono slot. Editor and Player bytes may differ because of compilation defines; an absent required runtime type still rejects preparation.

Gameplay subclasses can inherit lifecycle messages from verified native package dependencies. The SDK generates source-owned wrappers that call accessible protected/public base methods after graph hydration, retaining the inherited execution order. Private native callbacks reject preparation. The approved package names become part of the immutable compiler configuration used for later edits and restarts.

Source preparation publishes the accepted DLL hashes, source set, GUID/type bindings and lifecycle proofs into its compiler baseline. It also records the verified NGO postprocessor input key, so a restart reuses processed DLLs while changed processor inputs require a rebuild. The manual `prepare-project --native-catalog <path>` route reuses an existing verified public native catalog without Editor recooking.

A project without gameplay C# assemblies produces an explicit `nativeOnly` compiler plan with the native registration digest. Package script GUID/type checks still apply, and native registration drift rejects a restart. Native package DLL fingerprints are checked against the active copied Player. Adding the first gameplay script requires a fresh public graph and native assembly slot preparation.

Prefab and native preparation require batch mode and `ProjectSettings/UnityMicroEditorStaging.json` with schema version 1 and a canonical existing `sourceProject` different from the staging project. They never publish back to authored sources. The coordinator validates source revisions, dependencies and staged output before publication. Native catalogs preserve registered package roots even when public discovery finds no assets needing a pack.

Mailbox files live under `Library/UnityMicroEditor/editor-bridge`, which survives Editor restarts. IDs bind exact request bytes, action, arguments, project/profile/executable and PID guard. The Editor moves a request to its immutable accepted receipt before executing. A completed response can be replayed across clients/restarts; an accepted ID with a missing response returns an unknown outcome and never executes again. Timeout/cancellation after delivery returns `result_pending` or `result_unknown`, with the original ID and receipt path for inspection. A new ID is a new operation.

Project and executable versions are checked before cache creation or authored package installation. Project version parsing accepts LF/CRLF while rejecting conflicting or duplicate declarations. Startup is serialized across clients, and asset import workers are excluded from Editor discovery. `Close` requests a public shutdown only for its captured owned batch process, then uses bounded process cleanup if unresponsive. It never stops an attached interactive Editor. An Editor stopped during an accepted operation leaves an unknown outcome.

Requests are bounded to 256 KiB, responses/catalogs to 32 MiB, and paths reject symlinks. Only the listed actions and the package's named public adapters are callable. The protocol provides no generic C# evaluation, CustomEditor, EditorWindow or EditMode invocation.

The default tests use real isolated helper subprocesses for ownership, retries, response loss, cancellation, process exit, startup locking and path/protocol bounds. Actual Editor checks require explicit disposable fixtures:

```bash
UNITY_EDITOR_BRIDGE_PROJECT=/absolute/disposable/project \
UNITY_EDITOR_BRIDGE_EDITOR=/absolute/6000.6.3f1/Editor/Unity \
UNITY_EDITOR_BRIDGE_SCENE=Assets/Scenes/Main.unity \
go test -race ./internal/editorbridge -run TestEditorBridgeActualEditor -count=1 -v
```

Optional integration flags: `UNITY_EDITOR_BRIDGE_COMPONENTS=1`, `UNITY_EDITOR_BRIDGE_BUILD_ACTION=build_project|build_host`, and `UNITY_EDITOR_BRIDGE_NATIVE=1` in a marked batch staging fixture. `UNITY_EDITOR_BRIDGE_NATIVE_ASSETS` adds a comma-separated native asset list; `UNITY_EDITOR_BRIDGE_NATIVE_EMPTY=1` verifies a scene whose public dependency closure needs no native pack. `UNITY_EDITOR_BRIDGE_NATIVE_ONLY=1` requires a project with zero gameplay assemblies and a nonempty native host graph.
