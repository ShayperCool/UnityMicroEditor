# Native import

Heavy content (models, audio, compressed textures) is imported by Unity's own AssetImportWorker, driven by this tool's coordinator. The main Editor process, its GUI and a pre-imported `Library` are not needed for a worker run. Scenes and authored prefabs do not go through this path; the runtime loader reads them from source.

Only Unity `6000.6.3f1` on Linux x64 is supported. The coordinator starts `Unity` in AssetImportWorker mode and a separate `Unity.DataStore` from the installed engine. The importers themselves stay inside Unity's executable.

## Commands

Import selected files into a new owned context:

```bash
unity-micro-editor project-import \
  --project /absolute/project \
  --output /absolute/project/Library/UnityMicroEditor/imports/manual-01 \
  --asset Assets/Audio/door.wav \
  --timeout 45s
```

`--asset` can be repeated. `--output` must be a new directory under the project's `Library/UnityMicroEditor` or `Temp`. `--timeout` covers the whole import and is capped at 5 minutes. The coordinator reads the real source and `.meta` bytes and their dependencies, builds its own SourceAssetDB, and extracts the worker's reply from its own DataStore. It checks native content hashes, SHA-256 and that the source inputs did not change. The result is `<output>/result.json`.

This imports the chosen files. It does not bootstrap a project, and it does not create the baseline or catalog that `run` needs.

Accept a successful import into a session's runtime cache without running the worker again:

```bash
unity-micro-editor native-adopt \
  --project /absolute/project \
  --baseline /absolute/.../current.json \
  --output /absolute/project/Library/UnityMicroEditor/Sessions/dev \
  --report /absolute/project/Library/UnityMicroEditor/imports/manual-01/result.json
```

`native-adopt` checks source, metadata and GUID again, along with every copied dependency, the current import environment and the native XXH3-128 and SHA-256 of each artifact. A report without a complete environment record is refused.

Print the current import environment fingerprint without starting any Unity process:

```bash
unity-micro-editor native-environment --project /absolute/project
```

## During a run

Before expanding a scene, the runner walks the GUID references of the selected scene, its runtime prefabs and their authored dependencies. New or changed FBX and WAV files, and PNG files that need native resize or compression, are imported in batches of 32 assets per worker. An unchanged baseline or cache entry is skipped, as is a PNG in the supported raw profile.

Each cache entry records an `environmentSha256`. The fingerprint covers the pinned protocol revision, the Unity path, the exact SHA-256 of `ProjectSettings.asset`, the parsed settings and the wire environment. An index with no fingerprint, or a different one, is skipped and its assets are imported again. Changing this environment while a runner is live requires restarting the runner.

## How an import works

1. The coordinator creates an isolated context with its own `Assets`, `ProjectSettings`, `Packages`, `Library` and `Logs`, and copies the exact source and `.meta` bytes into it.
2. The coordinator writes a minimal `SourceAssetDB` directly: GUIDs, paths, importer settings and XXH3-128 file hashes taken from the sources. Nothing is copied from an Editor.
3. It starts a DataStore with a unique namespace, accepts the worker's local TCP connection, and sends `Initialize`, `Prepare` and `ImportRequest`.
4. Requests use the primary importer ID `-1`, so the native pipeline picks the importer and reads its settings from `.meta`. Explicit importer IDs run an extra importer with default settings and are rejected by content validation.
5. The worker is stopped through the protocol, then the DataStore. It reads only the stopped store, following `ImportResponse.ArtifactBlobs[1]` → `ArtifactMetaInfo.Files[].Hash` → the stored file.

Worker parameters come from an exact copy of `ProjectSettings.asset`: color space, shader precision and the Linux graphics API override. The profile is limited to the Linux standalone target, with OpenGLCore and Vulkan as the verified automatic API set. Unsupported or ambiguous settings are refused. The applied values and the settings hash are written to `environment` in the report.

For each linked file the coordinator checks length, storage bounds and the native XXH3-128 content key, and records SHA-256. Serialized files are inspected with Unity's standalone `binary2text`, which does not start an Editor. Checked files are saved in `<output>/artifacts`, wire payloads in `<output>/frame-*.bin` and process logs in `<output>/logs`.

## Textures

TextureImporter receives the project's default texture compression format and an explicit record that no user overrides exist. DXTC is the verified default.

Cold import is limited to ordinary uncompressed textures. A compressed PNG uses an exact warm artifact from an existing Unity import; a changed source or `.meta` without such an artifact is refused. For BC7, the compressor setting recorded in the artifact database is checked against `EditorSettings.asset`, and its hash is part of the cache key.

## Mesh colliders

An Editor artifact for a mesh with `m_IsReadable=0` can hold vertex and index data but no cooked collision data for a Player. For a MeshCollider, the coordinator writes a separate immutable runtime file with `m_IsReadable`, `m_KeepVertices` and `m_KeepIndices` enabled on the originally unreadable mesh. The FBX, its `.meta`, the scene and the collider geometry are not changed, and the projection is proven byte-identical to the import apart from those three flags.

The loader calls the public `Physics.BakeMesh` with the authored `convex` and `cookingOptions` values, then `UploadMeshData(true)` restores the original unreadable state before user callbacks run. Bounding-box substitutes are not used.

## Animation clips

