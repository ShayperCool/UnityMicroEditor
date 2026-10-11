# Raw scene preparation

The `authoring` module (`rust/src/authoring/`) also performs revision-checked component and prefab source
edits, including nested/variant create, overrides, apply/revert and unpack. Native
default templates are supplied by a version-bound cache; these transactions do
not run Editor. See [AGENT_MCP.md](AGENT_MCP.md).

The `packages` module (`rust/src/packages/`) resolves manifest and lock dependencies, embedded/file
packages, scoped registries, exact Git commits/subdirectories and built-in SDK
packages. Registry download and safe archive extraction run in-process. Existing
PackageCache directories are accepted only with source provenance checks; the
resolver never trusts a directory suffix as a version or commit proof. It writes
owned snapshots and a proposed lock under `Library/UnityMicroEditor/packages`,
preserving the authored manifest and lock. Offline mode requires verified local
sources. Use `unity-micro-editor resolve-packages --project /absolute/project`
(also `--editor`, `--offline`, `--current` and `--refresh-git`).

Player assembly graph derivation lives in `rust/src/sourcegraph/`. It uses these resolved
roots and authored `.asmdef`/`.asmref` files, with a separately captured pinned
engine/native-host profile. Rider/Visual Studio packages use Unity's public
CompilationPipeline for their assembly inventories; our implementation derives
target ownership and references directly from source.

The `project` module (`rust/src/project/`) prepares a scene from authored `.unity` and `.prefab` files without
running the Unity Editor or a Player. The existing C# raw
loader consumes its schema 1 `scene.json` without an adapter.

It currently starts from a validated scene manifest and a compatible cached native
import catalog. The catalog supplies FBX hierarchies, native object identities,
URP content and the imported asset packs already used by the host. This module
does not create those packs or replace the Unity import worker.

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

The caller compiles C#, owns the native importer cache, sends the prepared revision
to the Player, and publishes its current revision after successful runtime loading.
`Session::set_managed_bindings` updates DLL paths, script types and runtime initializers after
a verified compilation. Native source changes return `NeedsImportError` before
publishing a scene. After importing and adapting the changed source, the caller
can supply a `NativeReplacement` to `Session::replace_native_content`. The session verifies
the source and metadata hashes, exact existing object IDs/types, runtime file and
resource hashes, then publishes its own `nativeContentFiles` overlay. It leaves
the original manifest and catalog unchanged. Resource mounting belongs to the
caller. Previously accepted files are checked again before each publication.

Unity writes a plain value such as `Enemy #1` without quotes and reads it back
whole; checked against `6000.6.3f1`, which also keeps `#lead` and `a  # b`. The
reader does the same. A comment remains what follows a number, a quoted value
or a flow value, and a line holding nothing else. A `#` in a plain value folded
over several lines is rejected. The authoring editor still locates values by
the YAML rule, so it reads such a name only up to the `#`.

The standalone preparation command is useful for inspecting a revision:

```sh
cd $UNITY_MICRO_EDITOR_ROOT
./unity-micro-editor prepare \
  --project /absolute/project \
  --manifest /absolute/path/to/validated/scene.json \
  --native-catalog /absolute/path/to/catalog.json \
  --output /absolute/project/Library/UnityMicroEditor/revisions
```

Scene and prefab support includes nested prefabs and model hierarchies, stripped
object aliases, inherited IDs, scalar and object-reference overrides, array sizes,
added/removed components and GameObjects, prefab root naming and Unity 6 root
ordering. Custom raw ScriptableObjects are reread from source. A new script type
must be supplied by the compiler. A raw ScriptableObject must contain one class 114
document. Existing WAV updates are accepted after native importer adaptation.
Existing FBX updates additionally require an independently verified reference
graph and identical authored GameObject, Transform and renderer fields. Geometry
may change while IDs, types and those fields remain stable. Skinned/Animator
graphs, topology changes and other native asset profiles need a refreshed catalog.

Native dependency discovery scans bounded lines and hashes each entire source.
The ordinary text/YAML parser bound stays unchanged. Larger `.anim` sources may
be scanned up to 1 GiB only when a current public raw cooked catalog proves the
AnimationClip identity, exact source size/SHA256 and metadata SHA256, and its
registry packs match the baseline and current pack hashes. Discovery still
follows object references after the ordinary byte limit. An oversized source
requiring full YAML fallback is rejected; no large parsed graph or source buffer
is retained.

Native deferred prefab references enter the source asset closure only when their
exact owner is published as `imported`. The catalog supplies the complete binding
inventory; preparation selects the active owners, follows target prefab and
component references recursively, and publishes a deterministic
`importedAssetBindings` list. Authored ScriptableObjects use their current source
references. Targets must match the current GUID-to-path index and actual expanded
prefab object/alias IDs. Cycles are deduplicated, conflicting properties and
missing targets reject publication, and refreshed prefab roots replace stale
cached asset keys.
Existing raw material and texture descriptors can be reused unchanged, but this
package does not update them.

Missing script components and retained stale prefab overrides are skipped only
when the catalog contains the matching public Editor observations. Unknown
components, dangling references, duplicate GUIDs, recursive prefabs and Transform
cycles are errors. The Player remains responsible for validating the native
component settings it supports; this package is not a replacement for all Editor
validation.

The first preparation indexes metadata and hashes the source closure. Later
preparations retain that index and verify file identity using device, inode, size,
mtime and ctime. Parsed YAML stays in memory. Metadata and directory watcher events
must call `Session::invalidate`. Changed source files are reread, and all observed identities
are checked again before writing a revision. The input project's `Library` is never
read. Cached package sources must be declared in `catalog.sourceRoots`.

The optional full-scene checks compare every object identity, type, serialized
field and custom runtime prefab against a previously validated manifest of a real
project.
They also copy the source tree with XFS reflinks and change the copied scene,
leaving the original Assets untouched:

```sh
cargo test --locked --lib project::
UNITY_PLAY_PARITY_PROJECT=/absolute/project \
UNITY_PLAY_PARITY_MANIFEST=/absolute/scene.json \
UNITY_PLAY_PARITY_CATALOG=/absolute/catalog.json \
  cargo test --locked --lib project::tests::session::actual_sample_scene -- --nocapture
```

A repeated preparation of an unchanged project does no source reads and no YAML
parses.
