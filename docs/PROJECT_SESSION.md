# Scene preparation

The `authoring` module (`rust/crates/authoring/src/authoring/`) also performs revision-checked component and prefab source
edits, including nested/variant create, overrides, apply/revert and unpack. Native
default templates are supplied by a version-bound cache; these transactions do
not run Editor. See [AGENT_MCP.md](AGENT_MCP.md).

The `packages` module (`rust/crates/base/src/packages/`) resolves manifest and lock dependencies, embedded/file
packages, scoped registries, exact Git commits/subdirectories and built-in SDK
packages. Registry download and safe archive extraction run in-process. Existing
PackageCache directories are accepted only with source provenance checks; the
resolver never trusts a directory suffix as a version or commit proof. It writes
owned snapshots and a proposed lock under `Library/UnityMicroEditor/packages`,
preserving the authored manifest and lock. Offline mode requires verified local
sources. Use `unity-micro-editor resolve-packages --project /absolute/project`
(also `--editor`, `--offline`, `--current` and `--refresh-git`).

Player assembly graph derivation lives in `rust/crates/compiler/src/sourcegraph/`. It uses these resolved
roots and authored `.asmdef`/`.asmref` files, with a separately captured pinned
engine/native-host profile. Rider/Visual Studio packages use Unity's public
CompilationPipeline for their assembly inventories; our implementation derives
target ownership and references directly from source.

## The session

The `project` module (`rust/crates/base/src/project/`) publishes immutable revisions of a
scene manifest (`scene.json`) without running the Unity Editor or a Player. It
does not build the scene itself: the scene is written in the Player format by
Unity's import worker ([NATIVE_IMPORT.md](NATIVE_IMPORT.md#scenes-in-the-player-format)),
and the session keys that scene by its sources, checks what it is given and
writes the manifest that names it.

A session starts from a validated baseline manifest (schema 1, `supported`, the
same project root, a scene under `Assets`) and, optionally, the native catalog
of the preparation (same schema, engine version and project). The catalog
declares the package roots that sources may be read from (`sourceRoots`, each
under `Packages/`) and the hashes of the sources its content was made from.

```rust
use unity_micro_editor::project::{Options, Session};

let session = Session::new(Options {
    project: project_root,
    manifest: baseline_scene_json,
    native_catalog: native_catalog_json,
    ..Options::default()
})?;
session.invalidate(&changed_paths); // .meta and directory watcher events
let prepared = session.prepare(&output_root)?;
// prepared.manifest points to a new immutable revisions/<id>/scene.json.
```

`Options::scene` selects another authored scene on the same baseline, and
`Options::extra_assets` names the prefabs the game asks for by address, which
are written beside the scene.

## What `prepare` does

`Session::prepare` (`rust/crates/base/src/project/session.rs`):

1. Checks the retained built-in shaders the baseline names, indexes the GUIDs of
   the project's `.meta` files and hashes every source the baseline lists in
   `sourceDependencies`. A source the catalog recorded with another hash is a
   `NeedsImportError`.
2. Calls `native_scene_over` (`rust/crates/base/src/project/native_scene.rs`). It follows the
   GUIDs of the scene, the render pipeline asset, the preloaded assets, the
   addressed prefabs and every text asset they reach, and makes each file and its
   `.meta` a dependency of the revision. The hash of all of them is the scene's
   key. When the session does not hold a written scene for that key, it returns
   `NeedsNativeScene`. Otherwise it takes the written scene's content files and,
   through their references, the published shaders they need; a shader whose
   published file is missing or was made from another source, `.meta` or
   ingredient is a `NeedsImportError`. Every content file is checked: an
   absolute, clean, non-symlinked path outside the Editor's own `Library` (the
   tool's `Library/UnityMicroEditor` is allowed), with the recorded SHA-256.
3. Fills the manifest fields: `nativeScene` (the scene file, its hash, its
   external files in order and the aliases of the render pipeline asset, the
   preloaded assets and the addressed prefab roots), `nativeContentFiles`, and
   empty `objects`, `assets`, `importedAssetBindings`, `importedAssetPacks` and
   `renderPipelineGlobalSettings`. The manifest holds no object records.