Unity has no importer output for a `.anim` file: the worker returns only the importer object, and the Editor derives the Player's clip data from the YAML during a build. The tool performs that derivation itself (`rust/src/playerbake/`), so a new or changed clip needs neither an Editor nor a worker. It is baked in about a millisecond and published as native content like an imported asset; a watched session applies it to the running Player.

A legacy clip keeps its authored curves. Any other clip gets the muscle clip Mecanim evaluates: streamed cubic segments, the constant clip, value deltas and bindings. The result is compared byte for byte with objects built by Unity `6000.6.3f1`: the fixtures under `rust/tests/testdata/playerbake`, and any project's clips through `UNITY_PLAYERBAKE_CLIP_CORPUS`.

A float curve on a field of a script and an animation event that passes a script are supported: the baked file carries a `MonoScript` object naming the class, which the Player ties to the loaded class. The session takes the class and its assembly from the compiler graph of its start; a script added later is unknown until the next session.

The baker refuses what it does not reproduce exactly, and the session reports the clip instead of publishing an approximation:

- humanoid (muscle) curves and root motion curves;
- float curves on a built-in component other than an Animator parameter or `GameObject.m_IsActive`;
- object reference curves, an event that passes an object other than a script, and an additive reference pose;
- weighted tangents and negative infinite tangents;
- a source larger than 256 MiB. A source of tens of megabytes is read whole and takes a few seconds and up to a few gigabytes of memory.

## Animator controllers

A `.controller` file holds Editor objects (state machines, states, transitions, blend trees) that do not exist in a Player. A build flattens them into one constant per controller; the tool does the same (`rust/src/playerbake/controller.rs`). A new or changed controller is baked and published like a clip, and so is every clip it plays, because a baked controller refers to baked clips. The Player then runs a controller that no Editor has seen. When a clip changes, the baked controllers that play it are baked again, since a controller holds facts of its clips.

The comparison with Unity `6000.6.3f1` covers every field of the built object, with two deliberate differences. The hash-to-name table has no fixed order in the engine and is compared as a set. `m_ControllerSize`, the memory the Player reserves before reading the constant, is an upper bound rather than the engine's figure: it stays above it for every reference controller.

Covered: layers with blending, weights, avatar masks and synced layers with their override motions; nested state machines with entry, exit and any-state transitions; conditions, mute and solo; parameters; 1D, simple directional and direct blend trees, nested.

An `.overrideController` is baked as well: its Player form holds the same references as the source, so it is published with the baked controller and clips it names, and baked again when one of them changes.

Refused, never approximated:

- freeform 2D blend trees;
- state machine behaviours;
- a clip stored inside a model, in a controller or as an override, and any clip the clip baker refuses;
- an avatar mask stored inside the controller.

Limits of the path as a whole:

- A controller built into the native catalog keeps pointing at the clips it was built with. A changed clip reaches an Animator only when its controller can be baked; otherwise the session reports `native-bake-refused` and the Animator keeps the catalog's clip. The same holds for an override controller built into the catalog.
- A changed avatar mask does not bake the controllers that use it again; edit or touch the controller.
- `auto-prepare` publishes its baseline without this path, so a preparation whose scene refers to a controller or clip missing from the native catalog still needs a refreshed catalog. A running session accepts such a source.

## UI and fonts

Unchanged UXML, USS and font assets are extracted from an existing Unity import cache rather than imported cold. See [UI_IMPORTS.md](UI_IMPORTS.md).

## Limits

- The wire protocol, LMDB schema and DataStore layout were derived for one Unity version and are not a stable public API. A worker version mismatch is refused, and another engine version needs its own verification.
- There is no proven general support for animation inside models, skinned meshes, complex model settings, arbitrary texture settings, other audio codecs, platform overrides, custom AssetPostprocessors or every dependency of an arbitrary project.
- A shader can be imported, but the result has no platform GPU programs and cannot be delivered to a Player without a separate bake.
- The store reader handles inline and concatenated storage, limits one file to 64 MiB and refuses other layouts. The LMDB reader targets the observed Linux x64 format.
- A worker is started per import batch. A persistent native service is not used.

## Code

The coordinator lives in `rust/src/native/`, with the wire format, LMDB reader and SourceAssetDB writer beside it:

| File | Responsibility |
| --- | --- |
| `rust/src/native/import.rs`, `adopt.rs`, `batch.rs`, `importer.rs`, `warm.rs` | Project import, adoption, cold batches and the runtime cache |
| `rust/src/native/probe.rs` | One AssetImportWorker run |
| `rust/src/registry.rs` | The owned SourceAssetDB and native source hashes |
| `rust/src/protocol.rs` | Wire format, requests, responses and `ArtifactMetaInfo` |
| `rust/src/native/datastore.rs` | DataStore namespace, readiness and shutdown |
| `rust/src/lmdb.rs` | Reading a stopped LMDB without a native library |
| `rust/src/native/artifact.rs` | Extracting stored files and checking content keys |
| `rust/src/native/environment.rs` | The import environment fingerprint |
| `rust/src/native/baked.rs`, `rust/src/playerbake/` | Assets baked from authored YAML without a worker |

`rust/src/runtimeartifact/` adapts imported serialized files for the Player.

The ordinary tests check protocol and database bounds, corrupted content and wrong settings without starting Unity.
