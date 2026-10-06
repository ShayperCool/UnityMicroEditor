# Native asset import for raw scenes

Unity imports models, clips, audio, materials, textures, input actions and pipeline assets in a separate tool-owned project. The raw loader still constructs scenes and authored prefab templates. No `.unity` or authored `.prefab` asset enters the native pack.

The Python API in `imported_assets.py` separates preparation, Editor execution and publication:

```python
from imported_assets import prepare_import, publish_import, recover_import

prepared = prepare_import(project, editor, ["Assets/Scenes/SampleScene.unity"], seed_library=True)
# Run prepared["command"] with prepared["environment"].
catalog = publish_import(prepared)

# If CLI was interrupted after Editor finished, publish that exact attempt.
catalog = recover_import(project / "Temp/UnityPlay/native-import")
```

The same recovery is available through CLI:

```bash
$UNITY_MICRO_EDITOR_ROOT/unity-play import-recover
# Or select another tool-owned cache.
$UNITY_MICRO_EDITOR_ROOT/unity-play import-recover --native-cache /path/to/native-import
```

`export_imported_assets` performs those steps and reuses a valid prior revision. `asset_paths` selects extra native roots. Scenes passed to this API supply dependencies and native lighting observations. They are opened briefly for public read-only metadata without entering Play Mode or saving them. Scene construction and iteration stay on the raw path.

The isolated `ImportProject` mirrors `Assets`, `Packages` and `ProjectSettings`. It retains its own incremental `Library`. Optional initial Library seeding requires the source Editor to be closed and uses XFS reflinks. Source asset files are copied, never linked or rewritten. Publication checks the complete source snapshot and all exported dependency hashes.

When exact registry versions and Git commits already exist in a resolved Unity cache, preparation copies them into immutable `PackageSources` snapshots. Only the isolated manifest receives `file:` overrides. Its sidecar keeps the original manifest and lock hashes, package IDs and Git commit hashes. Transitive packages enter the same snapshot. An unavailable exact package leaves the original mirrored manifest in place so UPM can resolve normally. Unity's package content fingerprint is distinct from the Git commit hash; matching a folder name alone does not establish Git provenance.

Each attempt stores `request.json` and the original `prepared.json` under `Attempts/<exportId>/` before launching Editor. The completion catalog returns that ID and the exact request SHA256. `pending.json` also records the prepared manifest checksum. Recovery requires an idle importer, the original unchanged source inventory, matching importer implementation, matching request/completion identity and valid native files. It never reconstructs an inventory from the current project or modifies the mirror. An incomplete result or an old export without persisted preparation cannot be recovered automatically. Failure preserves `current.json` and its published revision.

The exporter writes one `ImportedAssetRegistry` carrier. Every reference stores the original asset GUID and signed 64-bit local identifier as strings. Selection never depends on the object's name, so duplicate names and model subassets remain distinct. The native pack has a CRC, SHA256 and an immutable published path.

Render pipeline global settings also enter the pack as explicit named roots. `AssetBundleBuild.addressableNames` maps each exported main asset to its original logical source path, including when the exported object is a native clone. `globalSettingsAssets` records its GUID, fileID and address, and `addressableAssets` records named pack roots. This lets the loader register global shader settings through public GraphicsSettings APIs before loading the registry. Renderer features can then resolve those shaders during native `OnEnable`.

`modelPrefabs` describes each ModelImporter's hierarchy through public `SerializedObject` properties and persistent identifiers. The raw parser expands this graph and constructs its components itself. Mesh, Avatar and AnimationClip objects stay native.

Objects with native `HideFlags.DontSaveInBuild` never enter the runtime registry or `assets` catalog. Unity can also omit imported objects without that flag. The exporter reads the actual Linux pack through public `AssetBundle.LoadFromFile/LoadAsset<ImportedAssetRegistry>` APIs, comparing every cooked row with an immutable pre-build identity and concrete-type snapshot. If a cooked reference is null, it records the original flags and exact pack SHA/CRC as `nativeCookedOmission`, removes that identity from the source registry and performs an incremental second build in the same Editor process. Publication requires the second registry readback to contain no null objects. Source objects are reloaded through AssetDatabase after build/readback because Unity can invalidate their old handles.

