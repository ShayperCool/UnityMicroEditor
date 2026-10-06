# Raw scene preparation in Go

This package prepares Dungeon's `SampleScene` from authored `.unity` and `.prefab`
files without running Python, the Unity Editor or a Player. The existing C# raw
loader consumes its schema 1 `scene.json` without an adapter.

It currently starts from a validated scene manifest and a compatible cached native
import catalog. The catalog supplies FBX hierarchies, native object identities,
URP content and the imported asset packs already used by the host. This package
does not create those packs or replace the Unity import worker.

```go
session, err := goproject.NewSession(goproject.Options{
    Project:       projectRoot,
    Manifest:      baselineSceneJSON,
    NativeCatalog: nativeCatalogJSON,
})
session.Invalidate(changedPaths) // .meta and directory watcher events
prepared, err := session.Prepare(outputRoot)
// prepared.Manifest points to a new immutable revisions/<id>/scene.json.
```

The caller compiles C#, owns the native importer cache, sends the prepared revision
to the Player, and publishes its current revision after successful runtime loading.
`SetManagedBindings` updates DLL paths, script types and runtime initializers after
a verified compilation. Native source changes return `*NeedsImportError` before
publishing a scene. After importing and adapting the changed source, the caller
can supply `NativeReplacement` to `ReplaceNativeContent`. The session verifies
the source and metadata hashes, exact existing object IDs/types, runtime file and
resource hashes, then publishes its own `nativeContentFiles` overlay. It leaves
the original manifest and catalog unchanged. Resource mounting belongs to the
caller. Previously accepted files are checked again before each publication.

The standalone preparation command is useful for inspecting a revision:

```sh
cd $UNITY_MICRO_EDITOR_ROOT/GoProject
CGO_ENABLED=0 go build -trimpath -ldflags='-s -w' -o ../../../Temp/UnityPlay/go-prepare ./cmd/prepare
../../../Temp/UnityPlay/go-prepare \
  --project ../../.. \
  --manifest /absolute/path/to/validated/scene.json \
  --native-catalog /absolute/path/to/catalog.json \
  --output ../../../Temp/UnityPlay/go-project/output
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
must call `Invalidate`. Changed source files are reread, and all observed identities
are checked again before writing a revision. The input project's `Library` is never
read. Cached package sources must be declared in `catalog.sourceRoots`.

The optional full-scene checks compare every object identity, type, serialized
field and custom runtime prefab against the previously validated Dungeon manifest.
They also copy the source tree with XFS reflinks and change the copied SampleScene,
leaving the original Assets untouched:

```sh
go test ./...
go test -race ./...
go vet ./...
UNITY_PLAY_PARITY_PROJECT=/absolute/project \
UNITY_PLAY_PARITY_MANIFEST=/absolute/scene.json \
UNITY_PLAY_PARITY_CATALOG=/absolute/catalog.json \
  go test -run TestActualSampleScene -v
```

On the current Dungeon scene, the parity run reproduced 857 objects and 1,264
assets. A repeated preparation took about 220 ms with zero source reads and zero
YAML parses. These times cover preparation only, not C# compilation or Player
scene replacement.
