# Native UI and font imports

UXML, USS, TSS (themes) and TTF/OTF assets reach the runner in one of two ways:

- **Warm.** An unchanged source is extracted from the import cache the pinned
  Unity 6000.6.3f1 (45d8eee7de74) Editor left in the project's `Library`.
- **Cold.** A new or changed source, or one in a project no Editor opened, is
  imported by the standalone AssetImportWorker with UI Toolkit's own importers
  (see [Cold imports](#cold-imports)).

The warm route is tried first when the project has the Editor's databases. If
its exact checks fail, the source is imported cold and the event
`native-warm-ui-unavailable` gives the reason.

Before publishing an independent runtime file, the adapter verifies:

- Current source and metadata SHA256 and native XXH3-128 hashes.
- SourceAssetDB path, GUID, metadata hash and source hash.
- ArtifactDB CurrentRevisions and ImportAddressToImportResultIDs agreement.
- The exact source/GUID/metadata tuple in importer dependencies.
- The importer stats' exact Unity version and revision.
- The DataStore artifact's native XXH3-128 content key.
- Unchanged source files and database snapshots throughout extraction.

The native serialized file keeps every retained object's data and type trees.
Projection changes the target from Editor to Linux Player and removes known
editor importer object entries. UXML keeps its managed serialized-data type
metadata, external stylesheet references, VisualTreeAsset and inline StyleSheet.
USS keeps the StyleSheet's actual selectors, rules, values and font references.
Fonts keep the native Font, Material and Texture2D subassets. Dependencies are
validated and published before their consumers; runtime files and proofs live
under the runner's own `Temp` output.

Runtime compatibility must be checked in the actual Player through public
`ContentFile.GetObject`, exact type/ID readback, and `VisualTreeAsset.CloneTree`.
Format/hash verification alone does not prove UI rendering compatibility.

In the actual Player, the retained UXML loaded successfully but its external USS
slots were null, even though the original native PPtrs and dependency handles
were present. Changing the Editor external-reference kind from Meta to Normal
did not fix this. The runner therefore reads the verified file with Unity's
standalone `binary2text` and records the exact native UI PPtr slots, target GUIDs
and local IDs. Extraction proof covers the native file, reader executable, dump
and binding list. Every new process verifies these inputs again; no Editor,
asset importer or bundle builder is started for this step.

The runtime restores only those declared references through public
`JsonUtility`, including USS fonts and the local inline StyleSheet. Managed UXML
references are resolved by their registry ID rather than the dump's array
position: Unity's JSON can insert a null sentinel before the root record.
Final `Object.Instantiate` copies run normal StyleSheet `OnEnable` and VTA
initialization after reference restoration. Their native rules, selectors,
attributes and element scopes remain authored data. The cache owns these copies
and retains their dependencies; source assets and cached native bytes are not
modified. Missing or conflicting slots/targets fail before scene activation.

This route was checked in a GPU Player on a real UI scene: the visual tree exposed
its native stylesheets with matching content hashes, both were attached to the UI
root, and elements carrying a `display: none` class resolved to
`DisplayStyle.None`. It does not establish cold import support for changed
UXML/USS sources.

## Cold imports

The worker runs `UIElementsViewImporter`, `StyleSheetImporter`,
`ThemeStyleSheetImporter` and the font importer as the Editor does. Their
outputs pass through the same projection, `binary2text` binding list and
runtime restoration as warm artifacts.

A UI source is imported in one batch with the sources it names that are not
imported yet, the sources they name, and so on. A source names another:

- by GUID, as the Editor writes a reference;
- by a `url()` path relative to the source, or a document's `<Style src>` and
  `<Template src>`;
- by a path relative to the project (`/Assets/...` or `project://database/...`).

`unity-theme://` and other schemes name no source. Such a batch records each
import in the worker's ArtifactDB, because the importers load the style sheets
a source imports. Without that record, a theme's `@import` of a USS stayed
empty.

A theme holds the themes it imports, `unity-theme://default` included, with
their textures. The artifact therefore keeps `ThemeStyleSheet` objects (script
19202) and `Texture2D` objects with their pixels. The runtime prepares each
style sheet after the local sheets it links: an imported theme comes before
the theme that imports it.

A document's custom elements (`[UxmlElement]` classes, written with their
namespace's prefix such as `<d:SegmentBar>`) are created in a Player from the
serialized data the importer writes into the `VisualTreeAsset`. The importer
writes it only for a class it finds among the loaded assemblies; without the
class it logs "missing a UxmlElementAttribute", and the Player creates
nothing there. So the worker of a batch with a `.uxml` loads, from the
Editor compilation (`compile_editor_assemblies`), each assembly whose metadata
holds the name and the namespace of such an element, and what that assembly
refers to. For these assemblies, unlike the importers of packages, sources
under `Assets` are placed in the worker's project. The cache key of a
document includes the identity of that compilation. Dungeon's
`InventoryScreen.uxml` loaded `Dungeon.Runtime` and the 29 assemblies it
refers to, 1657 sources.

These importers add objects of the engine's script classes (`VisualTreeAsset`,
`StyleSheet`). `AddObjectToAsset` takes such an object only when its
`MonoScript` is in the register of scripts. The main Editor keeps that
register in the source database as `mem://Library/MonoManager.asset` under
`SerializedSingletons`, and a worker reads it from there. Without it, every
script-backed object is refused with "Failed to add object of type
`VisualTreeAsset`. Check that the definition is in a file of the same name",
the engine's own classes included.

The coordinator therefore writes the register into every source database it
makes. It lists the `MonoScript` objects of the installed Editor's
`Data/Resources/unity default resources` (30 in 6000.6.3f1, for example
`VisualTreeAsset` 11995 and `StyleSheet` 11998) as references into
`Library/unity default resources`. The Editor's own register also lists the
project's scripts. A worker finds those through the source registry, so they
are left out. With the register:

- `RuntimeDebugWindow.uxml` of SRP Core imports with the same import result
  as with the register a real Editor wrote.
- An `.inputactions` import keeps the same result ID as without it.

Run the optional real-project check of a stopped batch import with:

```bash
UNITY_WORKER_UI_PROJECT=/absolute/project \
UNITY_WORKER_BATCH_IMPORT_REPORT=/absolute/project/Library/UnityMicroEditor/imports/manual-01/result.json \
  cargo test --locked native_adoption_actual_stopped_batch_proves_every_artifact -- --nocapture
```

The native UI projection fixture test (`adapt_ui_retains_real_importer_payload_and_class_identity`
in `rust/tests/runtimeartifact.rs`) additionally accepts
`UNITY_WORKER_UI_FIXTURES` pointing at a directory containing the original
UXML/USS primary artifacts named by their source GUID plus `.bin`.
