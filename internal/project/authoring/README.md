# Authored project editing

`authoring.Store` binds to one explicitly supplied Unity project. Source writes
require the verified `6000.6.3f1` profile, an `expectedRevision`, and an
`operationId`. A source revision hashes both source and `.meta` bytes. Compilation,
asset import and Player readiness are separate results.

```go
store, err := authoring.NewStore(projectRoot)
source, err := store.Read(ctx, "Assets/Scenes/Main.unity")
result, err := store.Edit(ctx, authoring.EditRequest{
    Path: source.Path,
    ExpectedRevision: source.Revision,
    OperationID: "rename-2026-10-06-001",
    Operations: []authoring.Operation{{
        Action: "set_property", FileID: "100",
        Property: "m_Name", Value: "Agent test object",
    }},
})
```

`Inventory` pages all ordinary files under `Assets`, including binary assets.
It accepts `query`, `pathPrefix`, `extensions`, `offset`, `limit` (default 100,
maximum 500), `expectedRevision` and `includeSourceRevision`. Results report
`total`, `nextOffset`, path, GUID, size, modification time and identity issues.
Its snapshot revision binds names, file statistics and metadata bytes; it does
not hash the contents of every binary asset. When requested, supported text assets
on the page also include the exact source revision required for editing.

`ResolvedHierarchy` expands nested authored prefab instances and variants through
the same graph implementation used by our runtime scene loader. It pages and
filters the expanded objects, exposes source GUID/fileID provenance and dependency
revisions, and includes both the graph `revision` and the editable file's
`sourceRevision`. Composite IDs identify expanded objects for reading. Local IDs
in provenance identify editable source records. `Hierarchy` retains the raw,
unexpanded view and reports `unresolvedRootIds` when native SceneRoots references
a prefab instance whose local root owner is inherited or absent. It never
substitutes a fictitious GameObject ID `0`. Native model prefabs require a verified import catalog and are
outside standalone source graph resolution.

A prepared `Session.AuthoredGraph` and runtime source manifest expose
`ignoredPrefabOverrideDiagnostics` when a saved override targets an object
removed by a model reimport. Ignoring it requires a complete current public
native model inventory, verified raw importer output, and matching source and
`.meta` hashes. Each diagnostic retains the exact target GUID/fileID, origin
prefab/instance/property and source/meta/inventory SHA256. Existing imported
objects outside the expanded graph, incomplete inventories and unknown authored
prefab targets remain errors.

`set_property` changes scalar fields through paths such as `m_LocalPosition.x` or
`items[0].amount`, and explicitly replaces existing mappings and arrays at nested
paths. Scalar edits retain sibling whitespace and comments. Collection edits
replace only that field's span. Other documents, unknown fields, existing fileIDs
and metadata retain their bytes. Reference fileIDs must be decimal strings; GUIDs
must be 32 hexadecimal characters. JSON booleans serialize as Unity `0`/`1`.
Unity field types are validated separately by the compiler and Player.

`create_gameobject`, `delete_gameobject` and `reparent` maintain Transform parents,
children, component ownership and Unity 6 SceneRoots. Reparenting preserves local
transform. Deletion refuses surviving references in the file or other supported
text assets. Recursive deletion must be explicit. A prefab retains one root.
Creation keys let subsequent operations use `$key`, `$key.transform` and, for
prefab creation, `$key.instance`. A `rectTransform` creation requires explicit
complete layout fields or a revision-bound RectTransform template.

`add_component` attaches a MonoBehaviour through an existing `scriptPath` and
explicit serialized `fields`. Native components use a supported `componentType`
or `classId` from `NativeComponentTypes`, with explicit fields or a revision-bound
ordinary component template (`templatePath`, `templateFileId`,
`templateRevision`). There are no inferred native defaults. Templates with local
references require explicit replacements for those reference fields. Ownership
fields cannot be supplied or edited. Components with required companions need a
transaction containing all captured companion components. `AddNativeDefaults`
uses a complete public capture with its exact engine/context profile and content
hash. The MCP `component_add` reads this versioned cache and writes source in Go;
a missing cache produces an error and never starts an Editor.

