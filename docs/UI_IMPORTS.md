# Native UI and font cache

The runner can extract unchanged UXML, USS and TTF/OTF assets from the pinned
Unity 6000.6.3f1 (45d8eee7de74) import cache without launching Editor. This is a
cache adapter, not a replacement UI importer. A newly added or changed source
fails if no exact current native artifact exists.

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

The standalone worker already reaches the real StyleSheetImporter, but its
`AssetImportContext.AddObjectToAsset(StyleSheet)` currently fails because the
worker does not have the required builtin scripting-class registration. The
exact-cache route preserves existing import results without hiding that gap.

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