4. `publish_revision` writes `contentHash` (the SHA-256 of the dependency list),
   `sourceDependencies`, `ignoredPrefabOverrideDiagnostics`, `projectRoot`,
   `requiresEditor: false` and `requiresLibrary: false`. It checks that no file
   the preparation read changed meanwhile (`source changed during preparation,
   retry`). A manifest that would equal the one published last but for its
   revision id is that revision again, and nothing is written. Otherwise it
   writes `revisions/<random id>/scene.json` as a new file.

The caller resolves the two errors and prepares again. The runner's
`NativeProjectImporter::prepare` (`rust/crates/native/src/native/importer.rs`) has the scene
worker write the scene for `NeedsNativeScene` and hands it over with
`Session::set_native_scene`; for `NeedsImportError` it builds the shader and
hands it over with `Session::replace_native_content`, which checks the source
and metadata hashes, that the objects are shaders with valid local IDs, the
file's hash and that the shaders' dependencies form no cycle. Any other source
named by `NeedsImportError` is refused: the session builds only shaders. The
original baseline and catalog are not changed.

The caller compiles C#, sends the prepared revision to the Player, and publishes
its current revision after successful runtime loading.
`Session::set_managed_bindings` updates DLL paths, script types and runtime
initializers after a verified compilation.

## Caches

The first preparation indexes metadata and hashes the source closure. Later
preparations retain that index and verify file identity using device, inode, size,
mtime and ctime. Text sources stay in memory. Metadata and directory watcher events
must call `Session::invalidate`. Changed source files are reread, and all observed identities
are checked again before writing a revision. The input project's `Library` is never
read. Cached package sources must be declared in `catalog.sourceRoots`.

`Session::read_identities`, `Session::guid_index_sha256` and
`Session::configuration_sha256` describe what a preparation was made of. The
importer records them after a publication, so that a start whose sources did
not change takes the revision without preparing ([RUNNER.md](RUNNER.md#a-start-whose-sources-did-not-change)).

## Authored YAML

The source tools read authored scenes and prefabs through the same module:
`rust/crates/base/src/project/expand.rs` expands nested prefabs and model hierarchies for
`scene_hierarchy` and the `authored-graph` command. This expansion is not used
to load a scene in the Player.

Unity writes a plain value such as `Enemy #1` without quotes and reads it back
whole; checked against `6000.6.3f1`, which also keeps `#lead` and `a  # b`. The
reader does the same. A comment remains what follows a number, a quoted value
or a flow value, and a line holding nothing else. A `#` in a plain value folded
over several lines is rejected. The authoring editor still locates values by
the YAML rule, so it reads such a name only up to the `#`.

An inherited object of a prefab has the local ID the Editor's import gives it
(compared with the pinned Editor's prefab artifacts): the ID of its stripped
document in that prefab, whatever that ID is, and otherwise the instance ID XOR
the ID the object has in the instance's source asset, without the sign bit.
When another document of the prefab already has that derived ID, the Editor
imports the object under an ID the sources do not determine. Such an object is
expanded, but has no local ID here, as has an object named by two stripped
documents; a reference to either through an enclosing prefab or scene is
refused as unresolved. Two objects without stripped documents whose derived IDs
are equal are refused, and the error names both.

## The `prepare` command

The standalone preparation command is useful for inspecting a revision:

```sh
cd $UNITY_MICRO_EDITOR_ROOT
./unity-micro-editor prepare \
  --project /absolute/project \
  --manifest /absolute/path/to/validated/scene.json \
  --native-catalog /absolute/path/to/catalog.json \
  --output /absolute/project/Library/UnityMicroEditor/revisions
```

It runs the session alone, without an importer: a scene the session does not
hold in the Player format ends it with `NeedsNativeScene` (`the scene ... is not
written in the Player format`). Use `run --prepare-only` for a revision with its
scene written.

## Limits

- A revision depends on prepared content: a validated baseline, the Editor
  compilation the worker loads to write the scene, and the shaders the session
  builds. Without them `prepare` fails with the error that names what is
  missing; it does not fall back to another way of loading the scene.
- The session does not validate component settings; the engine reads what the
  worker wrote, and the Player reports what it refuses.
- Every source the scene reaches is a dependency of the revision: an edit of any
  of them asks for a new write of the scene.