`instantiate_prefab` writes a linked instance of an authored prefab or variant,
with `prefabPath`, `prefabRevision` and all `dependencyRevisions` bound before the
write. `set_prefab_override` and `revert_prefab_override` address `instanceId`,
`targetGuid`, `targetFileId` and a scalar/reference `property`. They update only the
instance modification array and preserve stripped records and unknown sibling
fields. Duplicate override entries remain errors. Direct writes to stripped
records, and structural reparent/deletion through unresolved prefab subtrees,
are refused.

`PrefabEdit` implements the MCP `prefab_edit` actions entirely in Go, with backend
`raw_source`. It creates a prefab from an ordinary scene subtree, preserving
nested instances; creates variants; instantiates authored prefabs/variants;
sets scalar/reference or compound leaf overrides; applies/reverts overrides;
and unpacks the outer layer or the complete resolved subtree. Added native
components use `DefaultsTemplate`, including required companions, and retain
their scene identities when apply replaces them with source aliases. Apply
publishes both source prefab and scene in one journal; default root name and
placement overrides stay local during apply/revert. Partial unpack retains the
immediate nested/base links; complete unpack materializes the resolved objects.

New prefab assets preserve ordinary source IDs and convert inherited stripped
aliases to their canonical prefab-local IDs, remapping internal references.
The modification writer emits Unity-compatible block arrays and retains unknown
sibling fields. Every resolved source dependency and metadata revision guards
publication. Immutable plans under `authoring/prefab-plans` support exact replay
after restart; changed arguments with the same ID fail. `BeforePublish` is a
nonserialized callback supplied anew for each write/retry, used by the MCP to
revalidate the native-default cache/context before creating its journal.

The supported source operations fail explicitly for unresolved/native-model
prefabs, outside scene references that cannot become prefab-local references,
inherited-parent instantiation, removed-component/GameObject or added-GameObject
apply/revert and partial unpack, and changing an inherited Transform into a
RectTransform. Creating a prefab directly from an inherited subtree requires an
explicit variant operation instead. Compound array overrides keep their existing
element count; resizing is not supported by this authoring API. These restrictions
do not invoke an Editor fallback. Native readback on an isolated project verifies
the supported nested/variant and added-component paths; see
[source prefab comparison](../../../Integrations/EditorParity/SOURCE_PREFAB_GO_PARITY.md).

`CreateAsset` creates an explicit Unity YAML `.mat`, `.asset` or `.prefab` and its
metadata with expected revision `absent`. Materials require a Material record;
ScriptableObjects require script-linked, owner-free records; prefabs require one
root. Shader and script defaults are supplied by the caller. `CreateScript`
creates a `.cs` and MonoImporter metadata together. The parent directory must
exist. Optional explicit GUIDs are checked for collisions; generated GUIDs are
also checked. `EditScript` preserves metadata exactly.

`PublishStaged` publishes an explicit source plan, including the Go prefab plan
or a separately prepared public Editor result. Its request has an `operationId`,
`inputs` of `{path, expectedRevision}`, and `outputs` of `{path, content, metadata?}`.
Every output must have an input guard. Read-only dependencies may include ordinary
files under `Assets`, `Packages` and `ProjectSettings`; `DependencyRevision`
streams their complete bytes. Existing metadata must remain byte-identical. New
files require revision `absent` and explicit valid metadata. Publication supports
up to 128 outputs, 4096 inputs and 64 MiB total output, requires existing parent
directories, and never implicitly deletes files. Reference validation considers
the complete proposed output set.

`Preview: true` returns proposed edits without source writes. `Undo` restores an
existing file while its revision still equals the original operation's result.
`UndoPublication` restores every changed existing file of a staged publication,
with an exact `expectedRevisions` map. Neither undo API erases newly created files.

Writers share a project-wide `flock` under
`Library/UnityMicroEditor/authoring`. A durable journal records old bytes, intended
bytes and the result before mutation. Retrying the exact request and operationId
returns the original result, including after process restart. Reusing an ID with
different arguments fails. Recovery recognizes fully written transactions or
restores partial transactions. External bytes matching neither version produce
`outcome_unknown` and require inspection.

Unity Editor and external readers do not participate in the lock. Multi-file
publication uses individual atomic renames, so those readers can observe an
intermediate file set. Close the authored target Editor during source writes and
use isolated copies for comparisons. Symlinks, traversal, SDK/package writes,
arbitrary Editor execution and release build orchestration are outside this
package's source mutation API.

```sh
go test -race ./internal/project/authoring
go vet ./...
```