`excludedRuntimeAssets` records original identity, path, type, name, numeric flags and the precise exclusion proof. `modelAnimationClips` records public flags for every imported model clip. Names such as `__preview__...` do not authorize exclusion. A real scene or prefab reference to any excluded ID remains a validation error.

CAS and KINEMATION animation ScriptableObjects are native clones inside the owned importer directory. `Object.Instantiate` and `EditorUtility.CopySerialized` preserve their curves and private serialized fields. Before creating or changing any clone, the exporter captures all original cross references and authored prefab bindings through public SerializedProperty paths. This preserves child bindings even when a parent's `OnValidate` changes referenced child clones. Cross references then point to other clones, and authored prefab references are cleared only on clones and returned as `deferredBindings`, using paths such as `modifiers.Array.data[0].characterPrefab`. The runtime restores those references to its own hydrated templates before releasing deferred ScriptableObject callbacks.

`PlayerSettings.GetPreloadedAssets` supplies extra native roots in their original order. This includes custom settings assets such as `VContainerSettings`, even when the selected scene does not reference them. Their authored root prefab references use the same clone/binding mechanism. An authored prefab preload supplies raw template metadata only. A scene preload is rejected.

The catalog also records all registered package source roots for script GUID resolution, imported model graphs, native lighting facts and public prefab observations. The parser accepts an already missing script only when public metadata matches the exact GameObject local identifier and component index. The loader omits that component and resolves its references to Unity null. Public `AddComponent` cannot create a native missing-script slot, so later `GetComponents` indices can differ. Stale array overrides are ignored only after a public `SerializedObject` observation verifies the effective array size and absent property. Scene lighting fields, script identifiers and prefab structural identities invalidate those proofs; a position or scalar edit does not.

Root discovery follows the entire authored graph through scenes, preloaded assets and referenced prefab/data graphs. This includes a preloaded `VContainerSettings` asset pointing to a raw root prefab, then a raw `DungeonSoundBank` and its WAV clips or mixer. Discovery selects heavy native roots without cooking the traversed scene, prefab or ordinary game data. The subsequent native dependency traversal skips authored prefabs. A final `AssetDatabase.GetDependencies` check on the actual carrier rejects any remaining prefab or scene dependency, including references retained inside a native controller or unsupported engine object. Such a failure leaves the prior revision unchanged.

Scene positions and ordinary game method edits do not change the native discovery key. A changed heavy source, importer `.meta`, actual native dependency, package version, importer code or relevant settings invalidates it. Plain Dungeon data assets remain on the raw path unless explicitly selected for native export.

Serialized ScriptableObject classes have a stricter code contract. Unity's dependency list includes their `.cs` files, and their full source hashes remain checked. Editing those files currently reruns native export, including method-only edits. Ordinary Dungeon MonoBehaviour method changes use offline compilation and reuse native content. A future relaxation must verify the Player serialization layout, field defaults and serialization/import callbacks before allowing cached data. No layout-compatible code heuristic is enabled.

The SampleScene export including native preloads and their heavy dependencies produced 13 550 runtime object records, 110 models, 14 deferred bindings and a 113.29 MiB pack after filtering Editor-only ShaderGraph/URP helper objects. Controllers and mixer objects implemented by Editor classes keep their Player-compatible runtime type names. This export took 80.95 seconds inside the exporter, excluding Editor startup. UPM package listing independently stalled for 137.97 seconds on `packages.unity.com` before using cached packages. Its output confirmed no lightmaps or baked renderers and contained no authored scene or prefab dependency. Full Player launch and iteration measurements are reported separately in the raw workflow documents.

The implementation still rejects native objects that retain authored prefab references outside the supported ScriptableObject clone graph, such as an unsupported controller graph. A new native module or stripped type may require rebuilding the reusable host. Native packs target the exact Unity version, Linux Mono Player and OpenGL Core profile.

Thirty-five automated Python checks cover cache reuse, immutable revisions, source races, interrupted recovery, completion identity, package snapshots, explicit global-settings addresses, exact build-exclusion flags and cooked readback proofs, duplicate identities, lighting/structural metadata invalidation and source isolation. The Editor helper also compiles against the installed Unity DLLs with warnings treated as errors. Actual scene compatibility is checked by the CLI integration and its Editor comparison probes.
