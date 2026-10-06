"""The raw route consumes source graphs and rejects incomplete imports."""
from pathlib import Path
import json
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import raw_scene

PREFAB_GUID = "12345678901234567890123456789012"
MATERIAL_GUID = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
TEXTURE_GUID = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
SCRIPT_GUID = "cccccccccccccccccccccccccccccccc"


def doc(class_id, file_id, name, fields, stripped=False):
    return f"--- !u!{class_id} &{file_id}{' stripped' if stripped else ''}\n{name}:\n{fields}\n"


def simple_graph(name="Cube", first=1, father=0):
    return doc(1, first, "GameObject", f"  m_Name: {name}\n  m_IsActive: 1\n  m_Component:\n  - component: {{fileID: {first+1}}}") + doc(4, first+1, "Transform", f"  m_GameObject: {{fileID: {first}}}\n  m_Father: {{fileID: {father}}}\n  m_Children: []\n  m_LocalPosition: {{x: 0, y: 0, z: 0}}\n  m_LocalRotation: {{x: 0, y: 0, z: 0, w: 1}}\n  m_LocalScale: {{x: 1, y: 1, z: 1}}")


def prefab(instance, guid=PREFAB_GUID, overrides="", parent=0):
    return doc(1001, instance, "PrefabInstance", f"  m_Modification:\n    m_TransformParent: {{fileID: {parent}}}\n    m_Modifications: {('[]' if not overrides else '')}\n{overrides}    m_RemovedComponents: []\n    m_RemovedGameObjects: []\n    m_AddedComponents: []\n    m_AddedGameObjects: []\n  m_SourcePrefab: {{fileID: 100100000, guid: {guid}, type: 3}}")


class RawSceneTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.project = Path(self.temp.name)
        (self.project / "Assets").mkdir()
        (self.project / "ProjectSettings").mkdir()
        (self.project / "ProjectSettings/ProjectVersion.txt").write_text("m_EditorVersion: 6000.6.3f1\n")
        self.scene = self.project / "Assets/Example.unity"

    def tearDown(self):
        self.temp.cleanup()

    def asset(self, name, content, guid, importer=""):
        path = self.project / "Assets" / name
        path.write_text(content)
        Path(str(path) + ".meta").write_text(f"fileFormatVersion: 2\nguid: {guid}\n{importer}")
        return path

    def validate(self, text, native_catalog=None):
        self.scene.write_text(text)
        return raw_scene.validate_scene(self.project, "Example", native_catalog)

    def test_no_library_large_fileids_and_string_names(self):
        report = self.validate(simple_graph("on", first=9223372036854775800))
        self.assertTrue(report["supported"], report["diagnostics"])
        self.assertFalse((self.project / "Library").exists())
        self.assertEqual(report["objects"][0]["fileId"], "9223372036854775800")
        self.assertEqual(json.loads(report["objects"][0]["dataJson"])["m_Name"], "on")
        self.assertEqual(report["objects"][1]["gameObjectId"], "9223372036854775800")
        self.assertFalse(report["requiresCookedContent"])

    def test_dangling_component_is_error_and_does_not_publish(self):
        self.validate(simple_graph().replace("fileID: 2", "fileID: 404", 1))
        output = self.project / "manifest.json"
        output.write_text("previous valid revision")
        with self.assertRaises(raw_scene.RawSceneError) as caught:
            raw_scene.prepare_scene(self.project, "Example", output)
        self.assertIn("dangling-reference", [d["code"] for d in caught.exception.report["diagnostics"]])
        self.assertEqual(output.read_text(), "previous valid revision")

    def test_native_class_without_adapter_is_explicit_error(self):
        report = self.validate(simple_graph() + doc(59, 3, "HingeJoint", "  m_GameObject: {fileID: 1}"))
        self.assertFalse(report["supported"])
        self.assertIn("native-class", [d["code"] for d in report["diagnostics"]])

    def test_prefab_instances_get_distinct_ids_and_overrides(self):
        self.asset("Cube.prefab", simple_graph(), PREFAB_GUID)
        overrides = f"    - target: {{fileID: 1, guid: {PREFAB_GUID}, type: 3}}\n      propertyPath: m_Name\n      value: Changed\n      objectReference: {{fileID: 0}}\n    - target: {{fileID: 2, guid: {PREFAB_GUID}, type: 3}}\n      propertyPath: m_LocalPosition.x\n      value: 7.5\n      objectReference: {{fileID: 0}}\n"
        report = self.validate(prefab(100, overrides=overrides) + prefab(200))
        self.assertTrue(report["supported"], report["diagnostics"])
        objects = {o["fileId"]: json.loads(o["dataJson"]) for o in report["objects"]}
        self.assertEqual(set(objects), {"100:1", "100:2", "200:1", "200:2"})
        self.assertEqual(objects["100:1"]["m_Name"], "Changed")
        self.assertEqual(objects["200:1"]["m_Name"], "Cube")
        self.assertEqual(objects["100:2"]["m_LocalPosition"]["x"], 7.5)
        self.assertEqual(objects["100:2"]["m_GameObject"]["fileID"], "100:1")

    def test_prefab_imported_root_uses_filename_before_instance_name_overrides(self):
        graph = simple_graph("Different authored root").replace("m_Children: []", "m_Children:\n  - {fileID: 12}")
        graph += simple_graph("Keep authored child", 11, 2)
        source = self.asset("File prefab.prefab", graph, PREFAB_GUID)
        original = source.read_bytes()
        overrides = f"    - target: {{fileID: 1, guid: {PREFAB_GUID}, type: 3}}\n      propertyPath: m_Name\n      value: Explicit instance name\n      objectReference: {{fileID: 0}}\n"
        report = self.validate(prefab(100) + prefab(200, overrides=overrides))
        self.assertTrue(report["supported"], report["diagnostics"])
        objects = {obj["fileId"]: json.loads(obj["dataJson"]) for obj in report["objects"]}
        self.assertEqual(objects["100:1"]["m_Name"], "File prefab")
        self.assertEqual(objects["200:1"]["m_Name"], "Explicit instance name")
        self.assertEqual(objects["100:11"]["m_Name"], "Keep authored child")
        self.assertEqual(objects["200:11"]["m_Name"], "Keep authored child")
        self.assertEqual(source.read_bytes(), original)

    def test_prefab_imported_root_name_preserves_nested_instance_override(self):
        self.asset("Base.prefab", simple_graph("Different authored root"), PREFAB_GUID)
        nested_guid = "dddddddddddddddddddddddddddddddd"
        override = f"    - target: {{fileID: 1, guid: {PREFAB_GUID}, type: 3}}\n      propertyPath: m_Name\n      value: Nested instance name\n      objectReference: {{fileID: 0}}\n"
        container = simple_graph("Different container root", 11) + prefab(100, overrides=override, parent=12)
        self.asset("Container.prefab", container, nested_guid)
        report = self.validate(prefab(300, nested_guid))
        self.assertTrue(report["supported"], report["diagnostics"])
        objects = {obj["fileId"]: json.loads(obj["dataJson"]) for obj in report["objects"]}
        self.assertEqual(objects["300:11"]["m_Name"], "Container")
        self.assertEqual(objects["300:100:1"]["m_Name"], "Nested instance name")

    def test_nested_prefab_stripped_alias_and_parent(self):
        self.asset("Cube.prefab", simple_graph(), PREFAB_GUID)
        nested_guid = "dddddddddddddddddddddddddddddddd"
        stripped = doc(4, 42, "Transform", f"  m_CorrespondingSourceObject: {{fileID: 2, guid: {PREFAB_GUID}, type: 3}}\n  m_PrefabInstance: {{fileID: 100}}", True)
        self.asset("Nested.prefab", prefab(100) + stripped, nested_guid)
        scene_stripped = doc(4, 55, "Transform", f"  m_CorrespondingSourceObject: {{fileID: 42, guid: {nested_guid}, type: 3}}\n  m_PrefabInstance: {{fileID: 300}}", True)
        report = self.validate(prefab(300, nested_guid) + scene_stripped + simple_graph("Child", 11, 55))
        self.assertTrue(report["supported"], report["diagnostics"])
        transform = next(o for o in report["objects"] if o["fileId"] == "12")
        self.assertEqual(json.loads(transform["dataJson"])["m_Father"]["fileID"], "300:100:2")

    def test_prefab_cycles_and_structural_overrides_rejected(self):
        self.asset("Cycle.prefab", prefab(100), PREFAB_GUID)
        report = self.validate(prefab(200))
        self.assertIn("prefab-cycle", [d["code"] for d in report["diagnostics"]])
        self.asset("Cycle.prefab", simple_graph(), PREFAB_GUID)
        report = self.validate(prefab(200).replace("m_RemovedComponents: []", f"m_RemovedComponents:\n    - {{fileID: 2, guid: {PREFAB_GUID}}}"))
        self.assertIn("prefab-structural-override", [d["code"] for d in report["diagnostics"]])

    def test_removed_component_prunes_source_component_list_and_nulls_references(self):
        graph = simple_graph().replace("  - component: {fileID: 2}", "  - component: {fileID: 2}\n  - component: {fileID: 3}")
        graph += doc(33, 3, "MeshFilter", "  m_GameObject: {fileID: 1}\n  m_Mesh: {fileID: 10202, guid: 0000000000000000e000000000000000, type: 0}")
        self.asset("Cube.prefab", graph, PREFAB_GUID)
        scene = prefab(100).replace("m_RemovedComponents: []", f"m_RemovedComponents:\n    - {{fileID: 3, guid: {PREFAB_GUID}, type: 3}}")
        report = self.validate(scene)
        self.assertTrue(report["supported"], report["diagnostics"])
        objects = {obj["fileId"]: json.loads(obj["dataJson"]) for obj in report["objects"]}
        self.assertNotIn("100:3", objects)
        self.assertEqual(objects["100:1"]["m_Component"], [{"component": {"fileID": "100:2"}}])
        self.assertEqual((self.project / "Assets/Cube.prefab").read_text(), graph)

    def test_added_component_resolves_stripped_owner_and_preserves_component_order(self):
        self.asset("Cube.prefab", simple_graph(), PREFAB_GUID)
        scene = prefab(100).replace("m_AddedComponents: []", f"m_AddedComponents:\n    - targetCorrespondingSourceObject: {{fileID: 1, guid: {PREFAB_GUID}, type: 3}}\n      insertIndex: -1\n      addedObject: {{fileID: 60}}")
        scene += doc(1, 50, "GameObject", f"  m_CorrespondingSourceObject: {{fileID: 1, guid: {PREFAB_GUID}, type: 3}}\n  m_PrefabInstance: {{fileID: 100}}", True)
        scene += doc(33, 60, "MeshFilter", "  m_GameObject: {fileID: 50}\n  m_Mesh: {fileID: 10202, guid: 0000000000000000e000000000000000, type: 0}")
        report = self.validate(scene)
        self.assertTrue(report["supported"], report["diagnostics"])
        objects = {obj["fileId"]: json.loads(obj["dataJson"]) for obj in report["objects"]}
        self.assertEqual(objects["60"]["m_GameObject"]["fileID"], "100:1")
        self.assertEqual([entry["component"]["fileID"] for entry in objects["100:1"]["m_Component"]], ["100:2", "60"])

    def test_added_gameobject_uses_transform_reference_and_insertion_index(self):
        self.asset("Cube.prefab", simple_graph(), PREFAB_GUID)
        scene = prefab(100).replace("m_AddedGameObjects: []", f"m_AddedGameObjects:\n    - targetCorrespondingSourceObject: {{fileID: 2, guid: {PREFAB_GUID}, type: 3}}\n      insertIndex: 0\n      addedObject: {{fileID: 12}}") + simple_graph("Added", 11)
        report = self.validate(scene)
        self.assertTrue(report["supported"], report["diagnostics"])
        objects = {obj["fileId"]: json.loads(obj["dataJson"]) for obj in report["objects"]}
        self.assertEqual(objects["12"]["m_Father"], {"fileID": "100:2"})
        self.assertEqual(objects["100:2"]["m_Children"], [{"fileID": "12"}])

    def test_removed_gameobject_removes_descendants_and_parent_child_entries(self):
        graph = simple_graph().replace("m_Children: []", "m_Children:\n  - {fileID: 12}") + simple_graph("Child", 11, 2)
        graph = graph.replace("m_GameObject: {fileID: 11}\n  m_Father: {fileID: 2}\n  m_Children: []", "m_GameObject: {fileID: 11}\n  m_Father: {fileID: 2}\n  m_Children:\n  - {fileID: 22}") + simple_graph("Grandchild", 21, 12)
        self.asset("Cube.prefab", graph, PREFAB_GUID)
        scene = prefab(100).replace("m_RemovedGameObjects: []", f"m_RemovedGameObjects:\n    - {{fileID: 11, guid: {PREFAB_GUID}, type: 3}}")
        report = self.validate(scene)
        self.assertTrue(report["supported"], report["diagnostics"])
        self.assertEqual({obj["fileId"] for obj in report["objects"]}, {"100:1", "100:2"})
        self.assertEqual(json.loads(report["objects"][1]["dataJson"])["m_Children"], [])

    def test_prefab_array_resize_retains_source_and_accepts_nested_struct_overrides(self):
        data = {"layers": [{"name": "base", "weight": 0.5, "chain": []}]}
        raw_scene._Reader.apply_override(data, "layers.Array.size", "2", {"fileID": "0"})
        raw_scene._Reader.apply_override(data, "layers.Array.data[1].name", "extra", {"fileID": "0"})
        raw_scene._Reader.apply_override(data, "layers.Array.data[1].chain.Array.size", "1", {"fileID": "0"})
        raw_scene._Reader.apply_override(data, "layers.Array.data[1].chain.Array.data[0].index", "17", {"fileID": "0"})
        self.assertEqual(data["layers"], [{"name": "base", "weight": 0.5, "chain": []}, {"name": "extra", "weight": 0.5, "chain": [{"index": 17}]}])
        with self.assertRaises(ValueError):
            raw_scene._Reader.apply_override(data, "layers.Array.size", "-1", {"fileID": "0"})
        with self.assertRaises(KeyError):
            raw_scene._Reader.apply_override(data, "typo.scalar", "1", {"fileID": "0"})

    def test_native_catalog_exposes_all_exact_subobject_ids_and_keeps_raw_scene(self):
        self.asset("Model.fbx", "heavy source", PREFAB_GUID)
        model_graph = simple_graph("Imported").replace("  - component: {fileID: 2}", "  - component: {fileID: 2}\n  - component: {fileID: 3}")
        model_graph += doc(33, 3, "MeshFilter", f"  m_GameObject: {{fileID: 1}}\n  m_Mesh: {{fileID: 4300000, guid: {PREFAB_GUID}, type: 3}}")
        model_docs = raw_scene.parse_unity_yaml(Path("synthetic-model"), model_graph)
        catalog = {"schemaVersion": 1, "engineVersion": "6000.6.3f1", "packs": [{"bundlePath": "/cache/heavy.bundle", "crc": 7, "sha256": "abc", "registryAsset": "assets/registry.asset"}],
                   "assets": [{"guid": PREFAB_GUID, "fileId": str(i), "classId": 43, "path": "Assets/Model.fbx", "typeName": "UnityEngine.Mesh", "name": str(i)} for i in (4300000, 4300001)],
                   "modelPrefabs": [{"guid": PREFAB_GUID, "path": "Assets/Model.fbx", "rootFileId": "1", "objects": model_docs}]}
        scene = prefab(100) + doc(4, 50, "Transform", f"  m_CorrespondingSourceObject: {{fileID: 2, guid: {PREFAB_GUID}, type: 3}}\n  m_PrefabInstance: {{fileID: 100}}", True) + simple_graph("Authored child", 11, 50)
        report = self.validate(scene, catalog)
        self.assertTrue(report["supported"], report["diagnostics"])
        objects = {obj["fileId"]: json.loads(obj["dataJson"]) for obj in report["objects"]}
        self.assertEqual(objects["12"]["m_Father"], {"fileID": "100:2"})
        self.assertEqual(objects["100:3"]["m_Mesh"]["fileID"], "4300000")
        self.assertEqual([asset["fileId"] for asset in report["assets"]], ["4300000", "4300001"])
        self.assertEqual(report["importedAssetPacks"], catalog["packs"])
        self.assertEqual(self.scene.read_text(), scene)
        # The imported graph, rather than scene source, owns the mesh reference.
        catalog["modelPrefabs"][0]["objects"][-1]["data"]["m_Mesh"]["fileID"] = "4300002"
        bad = self.validate(scene, catalog)
        self.assertIn("asset-fileid", [diagnostic["code"] for diagnostic in bad["diagnostics"]])

    def test_native_catalog_version_duplicates_and_source_path_mismatch_are_errors(self):
        self.asset("Sound.wav", "audio source", PREFAB_GUID)
        asset = {"guid": PREFAB_GUID, "fileId": "8300000", "classId": 83, "path": "Assets/Sound.wav", "typeName": "UnityEngine.AudioClip", "name": "Sound"}
        catalog = {"schemaVersion": 1, "engineVersion": "other-version", "assets": [asset]}
        report = self.validate(simple_graph(), catalog)
        self.assertIn("native-catalog-version", [diagnostic["code"] for diagnostic in report["diagnostics"]])
        catalog["assets"].append(asset)
        with self.assertRaises(raw_scene.RawSceneError):
            self.validate(simple_graph(), catalog)
        catalog["assets"] = [dict(asset, path="Assets/Different.wav")]
        audio = doc(82, 3, "AudioSource", f"  m_GameObject: {{fileID: 1}}\n  m_audioClip: {{fileID: 8300000, guid: {PREFAB_GUID}, type: 3}}")
        report = self.validate(simple_graph() + audio, catalog)
        self.assertIn("native-catalog-path", [diagnostic["code"] for diagnostic in report["diagnostics"]])

    def test_runtime_prefab_references_emit_own_template_graph_and_typed_aliases(self):
        self.asset("Cube.prefab", simple_graph(), PREFAB_GUID)
        self.asset("Probe.cs", "namespace Example { public class Probe : UnityEngine.MonoBehaviour {} }", SCRIPT_GUID)
        text = simple_graph().replace("  - component: {fileID: 2}", "  - component: {fileID: 2}\n  - component: {fileID: 3}")
        text += doc(114, 3, "MonoBehaviour", f"  m_GameObject: {{fileID: 1}}\n  m_Script: {{fileID: 11500000, guid: {SCRIPT_GUID}, type: 3}}\n  template: {{fileID: 2, guid: {PREFAB_GUID}, type: 3}}")
        report = self.validate(text)
        self.assertTrue(report["supported"], report["diagnostics"])
        template = next(asset for asset in report["assets"] if asset["guid"] == PREFAB_GUID)
        self.assertEqual(template["kind"], "prefab")
        self.assertEqual(template["rootFileId"], "1")
        self.assertEqual({obj["fileId"] for obj in template["objects"]}, {"1", "2"})
        self.assertIn({"fileId": "2", "objectId": "2"}, template["objectAliases"])

    def test_explicit_native_package_root_resolves_script_without_project_library(self):
        with tempfile.TemporaryDirectory() as directory:
            package = Path(directory)
            (package / "Probe.cs").write_text("namespace Example { public class Probe : UnityEngine.MonoBehaviour {} }")
            (package / "Probe.cs.meta").write_text(f"guid: {SCRIPT_GUID}\nMonoImporter:\n  executionOrder: -400\n")
            catalog = {"schemaVersion": 1, "sourceRoots": [{"path": "Packages/com.example", "source": str(package)}]}
            text = simple_graph() + doc(114, 3, "MonoBehaviour", f"  m_GameObject: {{fileID: 1}}\n  m_Script: {{fileID: 11500000, guid: {SCRIPT_GUID}, type: 3}}")
            report = self.validate(text, catalog)
            self.assertTrue(report["supported"], report["diagnostics"])
            self.assertFalse((self.project / "Library").exists())
            script = next(asset for asset in report["assets"] if asset["guid"] == SCRIPT_GUID)
            self.assertEqual(script["path"], "Packages/com.example/Probe.cs")
            self.assertEqual(script["executionOrder"], -400)
            dependency = next(item for item in report["sourceDependencies"] if item["path"] == script["path"])
            self.assertEqual(dependency["sourcePath"], str(package / "Probe.cs"))

    def test_imported_asset_prefab_bindings_register_own_template_and_validate_identity(self):
        self.asset("Cube.prefab", simple_graph(), PREFAB_GUID)
        self.asset("Animation.asset", "heavy animation data", MATERIAL_GUID)
        catalog = {"schemaVersion": 1,
                   "assets": [{"guid": MATERIAL_GUID, "fileId": "11400000", "path": "Assets/Animation.asset", "classId": 114, "typeName": "AnimationData", "name": "Animation"}],
                   "deferredBindings": [{"ownerGuid": MATERIAL_GUID, "ownerFileId": "11400000", "propertyPath": "target", "targetGuid": PREFAB_GUID, "targetFileId": "2", "targetPath": "Assets/Cube.prefab"}]}
        report = self.validate(simple_graph(), catalog)
        self.assertTrue(report["supported"], report["diagnostics"])
        self.assertEqual(report["importedAssetBindings"], catalog["deferredBindings"])
        owner = next(asset for asset in report["assets"] if asset["guid"] == MATERIAL_GUID)
        self.assertEqual((owner["kind"], owner["fileId"]), ("imported", "11400000"))
        template = next(asset for asset in report["assets"] if asset["guid"] == PREFAB_GUID)
        self.assertEqual(template["kind"], "prefab")
        catalog["deferredBindings"][0]["targetFileId"] = "404"
        bad = self.validate(simple_graph(), catalog)
        self.assertIn("native-binding-fileid", [item["code"] for item in bad["diagnostics"]])

    def test_hybrid_raw_scriptable_objects_require_verified_lifecycle_scope(self):
        self.asset("Probe.cs", "namespace Example { public class Probe : OwnBase { private void OnEnable() {} } }", SCRIPT_GUID)
        self.asset("Settings.asset", doc(114, 11400000, "MonoBehaviour", f"  m_Script: {{fileID: 11500000, guid: {SCRIPT_GUID}, type: 3}}\n  value: 7"), MATERIAL_GUID)
        scene = simple_graph() + doc(114, 3, "MonoBehaviour", f"  m_GameObject: {{fileID: 1}}\n  m_Script: {{fileID: 11500000, guid: {SCRIPT_GUID}, type: 3}}\n  settings: {{fileID: 11400000, guid: {MATERIAL_GUID}, type: 2}}")
        legacy = self.validate(scene)
        self.assertFalse(legacy["supported"])
        report = self.validate(scene, {"schemaVersion": 1})
        self.assertTrue(report["supported"], report["diagnostics"])
        self.assertTrue(report["requiresScriptableObjectBarrier"])

    def test_nested_prefab_inherited_ids_resolve_without_stripped_yaml_documents(self):
        self.asset("Base.prefab", simple_graph(first=31), PREFAB_GUID)
        nested_guid = "dddddddddddddddddddddddddddddddd"
        self.asset("Variant.prefab", prefab(100), nested_guid)
        # Unity's inherited IDs also exist for objects absent from stripped YAML.
        inherited_root = str(100 ^ 31)
        inherited_transform = str(100 ^ 32)
        overrides = f"    - target: {{fileID: {inherited_root}, guid: {nested_guid}, type: 3}}\n      propertyPath: m_Name\n      value: Own instance\n      objectReference: {{fileID: 0}}\n"
        scene = prefab(200, nested_guid, overrides=overrides)
        scene += doc(4, 50, "Transform", f"  m_CorrespondingSourceObject: {{fileID: {inherited_transform}, guid: {nested_guid}, type: 3}}\n  m_PrefabInstance: {{fileID: 200}}", True)
        scene += simple_graph("Own child", 11, 50)
        report = self.validate(scene)
        self.assertTrue(report["supported"], report["diagnostics"])
        objects = {obj["fileId"]: json.loads(obj["dataJson"]) for obj in report["objects"]}
        self.assertEqual(objects["200:100:31"]["m_Name"], "Own instance")
        self.assertEqual(objects["12"]["m_Father"], {"fileID": "200:100:32"})

    def test_scene_roots_accept_prefab_instance_ids(self):
        self.asset("Cube.prefab", simple_graph(), PREFAB_GUID)
        scene = prefab(100) + simple_graph("First", 11)
        scene += doc(1660057539, 999, "SceneRoots", "  m_Roots:\n  - {fileID: 12}\n  - {fileID: 100}")
        report = self.validate(scene)
        self.assertTrue(report["supported"], report["diagnostics"])
        roots = next(obj for obj in report["objects"] if obj["classId"] == 1660057539)
        self.assertEqual(json.loads(roots["dataJson"])["m_Roots"], [{"fileID": "12"}, {"fileID": "100:2"}])
        self.assertEqual(report["objects"][0]["fileId"], "11")

    def test_deleted_prefab_target_requires_complete_editor_identity_observation(self):
        self.asset("Cube.prefab", simple_graph(), PREFAB_GUID)
        overrides = f"    - target: {{fileID: 404, guid: {PREFAB_GUID}, type: 3}}\n      propertyPath: m_Name\n      value: Retained old override\n      objectReference: {{fileID: 0}}\n"
        scene = prefab(100, overrides=overrides)
        observed = {"guid": PREFAB_GUID, "path": "Assets/Cube.prefab", "fileIds": ["1", "2"], "complete": False}
        catalog = {"schemaVersion": 1, "prefabObservations": [observed]}
        self.assertFalse(self.validate(scene, catalog)["supported"])
        observed["complete"] = True
        report = self.validate(scene, catalog)
        self.assertTrue(report["supported"], report["diagnostics"])
        self.assertIn("prefab-stale-target-override", [item["code"] for item in report["diagnostics"]])
        observed["fileIds"].append("404")
        self.assertFalse(self.validate(scene, catalog)["supported"])

    def test_lighting_dummy_requires_editor_zero_lightmap_and_baked_renderer_observation(self):
        scene = simple_graph() + doc(157, 77, "LightmapSettings", "  m_LightingDataAsset: {fileID: 20201, guid: 0000000000000000f000000000000000, type: 0}")
        observed = {"scenePath": "Assets/Example.unity", "lightmapsCount": 0, "bakedRenderersCount": 0, "capturedInPlayMode": False}
        catalog = {"schemaVersion": 1, "nativeSceneMetadata": [observed]}
        self.assertFalse(self.validate(scene)["supported"])
        report = self.validate(scene, catalog)
        self.assertTrue(report["supported"])
        self.assertTrue(report["emptyBuiltinLightingDataVerified"])
        observed["bakedRenderersCount"] = 1
        self.assertFalse(self.validate(scene, catalog)["supported"])
        observed["bakedRenderersCount"] = False
        self.assertFalse(self.validate(scene, catalog)["supported"])

    def test_imported_model_asset_local_mesh_refs_become_native_guid_refs(self):
        self.asset("Model.fbx", "heavy source", PREFAB_GUID)
        graph = simple_graph().replace("  - component: {fileID: 2}", "  - component: {fileID: 2}\n  - component: {fileID: 3}")
        graph += doc(33, 3, "MeshFilter", "  m_GameObject: {fileID: 1}\n  m_Mesh: {fileID: 4300000}")
        catalog = {"schemaVersion": 1, "assets": [{"guid": PREFAB_GUID, "fileId": "4300000", "path": "Assets/Model.fbx", "classId": 43, "typeName": "UnityEngine.Mesh", "name": "Mesh"}],
                   "modelPrefabs": [{"guid": PREFAB_GUID, "path": "Assets/Model.fbx", "rootFileId": "1", "objects": raw_scene.parse_unity_yaml(Path("synthetic"), graph)}]}
        report = self.validate(prefab(100), catalog)
        self.assertTrue(report["supported"], report["diagnostics"])
        mesh = next(obj for obj in report["objects"] if obj["classId"] == 33)
        self.assertEqual(json.loads(mesh["dataJson"])["m_Mesh"], {"fileID": "4300000", "guid": PREFAB_GUID, "type": 3})

    def test_missing_script_requires_native_owner_and_slot_observation_and_resolved_script_cancels_it(self):
        graph = simple_graph().replace("  - component: {fileID: 2}", "  - component: {fileID: 2}\n  - component: {fileID: 3}")
        graph += doc(114, 3, "MonoBehaviour", f"  m_GameObject: {{fileID: 1}}\n  m_Script: {{fileID: 11500000, guid: {SCRIPT_GUID}, type: 3}}\n  retainedSetting: 7")
        self.asset("Cube.prefab", graph, PREFAB_GUID)
        observed = {"guid": PREFAB_GUID, "path": "Assets/Cube.prefab", "complete": False,
                    "fileIds": ["1", "2"], "missingComponents": [{"gameObjectFileId": "1", "componentIndex": 1, "observedMissing": True}]}
        catalog = {"schemaVersion": 1, "prefabObservations": [observed]}
        self.assertFalse(self.validate(prefab(100), {"schemaVersion": 1})["supported"])
        report = self.validate(prefab(100), catalog)
        self.assertTrue(report["supported"], report["diagnostics"])
        component = next(obj for obj in report["objects"] if obj["classId"] == 114)
        self.assertTrue(component["skipMissingScript"])
        self.assertEqual(json.loads(component["scalarDataJson"])["retainedSetting"], 7)
        self.assertEqual([entry["component"]["fileID"] for entry in json.loads(report["objects"][0]["dataJson"])["m_Component"]], ["100:2", "100:3"])
        self.assertIn("missing-script-component", [item["code"] for item in report["diagnostics"]])
        self.asset("Probe.cs", "public class Probe : UnityEngine.MonoBehaviour {}", SCRIPT_GUID)
        restored = self.validate(prefab(100), catalog)
        self.assertTrue(restored["supported"], restored["diagnostics"])
        self.assertFalse(next(obj for obj in restored["objects"] if obj["classId"] == 114)["skipMissingScript"])
        observed["missingComponents"][0]["componentIndex"] = 0
        (self.project / "Assets/Probe.cs").unlink()
        invalid = self.validate(prefab(100), catalog)
        self.assertFalse(invalid["supported"])

    def test_native_array_staleness_proof_matches_exact_source_override(self):
        self.asset("Probe.cs", "public class Probe : UnityEngine.MonoBehaviour {}", SCRIPT_GUID)
        graph = simple_graph().replace("  - component: {fileID: 2}", "  - component: {fileID: 2}\n  - component: {fileID: 3}")
        graph += doc(114, 3, "MonoBehaviour", f"  m_GameObject: {{fileID: 1}}\n  m_Script: {{fileID: 11500000, guid: {SCRIPT_GUID}, type: 3}}\n  weights: [1, 2]")
        self.asset("Cube.prefab", graph, PREFAB_GUID)
        overrides = f"    - target: {{fileID: 3, guid: {PREFAB_GUID}, type: 3}}\n      propertyPath: weights.Array.size\n      value: 1\n      objectReference: {{fileID: 0}}\n    - target: {{fileID: 3, guid: {PREFAB_GUID}, type: 3}}\n      propertyPath: weights.Array.data[1]\n      value: 9\n      objectReference: {{fileID: 0}}\n"
        self.asset("Variant.prefab", prefab(100, overrides=overrides), MATERIAL_GUID)
        proof = {"path": "Assets/Variant.prefab", "instanceFileId": "100", "targetGuid": PREFAB_GUID,
                 "targetFileId": "3", "propertyPath": "weights.Array.data[1]", "arrayPath": "weights", "observedArraySize": 1, "propertyExists": False}
        catalog = {"schemaVersion": 1, "ignoredPrefabOverrides": [proof]}
        self.assertFalse(self.validate(prefab(200, MATERIAL_GUID))["supported"])
        report = self.validate(prefab(200, MATERIAL_GUID), catalog)
        self.assertTrue(report["supported"], report["diagnostics"])
        self.assertIn("prefab-stale-array-override", [item["code"] for item in report["diagnostics"]])
        proof["targetFileId"] = "4"
        self.assertFalse(self.validate(prefab(200, MATERIAL_GUID), catalog)["supported"])

    def test_hybrid_project_settings_preserve_preload_order_global_pipeline_refs_and_source_hashes(self):
        self.asset("Preload.asset", "native preload", MATERIAL_GUID)
        self.asset("Pipeline.asset", "native pipeline", PREFAB_GUID)
        self.asset("Globals.asset", "native global settings", TEXTURE_GUID)
        catalog = {"schemaVersion": 1, "assets": [{"guid": guid, "fileId": "11400000", "classId": 114, "path": path, "typeName": "UnityEngine.ScriptableObject", "name": path}
                   for guid, path in [(MATERIAL_GUID, "Assets/Preload.asset"), (PREFAB_GUID, "Assets/Pipeline.asset"), (TEXTURE_GUID, "Assets/Globals.asset")]],
                   "preloadedAssets": [{"guid": MATERIAL_GUID, "fileId": "11400000"}, {"guid": PREFAB_GUID, "fileId": "11400000"}]}
        player = self.project / "ProjectSettings/ProjectSettings.asset"
        player.write_text(doc(129, 1, "PlayerSettings", f"  preloadedAssets:\n  - {{fileID: 11400000, guid: {MATERIAL_GUID}, type: 2}}\n  - {{fileID: 11400000, guid: {PREFAB_GUID}, type: 2}}"))
        (self.project / "ProjectSettings/GraphicsSettings.asset").write_text(doc(30, 1, "GraphicsSettings", f"  m_CustomRenderPipeline: {{fileID: 0}}\n  m_RenderPipelineGlobalSettingsMap:\n    UnityEngine.Rendering.Universal.UniversalRenderPipeline: {{fileID: 11400000, guid: {TEXTURE_GUID}, type: 2}}"))
        (self.project / "ProjectSettings/QualitySettings.asset").write_text(doc(47, 1, "QualitySettings", f"  m_CurrentQuality: 0\n  m_QualitySettings:\n  - name: Mobile\n    customRenderPipeline: {{fileID: 0}}\n  - name: PC\n    customRenderPipeline: {{fileID: 11400000, guid: {PREFAB_GUID}, type: 2}}\n  m_PerPlatformDefaultQuality:\n    Standalone: 1"))
        before = player.read_bytes()
        report = self.validate(simple_graph(), catalog)
        self.assertTrue(report["supported"], report["diagnostics"])
        self.assertEqual(report["preloadedAssets"], [{"guid": MATERIAL_GUID, "fileId": "11400000", "type": 2}, {"guid": PREFAB_GUID, "fileId": "11400000", "type": 2}])
        self.assertEqual(report["renderPipelineGlobalSettings"], [{"pipelineTypeName": "UnityEngine.Rendering.Universal.UniversalRenderPipeline", "guid": TEXTURE_GUID, "fileId": "11400000"}])
        self.assertEqual(report["renderPipelineGuid"], PREFAB_GUID)
        self.assertEqual(report["qualityName"], "PC")
        self.assertEqual({dependency["path"] for dependency in report["projectSettingDependencies"]}, {"ProjectSettings/ProjectVersion.txt", "ProjectSettings/ProjectSettings.asset", "ProjectSettings/GraphicsSettings.asset", "ProjectSettings/QualitySettings.asset"})
        self.assertEqual(player.read_bytes(), before)
        catalog["preloadedAssets"].reverse()
        self.assertIn("native-preload-consistency", [item["code"] for item in self.validate(simple_graph(), catalog)["diagnostics"]])

    def test_project_preload_invalid_native_fileid_is_rejected(self):
        self.asset("Preload.asset", "native preload", MATERIAL_GUID)
        catalog = {"schemaVersion": 1, "assets": [{"guid": MATERIAL_GUID, "fileId": "11400000", "classId": 114, "path": "Assets/Preload.asset", "typeName": "UnityEngine.ScriptableObject", "name": "Preload"}]}
        (self.project / "ProjectSettings/ProjectSettings.asset").write_text(doc(129, 1, "PlayerSettings", f"  preloadedAssets:\n  - {{fileID: 404, guid: {MATERIAL_GUID}, type: 2}}"))
        report = self.validate(simple_graph(), catalog)
        self.assertFalse(report["supported"])
        self.assertIn("project-settings-fileid", [item["code"] for item in report["diagnostics"]])

    def test_project_preload_nested_raw_asset_reference_fileid_is_validated(self):
        self.asset("Preload.cs", "public class Preload : UnityEngine.ScriptableObject {}", SCRIPT_GUID)
        self.asset("Preload.asset", doc(114, 11400000, "MonoBehaviour", f"  m_Script: {{fileID: 11500000, guid: {SCRIPT_GUID}, type: 3}}\n  sound: {{fileID: 404, guid: {TEXTURE_GUID}, type: 3}}"), MATERIAL_GUID)
        self.asset("Sound.wav", "imported audio", TEXTURE_GUID)
        catalog = {"schemaVersion": 1, "assets": [{"guid": TEXTURE_GUID, "fileId": "8300000", "classId": 83, "path": "Assets/Sound.wav", "typeName": "UnityEngine.AudioClip", "name": "Sound"}]}
        (self.project / "ProjectSettings/ProjectSettings.asset").write_text(doc(129, 1, "PlayerSettings", f"  preloadedAssets:\n  - {{fileID: 11400000, guid: {MATERIAL_GUID}, type: 2}}"))
        report = self.validate(simple_graph(), catalog)
        self.assertFalse(report["supported"])
        errors = [item for item in report["diagnostics"] if item["code"] == "asset-fileid"]
        self.assertEqual(len(errors), 1)
        self.assertIn("sound requests invalid fileID 404", errors[0]["message"])
        self.assertEqual(errors[0]["path"], "Assets/Preload.asset")

    def test_texture_material_dependencies_and_content_hash(self):
        self.asset("Color.png", "png placeholder, front end does not decode", TEXTURE_GUID, "TextureImporter:\n  textureType: 0\n  textureShape: 1\n  sRGBTexture: 1\n")
        material = doc(21, 2100000, "Material", f"  m_Shader: {{fileID: 46, guid: 0000000000000000f000000000000000, type: 0}}\n  m_SavedProperties:\n    m_TexEnvs:\n    - _MainTex:\n        m_Texture: {{fileID: 2800000, guid: {TEXTURE_GUID}, type: 3}}\n        m_Scale: {{x: 2, y: 3}}\n        m_Offset: {{x: 0, y: 0}}\n    m_Floats: []\n    m_Colors:\n    - _Color: {{r: 1, g: 0.5, b: 0, a: 1}}")
        self.asset("Color.mat", material, MATERIAL_GUID)
        text = simple_graph() + doc(23, 3, "MeshRenderer", f"  m_GameObject: {{fileID: 1}}\n  m_Materials:\n  - {{fileID: 2100000, guid: {MATERIAL_GUID}, type: 2}}")
        report = self.validate(text)
        self.assertTrue(report["supported"], report["diagnostics"])
        asset = next(a for a in report["assets"] if a["kind"] == "material")
        self.assertEqual(asset["textureGuid"], TEXTURE_GUID)
        self.assertEqual(asset["shaderName"], "Standard")
        self.assertEqual(asset["color"]["g"], 0.5)
        (self.project / "Assets/Color.png").write_text("changed png")
        self.assertNotEqual(self.validate(text)["contentHash"], report["contentHash"])
        self.assertEqual(len(report["sourceDependencies"]), 6)

    def test_unknown_guids_and_custom_shaders_fail(self):
        report = self.validate(simple_graph() + doc(33, 3, "MeshFilter", "  m_GameObject: {fileID: 1}\n  m_Mesh: {fileID: 4300000, guid: 99999999999999999999999999999999, type: 3}"))
        self.assertIn("missing-guid", [d["code"] for d in report["diagnostics"]])

    def test_mono_script_type_scalar_json_and_reference(self):
        self.asset("Probe.cs", "namespace Example; public class Probe : UnityEngine.MonoBehaviour { public int m_speed; public UnityEngine.GameObject target; }", SCRIPT_GUID)
        report = self.validate(simple_graph() + doc(114, 3, "MonoBehaviour", f"  m_GameObject: {{fileID: 1}}\n  m_Script: {{fileID: 11500000, guid: {SCRIPT_GUID}, type: 3}}\n  m_speed: 7\n  target: {{fileID: 1}}"))
        self.assertTrue(report["supported"], report["diagnostics"])
        mono = next(o for o in report["objects"] if o["classId"] == 114)
        self.assertEqual(mono["typeName"], "Example.Probe")
        self.assertEqual(json.loads(mono["scalarDataJson"]), {"m_speed": 7})
        self.assertIn({"property": "target", "fileId": "1", "guid": "", "type": 0}, mono["references"])

    def test_scriptable_object_references_scalar_data_and_callbacks(self):
        data_script_guid = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
        data_guid = "ffffffffffffffffffffffffffffffff"
        self.asset("Data.cs", "namespace Example; public class Data : UnityEngine.ScriptableObject { public int value; public UnityEngine.Material color; }", data_script_guid)
        self.asset("Data.asset", doc(114, 11400000, "MonoBehaviour", f"  m_Script: {{fileID: 11500000, guid: {data_script_guid}, type: 3}}\n  value: 42"), data_guid)
        self.asset("Probe.cs", "namespace Example; public class Probe : UnityEngine.MonoBehaviour { public Data settings; }", SCRIPT_GUID)
        text = simple_graph() + doc(114, 3, "MonoBehaviour", f"  m_GameObject: {{fileID: 1}}\n  m_Script: {{fileID: 11500000, guid: {SCRIPT_GUID}, type: 3}}\n  settings: {{fileID: 11400000, guid: {data_guid}, type: 2}}")
        report = self.validate(text)
        self.assertTrue(report["supported"], report["diagnostics"])
        asset = next(a for a in report["assets"] if a["kind"] == "scriptableObject")
        self.assertEqual(asset["typeName"], "Example.Data")
        self.assertEqual(json.loads(asset["dataJson"])["value"], 42)
        self.assertEqual(asset["fileId"], "11400000")
        self.asset("Data.cs", "namespace Example; public class Data : UnityEngine.ScriptableObject { void OnEnable() {} }", data_script_guid)
        self.assertIn("scriptable-object-callback", [d["code"] for d in self.validate(text)["diagnostics"]])

    def test_texture_import_settings_and_unsupported_specialization(self):
        self.asset("Image.png", "placeholder", TEXTURE_GUID, "TextureImporter:\n  textureType: 0\n  textureShape: 1\n  mipmaps:\n    sRGBTexture: 0\n    enableMipMap: 0\n  textureSettings:\n    filterMode: 0\n    wrapU: 1\n    wrapV: 2\n    aniso: 4\n")
        self.asset("Probe.cs", "public class Probe : UnityEngine.MonoBehaviour { public UnityEngine.Texture2D image; }", SCRIPT_GUID)
        text = simple_graph() + doc(114, 3, "MonoBehaviour", f"  m_GameObject: {{fileID: 1}}\n  m_Script: {{fileID: 11500000, guid: {SCRIPT_GUID}, type: 3}}\n  image: {{fileID: 2800000, guid: {TEXTURE_GUID}, type: 3}}")
        report = self.validate(text)
        self.assertTrue(report["supported"], report["diagnostics"])
        importer = json.loads(next(a for a in report["assets"] if a["kind"] == "texture")["importerJson"])
        self.assertEqual(importer["sRGBTexture"], 0)
        self.assertEqual(importer["mipmapEnabled"], 0)
        self.assertEqual(importer["wrapV"], 2)
        self.asset("Image.png", "placeholder", TEXTURE_GUID, "TextureImporter:\n  textureType: 8\n")
        self.assertIn("texture-importer", [d["code"] for d in self.validate(text)["diagnostics"]])

    def test_material_nondefault_properties_and_keywords_are_explicit(self):
        material = doc(21, 2100000, "Material", "  m_Shader: {fileID: 46, guid: 0000000000000000f000000000000000, type: 0}\n  m_ValidKeywords: [_EMISSION]\n  m_SavedProperties:\n    m_TexEnvs: []\n    m_Floats:\n    - _Metallic: 0.8\n    m_Colors: []")
        self.asset("Color.mat", material, MATERIAL_GUID)
        report = self.validate(simple_graph() + doc(23, 3, "MeshRenderer", f"  m_GameObject: {{fileID: 1}}\n  m_Materials:\n  - {{fileID: 2100000, guid: {MATERIAL_GUID}, type: 2}}"))
        self.assertFalse(report["supported"])
        codes = [d["code"] for d in report["diagnostics"]]
        self.assertIn("material-keywords", codes)
        self.assertIn("material-float-property", codes)

    def test_invalid_external_fileid_and_unknown_builtin_are_rejected(self):
        self.asset("Probe.cs", "public class Probe : UnityEngine.MonoBehaviour {}", SCRIPT_GUID)
        report = self.validate(simple_graph() + doc(114, 3, "MonoBehaviour", f"  m_GameObject: {{fileID: 1}}\n  m_Script: {{fileID: 11500001, guid: {SCRIPT_GUID}, type: 3}}\n  image: {{fileID: 999, guid: 0000000000000000e000000000000000, type: 0}}"))
        codes = [d["code"] for d in report["diagnostics"]]
        self.assertIn("asset-fileid", codes)
        self.assertIn("builtin-reference", codes)

    def test_successful_manifest_is_atomic_and_self_contained(self):
        self.validate(simple_graph())
        output = self.project / "prepared/scene.json"
        manifest = raw_scene.prepare_scene(self.project, "Example", output)
        self.assertEqual(manifest["manifest"], str(output))
        stored = json.loads(output.read_text())
        self.assertTrue(stored["supported"])
        self.assertEqual(stored["objects"][1]["references"][0]["fileId"], "1")
        self.assertFalse(output.with_name(output.name + ".tmp").exists())

    def test_scene_roots_order_and_prefab_child_order(self):
        report = self.validate(simple_graph("A", 1) + simple_graph("B", 11) + doc(1660057539, 30, "SceneRoots", "  m_Roots:\n  - {fileID: 12}\n  - {fileID: 2}"))
        self.assertTrue(report["supported"], report["diagnostics"])
        self.assertEqual([o["fileId"] for o in report["objects"] if o["classId"] == 1], ["11", "1"])
        self.assertEqual([o["fileId"] for o in report["objects"] if o["classId"] == 4], ["12", "2"])
        graph = simple_graph().replace("m_Children: []", "m_Children:\n  - {fileID: 22}\n  - {fileID: 12}") + simple_graph("One", 11, 2) + simple_graph("Two", 21, 2)
        self.asset("Cube.prefab", graph, PREFAB_GUID)
        report = self.validate(prefab(100))
        self.assertTrue(report["supported"], report["diagnostics"])
        root = next(o for o in report["objects"] if o["fileId"] == "100:2")
        self.assertEqual([child["fileID"] for child in json.loads(root["dataJson"])["m_Children"]], ["100:22", "100:12"])

    def test_root_order_override_and_incomplete_scene_roots_rejected(self):
        self.asset("Cube.prefab", simple_graph(), PREFAB_GUID)
        override = f"    - target: {{fileID: 2, guid: {PREFAB_GUID}}}\n      propertyPath: m_RootOrder\n      value: 1\n      objectReference: {{fileID: 0}}\n"
        report = self.validate(prefab(100, overrides=override) + simple_graph("First", 11).replace("m_Children: []", "m_Children: []\n  m_RootOrder: 0"))
        self.assertTrue(report["supported"], report["diagnostics"])
        self.assertEqual([o["fileId"] for o in report["objects"] if o["classId"] == 1], ["11", "100:1"])
        report = self.validate(simple_graph() + doc(1660057539, 30, "SceneRoots", "  m_Roots: []"))
        self.assertIn("scene-roots", [d["code"] for d in report["diagnostics"]])

    def test_camera_aliases_and_unsupported_native_features(self):
        camera = doc(20, 3, "Camera", "  m_GameObject: {fileID: 1}\n  m_projectionMatrixMode: 1\n  orthographic size: 7\n  near clip plane: 0.1\n  far clip plane: 80\n  field of view: 45")
        report = self.validate(simple_graph() + camera)
        self.assertTrue(report["supported"], report["diagnostics"])
        data = json.loads(next(o for o in report["objects"] if o["classId"] == 20)["dataJson"])
        self.assertEqual(data["ProjectionMatrixMode"], 1)
        self.assertEqual(data["orthographicSize"], 7)
        report = self.validate(simple_graph() + camera.replace("m_projectionMatrixMode: 1", "m_projectionMatrixMode: 2"))
        self.assertIn("physical-camera", [d["code"] for d in report["diagnostics"]])
        collider = doc(65, 3, "BoxCollider", "  m_GameObject: {fileID: 1}\n  m_IncludeLayers: {m_Bits: 4}\n  m_ProvidesContacts: 1")
        report = self.validate(simple_graph() + collider)
        self.assertGreaterEqual(len([d for d in report["diagnostics"] if d["code"] == "native-property"]), 2)
        light = doc(108, 3, "Light", "  m_GameObject: {fileID: 1}\n  m_UseColorTemperature: 1\n  m_Lightmapping: 2")
        report = self.validate(simple_graph() + light)
        self.assertFalse(report["supported"])

    def test_texture_npot_resize_and_alpha_processing_rejected(self):
        import struct
        self.asset("Image.png", "placeholder", TEXTURE_GUID, "TextureImporter:\n  nPOTScale: 1\n  maxTextureSize: 2\n  alphaIsTransparency: 1\n")
        (self.project / "Assets/Image.png").write_bytes(b"\x89PNG\r\n\x1a\n" + struct.pack(">I", 13) + b"IHDR" + struct.pack(">II", 3, 4))
        self.asset("Probe.cs", "public class Probe : UnityEngine.MonoBehaviour {}", SCRIPT_GUID)
        report = self.validate(simple_graph() + doc(114, 3, "MonoBehaviour", f"  m_GameObject: {{fileID: 1}}\n  m_Script: {{fileID: 11500000, guid: {SCRIPT_GUID}, type: 3}}\n  image: {{fileID: 2800000, guid: {TEXTURE_GUID}, type: 3}}"))
        codes = [d["code"] for d in report["diagnostics"]]
        self.assertIn("texture-importer-property", codes)
        self.assertIn("texture-resize", codes)
        self.assertIn("texture-npot-resize", codes)

    def texture_settings_report(self, settings):
        self.asset("Image.png", "image placeholder", TEXTURE_GUID, "TextureImporter:\n" + settings)
        self.asset("Probe.cs", "public class Probe : UnityEngine.MonoBehaviour {}", SCRIPT_GUID)
        return self.validate(self.referenced_scene(TEXTURE_GUID, 2800000, 3))

    def test_explicit_uncompressed_texture_has_no_format_warning(self):
        for settings in (
            "  textureCompression: 0\n  compressionQuality: 50\n",
            "  platformSettings:\n  - buildTarget: DefaultTexturePlatform\n    overridden: 0\n    textureCompression: 0\n    textureFormat: -1\n    crunchedCompression: 0\n",
            "  textureCompression: 1\n  platformSettings:\n  - buildTarget: DefaultTexturePlatform\n    textureCompression: 1\n  - buildTarget: Standalone\n    overridden: 1\n    textureCompression: 0\n"):
            with self.subTest(settings=settings):
                report = self.texture_settings_report(settings)
                self.assertTrue(report["supported"], report["diagnostics"])
                self.assertNotIn("texture-runtime-format", [d["code"] for d in report["diagnostics"]])

    def test_effective_texture_compression_and_crunch_are_errors(self):
        for settings, expected in (
            ("  textureCompression: 1\n", "texture-compression"),
            ("  textureCompression: 0\n  platformSettings:\n  - buildTarget: DefaultTexturePlatform\n    overridden: 0\n    textureCompression: 2\n", "texture-compression"),
            ("  textureCompression: 0\n  platformSettings:\n  - buildTarget: Standalone\n    overridden: 1\n    textureCompression: 1\n", "texture-compression"),
            ("  textureCompression: 0\n  crunchedCompression: 1\n", "texture-crunch"),
            ("  textureCompression: 0\n  platformSettings:\n  - buildTarget: Standalone\n    overridden: 1\n    crunchedCompression: 1\n", "texture-crunch"),
            ("  textureCompression: 0\n  platformSettings:\n  - buildTarget: Standalone\n    overridden: 1\n    textureFormat: 10\n", "texture-format")):
            with self.subTest(settings=settings):
                report = self.texture_settings_report(settings)
                self.assertFalse(report["supported"])
                diagnostics = [d for d in report["diagnostics"] if d["code"] == expected]
                self.assertEqual(len(diagnostics), 1, report["diagnostics"])
                self.assertEqual(diagnostics[0]["severity"], "error")

    def test_inactive_and_nonstandalone_texture_compression_is_ignored(self):
        settings = "  textureCompression: 0\n  platformSettings:\n  - buildTarget: DefaultTexturePlatform\n    textureCompression: 0\n  - buildTarget: Standalone\n    overridden: 0\n    textureCompression: 1\n    textureFormat: 10\n    crunchedCompression: 1\n  - buildTarget: Android\n    overridden: 1\n    textureCompression: 3\n    textureFormat: 48\n    crunchedCompression: 1\n"
        report = self.texture_settings_report(settings)
        self.assertTrue(report["supported"], report["diagnostics"])
        self.assertNotIn("texture-runtime-format", [d["code"] for d in report["diagnostics"]])

    def test_missing_texture_compression_retains_explicit_uncertainty(self):
        report = self.texture_settings_report("  compressionQuality: 50\n")
        self.assertTrue(report["supported"], report["diagnostics"])
        warnings = [d for d in report["diagnostics"] if d["code"] == "texture-runtime-format"]
        self.assertEqual(len(warnings), 1)
        self.assertEqual(warnings[0]["severity"], "warning")
        self.assertIn("unspecified", warnings[0]["message"])

    def test_effective_texture_platform_keeps_size_guards(self):
        import struct
        settings = "  textureCompression: 0\n  maxTextureSize: 8\n  platformSettings:\n  - buildTarget: DefaultTexturePlatform\n    maxTextureSize: 2\n  - buildTarget: Standalone\n    overridden: 0\n    maxTextureSize: 8\n"
        self.texture_settings_report(settings)
        (self.project / "Assets/Image.png").write_bytes(b"\x89PNG\r\n\x1a\n" + struct.pack(">I", 13) + b"IHDR" + struct.pack(">II", 4, 4))
        scene = self.referenced_scene(TEXTURE_GUID, 2800000, 3)
        self.assertIn("texture-resize", [d["code"] for d in self.validate(scene)["diagnostics"]])
        meta = self.project / "Assets/Image.png.meta"
        meta.write_text(meta.read_text().replace("overridden: 0", "overridden: 1"))
        report = self.validate(scene)
        self.assertTrue(report["supported"], report["diagnostics"])

    def test_asset_names_preserve_authored_names_before_snapshots(self):
        self.asset("Data.cs", "public class Data : UnityEngine.ScriptableObject {}", "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee")
        self.asset("DifferentFilename.asset", doc(114, 11400000, "MonoBehaviour", "  m_Name: Authored data name\n  m_Script: {fileID: 11500000, guid: eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee, type: 3}"), "ffffffffffffffffffffffffffffffff")
        self.asset("Probe.cs", "public class Probe : UnityEngine.MonoBehaviour {}", SCRIPT_GUID)
        report = self.validate(simple_graph() + doc(114, 3, "MonoBehaviour", f"  m_GameObject: {{fileID: 1}}\n  m_Script: {{fileID: 11500000, guid: {SCRIPT_GUID}, type: 3}}\n  settings: {{fileID: 11400000, guid: ffffffffffffffffffffffffffffffff, type: 2}}"))
        self.assertTrue(report["supported"], report["diagnostics"])
        data = next(a for a in report["assets"] if a["kind"] == "scriptableObject")
        self.assertEqual(data["name"], "Authored data name")

    def reference_assets(self):
        """Supported assets whose GUID/fileID pairs resolve independently of type."""
        self.asset("Probe.cs", "public class Probe : UnityEngine.MonoBehaviour {}", SCRIPT_GUID)
        data_script = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
        data_guid = "ffffffffffffffffffffffffffffffff"
        self.asset("Data.cs", "public class Data : UnityEngine.ScriptableObject {}", data_script)
        self.asset("Settings.asset", doc(114, 11400000, "MonoBehaviour", f"  m_Script: {{fileID: 11500000, guid: {data_script}, type: 3}}\n  count: 7"), data_guid)
        self.asset("Color.mat", doc(21, 2100000, "Material", "  m_Shader: {fileID: 46, guid: 0000000000000000f000000000000000, type: 0}"), MATERIAL_GUID)
        cases = [(".cs", SCRIPT_GUID, 11500000, 3), (".asset", data_guid, 11400000, 2),
                 (".mat", MATERIAL_GUID, 2100000, 2)]
        for index, extension in enumerate((".png", ".jpg", ".jpeg"), start=1):
            guid = f"{index:032x}"
            self.asset("Image" + extension, "image placeholder", guid, "TextureImporter:\n  textureType: 0\n  textureShape: 1\n")
            cases.append((extension, guid, 2800000, 3))
        return cases

    def referenced_scene(self, guid, file_id, reference_type):
        kind = "" if reference_type is None else f", type: {reference_type}"
        return simple_graph() + doc(114, 3, "MonoBehaviour", f"  m_GameObject: {{fileID: 1}}\n  m_Script: {{fileID: 11500000, guid: {SCRIPT_GUID}, type: 3}}\n  target: {{fileID: {file_id}, guid: {guid}{kind}}}")

    def test_external_reference_types_reject_resolvable_wrong_kinds(self):
        for extension, guid, file_id, expected in self.reference_assets():
            with self.subTest(extension=extension):
                report = self.validate(self.referenced_scene(guid, file_id, 3 if expected == 2 else 2))
                self.assertFalse(report["supported"])
                errors = [d for d in report["diagnostics"] if d["code"] == "external-reference-type"]
                self.assertEqual(len(errors), 1, report["diagnostics"])
                self.assertEqual(errors[0]["path"], "Assets/Example.unity")
                self.assertEqual(errors[0]["fileId"], "3")
                self.assertIn("target", errors[0]["message"])
                self.assertIn(f"expected type {expected}", errors[0]["message"])

    def test_external_reference_types_accept_correct_or_omitted_kinds(self):
        for extension, guid, file_id, expected in self.reference_assets():
            for kind in (expected, None):
                with self.subTest(extension=extension, kind=kind):
                    report = self.validate(self.referenced_scene(guid, file_id, kind))
                    self.assertTrue(report["supported"], report["diagnostics"])

    def test_external_reference_types_skip_null_local_and_builtin_references(self):
        self.reference_assets()
        text = self.referenced_scene(MATERIAL_GUID, 0, 3).replace("m_GameObject: {fileID: 1}", "m_GameObject: {fileID: 1, type: 99}")
        text += doc(33, 4, "MeshFilter", "  m_GameObject: {fileID: 1}\n  m_Mesh: {fileID: 10202, guid: 0000000000000000e000000000000000, type: 99}")
        report = self.validate(text)
        self.assertTrue(report["supported"], report["diagnostics"])

    def test_external_reference_types_validate_material_import_dependencies(self):
        self.reference_assets()
        self.asset("Color.mat", doc(21, 2100000, "Material", "  m_Shader: {fileID: 46, guid: 0000000000000000f000000000000000, type: 0}\n  m_SavedProperties:\n    m_TexEnvs:\n    - _MainTex:\n        m_Texture: {fileID: 2800000, guid: 00000000000000000000000000000001, type: 2}"), MATERIAL_GUID)
        report = self.validate(self.referenced_scene(MATERIAL_GUID, 2100000, 2))
        errors = [d for d in report["diagnostics"] if d["code"] == "external-reference-type"]
        self.assertEqual(len(errors), 1, report["diagnostics"])
        self.assertEqual(errors[0]["path"], "Assets/Color.mat")
        self.assertIn("m_SavedProperties.m_TexEnvs[0]._MainTex.m_Texture", errors[0]["message"])

    def test_external_reference_types_validate_prefab_overrides_without_rejecting_prefab_targets(self):
        self.reference_assets()
        self.asset("Cube.prefab", self.referenced_scene("ffffffffffffffffffffffffffffffff", 11400000, 2), PREFAB_GUID)
        overrides = f"    - target: {{fileID: 3, guid: {PREFAB_GUID}, type: 3}}\n      propertyPath: target\n      value:\n      objectReference: {{fileID: 11400000, guid: ffffffffffffffffffffffffffffffff, type: 3}}\n"
        report = self.validate(prefab(100, overrides=overrides))
        errors = [d for d in report["diagnostics"] if d["code"] == "external-reference-type"]
        self.assertEqual(len(errors), 1, report["diagnostics"])
        self.assertIn("m_Modification.m_Modifications[0].objectReference", errors[0]["message"])
        self.assertTrue(self.validate(prefab(100, overrides=overrides.replace("ffffffffffffffffffffffffffffffff, type: 3", "ffffffffffffffffffffffffffffffff, type: 2")))["supported"])

    def test_external_reference_type_error_preserves_published_manifest(self):
        self.reference_assets()
        self.validate(self.referenced_scene("ffffffffffffffffffffffffffffffff", 11400000, 3))
        output = self.project / "manifest.json"
        output.write_text("previous valid revision")
        with self.assertRaises(raw_scene.RawSceneError) as caught:
            raw_scene.prepare_scene(self.project, "Example", output)
        self.assertIn("external-reference-type", [d["code"] for d in caught.exception.report["diagnostics"]])
        self.assertEqual(output.read_text(), "previous valid revision")

    def test_yaml_fast_parser_matches_safe_python_fallback(self):
        import yaml
        from unittest.mock import patch
        text = simple_graph("yes", 9223372036854775800)
        fast = self.validate(text)
        with patch.object(yaml, "CSafeLoader", yaml.SafeLoader, create=True):
            fallback = self.validate(text)
        self.assertEqual(fast, fallback)

    def test_guid_lookup_ignores_library_and_accepts_embedded_package(self):
        cache = self.project / "Library/PackageCache/example"
        cache.mkdir(parents=True)
        (cache / "Probe.cs").write_text("namespace Example; public class Probe : UnityEngine.MonoBehaviour {}")
        (cache / "Probe.cs.meta").write_text(f"guid: {SCRIPT_GUID}\n")
        text = simple_graph() + doc(114, 3, "MonoBehaviour", f"  m_GameObject: {{fileID: 1}}\n  m_Script: {{fileID: 11500000, guid: {SCRIPT_GUID}, type: 3}}")
        self.assertIn("missing-guid", [d["code"] for d in self.validate(text)["diagnostics"]])
        package = self.project / "Packages/com.example"
        package.mkdir(parents=True)
        (package / "Probe.cs").write_text((cache / "Probe.cs").read_text())
        (package / "Probe.cs.meta").write_text(f"guid: {SCRIPT_GUID}\n")
        report = self.validate(text)
        self.assertTrue(report["supported"], report["diagnostics"])
        self.assertEqual(next(o for o in report["objects"] if o["classId"] == 114)["typeName"], "Example.Probe")
        self.assertTrue(all(not item["path"].startswith("Library/") for item in report["sourceDependencies"]))

    def test_guid_metadata_mutation_after_indexing_cannot_publish(self):
        from unittest.mock import patch
        self.asset("Probe.cs", "public class Probe : UnityEngine.MonoBehaviour {}", SCRIPT_GUID)
        text = simple_graph() + doc(114, 3, "MonoBehaviour", f"  m_GameObject: {{fileID: 1}}\n  m_Script: {{fileID: 11500000, guid: {SCRIPT_GUID}, type: 3}}")
        self.scene.write_text(text)
        original_expand = raw_scene._Reader.expand
        def mutate_after_index(reader, path, *args, **kwargs):
            if path == self.scene:
                (self.project / "Assets/Probe.cs.meta").write_text("guid: dddddddddddddddddddddddddddddddd\n")
            return original_expand(reader, path, *args, **kwargs)
        output = self.project / "manifest.json"
        output.write_text("previous revision")
        with patch.object(raw_scene._Reader, "expand", mutate_after_index):
            with self.assertRaises(raw_scene.RawSceneError) as caught:
                raw_scene.prepare_scene(self.project, "Example", output)
        self.assertIn("GUID indexing", caught.exception.report["diagnostics"][0]["message"])
        self.assertEqual(output.read_text(), "previous revision")

    def test_parsed_yaml_uses_same_captured_bytes_as_dependency_hash(self):
        import hashlib
        self.scene.write_text(simple_graph("Original"))
        reader = raw_scene._Reader(self.project)
        reader.depend(self.scene)
        observed_hash = reader.dependencies["Assets/Example.unity"]["sha256"]
        self.scene.write_text(simple_graph("Changed"))
        text = reader.text(self.scene)
        self.assertIn("m_Name: Original", text)
        self.assertEqual(hashlib.sha256(text.encode()).hexdigest(), observed_hash)
        with self.assertRaises(raw_scene.RawSceneError):
            reader.depend(self.scene)

    def test_source_and_metadata_symlinks_into_project_library_rejected(self):
        library = self.project / "Library"
        library.mkdir()
        source = library / "Probe.cs"
        source.write_text("public class Probe : UnityEngine.MonoBehaviour {}")
        (self.project / "Assets/Probe.cs").symlink_to(source)
        (self.project / "Assets/Probe.cs.meta").write_text(f"guid: {SCRIPT_GUID}\n")
        text = simple_graph() + doc(114, 3, "MonoBehaviour", f"  m_GameObject: {{fileID: 1}}\n  m_Script: {{fileID: 11500000, guid: {SCRIPT_GUID}, type: 3}}")
        report = self.validate(text)
        self.assertFalse(report["supported"])
        self.assertIn("project-library-input", [d["code"] for d in report["diagnostics"]])
        (self.project / "Assets/Probe.cs").unlink()
        (self.project / "Assets/Probe.cs").write_text(source.read_text())
        metadata = library / "Probe.cs.meta"
        metadata.write_text(f"guid: {SCRIPT_GUID}\n")
        (self.project / "Assets/Probe.cs.meta").unlink()
        (self.project / "Assets/Probe.cs.meta").symlink_to(metadata)
        self.assertIn("project-library-input", [d["code"] for d in self.validate(text)["diagnostics"]])

    def test_external_package_root_aliasing_project_library_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            package = Path(directory)
            (package / "Probe.cs").write_text("public class Probe : UnityEngine.MonoBehaviour {}")
            (package / "Probe.cs.meta").write_text(f"guid: {SCRIPT_GUID}\n")
            (self.project / "Library").symlink_to(package, target_is_directory=True)
            catalog = {"schemaVersion": 1, "sourceRoots": [{"path": "Packages/com.example", "source": str(package)}]}
            report = self.validate(simple_graph(), catalog)
            self.assertFalse(report["supported"])
            self.assertIn("project-library-input", [item["code"] for item in report["diagnostics"]])

    def test_library_sibling_package_and_stale_metadata_remain_supported(self):
        package = self.project / "LibrarySibling"
        package.mkdir()
        (package / "Probe.cs").write_text("public class Probe : UnityEngine.MonoBehaviour {}")
        (package / "Probe.cs.meta").write_text(f"guid: {SCRIPT_GUID}\n")
        (self.project / "Assets/Stale.cs.meta").write_text("guid: " + "d" * 32 + "\n")
        catalog = {"schemaVersion": 1, "sourceRoots": [{"path": "Packages/com.example", "source": str(package)}]}
        text = simple_graph() + doc(114, 3, "MonoBehaviour", f"  m_GameObject: {{fileID: 1}}\n  m_Script: {{fileID: 11500000, guid: {SCRIPT_GUID}, type: 3}}")
        report = self.validate(text, catalog)
        self.assertTrue(report["supported"], report["diagnostics"])

    def test_duplicate_keys_and_aliases_are_not_accepted(self):
        for extra in ("  m_LocalPosition: duplicate", "  dangerous: &alias [1]\n  copy: *alias"):
            report = self.validate(simple_graph() + "\n" + extra)
            self.assertIn("parse-error", [d["code"] for d in report["diagnostics"]])

    def test_transform_parent_cycle_rejected(self):
        report = self.validate(simple_graph(father=2))
        self.assertIn("transform-cycle", [d["code"] for d in report["diagnostics"]])

    def test_unit_transform_quaternions_and_absent_rotation_remain_supported(self):
        for rotation in ("{x: 0.5, y: -0.5, z: 0.5, w: 0.5}",
                         "{x: 0, y: 0, z: 0, w: -1}",
                         "{x: 0, y: 0, z: 0, w: 1.000004}"):
            with self.subTest(rotation=rotation):
                report = self.validate(simple_graph().replace("{x: 0, y: 0, z: 0, w: 1}", rotation))
                self.assertTrue(report["supported"], report["diagnostics"])
        report = self.validate(simple_graph().replace("  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}\n", ""))
        self.assertTrue(report["supported"], report["diagnostics"])

    def test_nonunit_and_zero_transform_quaternions_are_explicitly_unsupported(self):
        for rotation in ("{x: 0.25, y: -0.3, z: 0.08, w: 0.917}",
                         "{x: 0, y: 0, z: 0, w: 0}",
                         "{x: 0, y: 0, z: 0, w: 1.000006}"):
            with self.subTest(rotation=rotation):
                report = self.validate(simple_graph().replace("{x: 0, y: 0, z: 0, w: 1}", rotation))
                self.assertFalse(report["supported"])
                errors = [d for d in report["diagnostics"] if d["code"] == "transform-quaternion"]
                self.assertEqual(len(errors), 1, report["diagnostics"])
                self.assertEqual(errors[0]["fileId"], "2")
                self.assertIn("normalizes", errors[0]["message"])
                self.assertEqual(json.loads(report["objects"][1]["dataJson"])["m_LocalRotation"], raw_scene._yaml(rotation))

    def test_nonunit_transform_quaternion_does_not_replace_manifest(self):
        self.validate(simple_graph().replace("{x: 0, y: 0, z: 0, w: 1}", "{x: 0.25, y: -0.3, z: 0.08, w: 0.917}"))
        output = self.project / "manifest.json"
        output.write_text("previous valid revision")
        with self.assertRaises(raw_scene.RawSceneError) as caught:
            raw_scene.prepare_scene(self.project, "Example", output)
        self.assertIn("transform-quaternion", [d["code"] for d in caught.exception.report["diagnostics"]])
        self.assertEqual(output.read_text(), "previous valid revision")

    def test_ambiguous_scene_and_exact_path(self):
        self.validate(simple_graph())
        nested = self.project / "Assets/Nested"
        nested.mkdir()
        (nested / "Example.unity").write_text(simple_graph())
        with self.assertRaises(raw_scene.RawSceneError):
            raw_scene.validate_scene(self.project, "Example")
        self.assertTrue(raw_scene.validate_scene(self.project, "Assets/Example.unity")["supported"])


if __name__ == "__main__":
    unittest.main()
