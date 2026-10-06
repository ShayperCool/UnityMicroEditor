"""Content iteration reuses verified inputs and rejects stale or raced sources."""
import hashlib
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import raw
import raw_cache
import raw_compile
import raw_scene
import imported_assets
import host
import unity_play
from test_raw_scene import simple_graph, prefab, PREFAB_GUID


class FileCacheTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.path = self.root / "value.txt"
        self.path.write_bytes(b"original")
        self.cache = raw_cache.RawPreparationSession()

    def tearDown(self):
        self.temp.cleanup()

    def test_unchanged_bytes_are_read_and_hashed_once(self):
        first = self.cache.read(self.path)
        with patch.object(Path, "open", side_effect=AssertionError("unexpected read")):
            self.assertEqual(self.cache.read(self.path), first)
        self.assertEqual(self.cache.stats["fileReads"], 1)
        self.assertEqual(self.cache.stats["fileHits"], 1)

    def test_restored_size_and_mtime_do_not_hide_a_write(self):
        first = self.cache.read(self.path)
        stamp = self.path.stat()
        self.path.write_bytes(b"modified")
        os.utime(self.path, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
        self.assertNotEqual(self.cache.read(self.path), first)
        self.assertEqual(self.cache.stats["fileReads"], 2)

    def test_atomic_replacement_does_not_reuse_bytes(self):
        first = self.cache.read(self.path)
        stamp = self.path.stat()
        other = self.root / "replacement"
        other.write_bytes(b"modified")
        os.utime(other, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
        other.replace(self.path)
        self.assertNotEqual(self.cache.read(self.path), first)

    def test_broken_process_cache_record_falls_back_to_source(self):
        self.cache.read(self.path)
        self.cache._files[str(self.path)] = {"bad": "record"}
        self.assertEqual(self.cache.read(self.path)[0], b"original")
        self.assertEqual(self.cache.stats["fileReads"], 2)

    def test_disappearing_or_changed_observed_source_is_rejected(self):
        self.cache.read(self.path)
        self.path.write_bytes(b"modified")
        with self.assertRaisesRegex(raw_cache.RawCacheError, "changed during preparation"):
            self.cache.assert_observed_stable()
        self.path.unlink()
        with self.assertRaisesRegex(raw_cache.RawCacheError, "disappeared"):
            self.cache.assert_observed_stable()

    def test_directory_inventory_detects_create_delete_and_symlink_target(self):
        first = self.cache.inventory([self.root])
        extra = self.root / "added.cs"
        extra.write_text("new source")
        second = self.cache.inventory([self.root])
        self.assertNotEqual(first, second)
        extra.unlink()
        third = self.cache.inventory([self.root])
        self.assertEqual(first, third)
        link = self.root / "link"
        link.symlink_to(self.path)
        fourth = self.cache.inventory([self.root])
        self.path.write_bytes(b"changed!")
        self.assertNotEqual(fourth, self.cache.inventory([self.root]))

    def test_excluded_descendant_cannot_be_reintroduced_as_a_separate_inventory_root(self):
        excluded = self.root / "cache"
        child = excluded / "nested"
        child.mkdir(parents=True)
        (child / "ignored.dll").write_bytes(b"generated output")
        inventory = self.cache.inventory([self.root, child], exclude=[excluded])
        self.assertFalse(any(name.startswith(str(excluded)) for name in inventory))
        direct = self.cache.inventory([self.root], files=[child / "ignored.dll"], exclude=[excluded])
        self.assertIn(str(child / "ignored.dll"), direct)

    def test_guid_index_key_reuses_atomic_source_content_edit_but_detects_root_replacement(self):
        source = self.root / "Cube.prefab"
        source.write_text(simple_graph())
        meta = Path(str(source) + ".meta")
        meta.write_text(f"guid: {PREFAB_GUID}\n")
        initial = self.cache.index_key([self.root], self.cache.inventory([self.root]))
        replacement = self.root / "replacement"
        replacement.write_text(simple_graph("Changed"))
        replacement.replace(source)
        self.assertEqual(initial, self.cache.index_key([self.root], self.cache.inventory([self.root])))
        old = self.root.parent / (self.root.name + "-old")
        self.root.rename(old)
        self.root.mkdir()
        os.link(old / source.name, source)
        os.link(old / meta.name, meta)
        self.assertNotEqual(initial, self.cache.index_key([self.root], self.cache.inventory([self.root])))
        import shutil
        shutil.rmtree(old)

    def test_old_text_cannot_poison_yaml_cache_with_a_new_source_digest(self):
        self.path.write_text(simple_graph("Original"))
        old_text = self.cache.read(self.path)[0].decode("utf-8-sig")
        self.path.write_text(simple_graph("New bytes"))
        with self.assertRaisesRegex(raw_cache.RawCacheError, "between reading and parsing"):
            self.cache.documents(self.path, raw_scene.parse_unity_yaml, old_text)
        current_text = self.path.read_text()
        documents = self.cache.documents(self.path, raw_scene.parse_unity_yaml, current_text)
        self.assertEqual(documents[0]["data"]["m_Name"], "New bytes")
        self.assertEqual(self.cache.stats["yamlParses"], 1)


class PreparationSessionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.project = self.root / "project"
        (self.project / "Assets").mkdir(parents=True)
        (self.project / "Packages").mkdir()
        (self.project / "ProjectSettings").mkdir()
        (self.project / "ProjectSettings/ProjectVersion.txt").write_text("m_EditorVersion: 6000.6.3f1\n")
        self.scene = self.project / "Assets/Example.unity"
        self.scene.write_text(simple_graph())
        self.cs = self.project / "Assets/Example.cs"
        self.cs.write_text("public class Example {}")
        self.host = self.root / "host/Host.x86_64"
        managed = self.host.parent / "Host_Data/Managed"
        managed.mkdir(parents=True)
        self.host.write_bytes(b"native player")
        (managed / "Dungeon.CLIPlay.dll").write_bytes(b"bridge")
        (self.host.parent / "host.json").write_text(json.dumps(
            {"engineVersion": "6000.6.3f1", "inputProjectAssetsBundled": False}))
        self.unity = self.root / "sdk/Unity"
        self.unity.parent.mkdir()
        self.unity.write_bytes(b"sdk")
        self.output = self.root / "output"
        self.session = raw_cache.RawPreparationSession()
        self.compile_count = 0
        self.compiler_patch = patch.object(raw_compile, "compile_project", side_effect=self.compile)
        self.compiler_mock = self.compiler_patch.start()
        self.addCleanup(self.compiler_patch.stop)

    def tearDown(self):
        self.temp.cleanup()

    def compile(self, project, unity, output, host, **kwargs):
        self.compile_count += 1
        output.mkdir(parents=True)
        assembly = output / "Example.dll"
        assembly.write_bytes(hashlib.sha256(self.cs.read_bytes()).digest())
        return {"assemblies": [{"path": str(assembly)}], "scripts": [],
                "compilerInputs": {str(self.cs): hashlib.sha256(self.cs.read_bytes()).hexdigest()},
                "compilerInputHash": "verified-inputs", "compiledAssemblyCount": 1}

    def prepare(self, previous=None, **kwargs):
        return raw.prepare_raw(self.project, self.unity, self.host, "Assets/Example.unity", self.output,
                               session_cache=self.session, previous_build=previous, **kwargs)

    def test_scene_move_and_new_gameobject_skip_compiler_and_keep_exact_assembly_paths(self):
        first = self.prepare()
        self.scene.write_text(simple_graph().replace("{x: 0, y: 0, z: 0}", "{x: 4, y: 0, z: 0}")
                              + simple_graph("Added object", first=10))
        second = self.prepare(first)
        self.assertEqual(self.compile_count, 1)
        self.assertTrue(second["reloadCompatible"])
        self.assertEqual(second["preparationMode"], "content-only")
        first_manifest = json.loads(Path(first["manifest"]).read_text())
        manifest = json.loads(Path(second["manifest"]).read_text())
        self.assertEqual(manifest["assemblies"], first_manifest["assemblies"])
        self.assertEqual(len(manifest["objects"]), 4)
        self.assertEqual(json.loads(manifest["objects"][1]["dataJson"])["m_LocalPosition"]["x"], 4)

    def test_existing_prefab_and_new_prefab_guid_are_resolved_without_compilation(self):
        source = self.project / "Assets/Cube.prefab"
        source.write_text(simple_graph())
        Path(str(source) + ".meta").write_text(f"guid: {PREFAB_GUID}\n")
        self.scene.write_text(prefab(100))
        first = self.prepare()
        source.write_text(simple_graph("Changed prefab"))
        second = self.prepare(first)
        self.assertTrue(second["reloadCompatible"])
        self.assertGreater(second["cacheStats"]["guidIndexHits"], 0)
        added = self.project / "Assets/New.prefab"
        added.write_text(simple_graph("New prefab"))
        guid = "f" * 32
        Path(str(added) + ".meta").write_text(f"guid: {guid}\n")
        self.scene.write_text(prefab(200, guid))
        third = self.prepare(second)
        self.assertTrue(third["reloadCompatible"])
        self.assertEqual(self.compile_count, 1)
        manifest = json.loads(Path(third["manifest"]).read_text())
        self.assertEqual(json.loads(manifest["objects"][0]["dataJson"])["m_Name"], "New")

    def test_changed_meta_rebuilds_guid_index_and_dangling_reference_keeps_last_revision(self):
        source = self.project / "Assets/Cube.prefab"
        source.write_text(simple_graph())
        meta = Path(str(source) + ".meta")
        meta.write_text(f"guid: {PREFAB_GUID}\n")
        self.scene.write_text(prefab(100))
        first = self.prepare()
        pointer = (self.output / "current.json").read_bytes()
        meta.write_text("guid: " + "f" * 32 + "\n")
        with self.assertRaisesRegex(unity_play.ToolError, "No raw asset .meta resolves"):
            self.prepare(first)
        self.assertEqual((self.output / "current.json").read_bytes(), pointer)

    def test_changed_code_restored_mtime_new_code_and_deleted_code_require_full_prepare(self):
        first = self.prepare()
        before = self.cs.stat()
        self.cs.write_text("public class Changed {}")
        os.utime(self.cs, ns=(before.st_atime_ns, before.st_mtime_ns))
        second = self.prepare(first)
        self.assertFalse(second["reloadCompatible"])
        added = self.project / "Assets/New.cs"
        added.write_text("public class New {}")
        third = self.prepare(second)
        self.assertFalse(third["reloadCompatible"])
        added.unlink()
        fourth = self.prepare(third)
        self.assertFalse(fourth["reloadCompatible"])
        self.assertEqual(self.compile_count, 4)

    def test_config_package_sdk_or_original_dll_mutation_requires_full_prepare(self):
        previous = self.prepare()
        targets = [self.project / "Packages/manifest.json", self.project / "ProjectSettings/TagManager.asset",
                   self.unity, self.host.parent / "Host_Data/Managed/Additional.dll"]
        for index, target in enumerate(targets):
            target.write_text(str(index))
            previous = self.prepare(previous)
            self.assertFalse(previous["reloadCompatible"])
        original_dll = Path(previous["compiler"]["assemblies"][0]["path"])
        original_dll.write_bytes(b"unexpected mutation")
        self.assertFalse(self.prepare(previous)["reloadCompatible"])

    def test_changed_paths_hint_cannot_hide_an_unreported_code_edit(self):
        first = self.prepare()
        self.scene.write_text(simple_graph("Moved"))
        self.cs.write_text("public class HiddenChange {}")
        self.assertFalse(self.prepare(first, changed_paths=[self.scene])["reloadCompatible"])
        self.assertEqual(self.compile_count, 2)

    def test_new_session_cannot_trust_an_existing_build(self):
        first = self.prepare()
        self.session = raw_cache.RawPreparationSession()
        second = self.prepare(first)
        self.assertFalse(second["reloadCompatible"])
        self.assertEqual(self.compile_count, 2)

    def test_parsed_yaml_reuse_does_not_mutate_prefab_template(self):
        first = self.prepare()
        second = self.prepare(first)
        self.assertGreater(second["cacheStats"]["yamlHits"], 0)
        self.assertEqual(second["cacheStats"]["yamlParses"], 0)
        self.assertEqual(Path(first["manifest"]).read_text().split('"objects"')[1].split('"assets"')[0],
                         Path(second["manifest"]).read_text().split('"objects"')[1].split('"assets"')[0])

    def test_mid_prepare_source_write_keeps_previous_pointer(self):
        first = self.prepare()
        pointer = (self.output / "current.json").read_bytes()
        original = raw_scene.prepare_scene
        def changed_after_parse(*args, **kwargs):
            manifest = original(*args, **kwargs)
            self.scene.write_text(simple_graph("Written while preparing"))
            return manifest
        with patch.object(raw_scene, "prepare_scene", side_effect=changed_after_parse):
            with self.assertRaisesRegex(unity_play.ToolError, "changed during preparation"):
                self.prepare(first)
        self.assertEqual((self.output / "current.json").read_bytes(), pointer)
        self.assertEqual(self.session._trusted["build"]["revision"], first["revision"])

    def test_parse_race_then_stable_retry_uses_new_scene_graph(self):
        first = self.prepare()
        pointer = (self.output / "current.json").read_bytes()
        original = self.session.documents
        def changed_before_cache_parse(path, parser, text):
            if path == self.scene:
                self.scene.write_text(simple_graph("New graph after raced read"))
            return original(path, parser, text)
        with patch.object(self.session, "documents", side_effect=changed_before_cache_parse):
            with self.assertRaisesRegex(unity_play.ToolError, "between reading and parsing"):
                self.prepare(first)
        self.assertEqual((self.output / "current.json").read_bytes(), pointer)
        replacement = self.prepare(first)
        manifest = json.loads(Path(replacement["manifest"]).read_text())
        self.assertEqual(json.loads(manifest["objects"][0]["dataJson"])["m_Name"], "New graph after raced read")

    def test_new_script_created_during_full_compile_is_rejected(self):
        original = self.compile
        def create_during_compile(*args, **kwargs):
            compiled = original(*args, **kwargs)
            (self.project / "Assets/Missed.cs").write_text("public class Missed {}")
            return compiled
        self.compiler_mock.side_effect = create_during_compile
        with self.assertRaisesRegex(unity_play.ToolError, "appeared during preparation"):
            self.prepare()

    def test_metadata_and_project_version_race_before_inventory_are_rejected(self):
        for target in (self.host.parent / "host.json", self.project / "ProjectSettings/ProjectVersion.txt"):
            self.session = raw_cache.RawPreparationSession()
            original = self.session.read
            changed = False
            def replace_after_read(path, **kwargs):
                nonlocal changed
                value = original(path, **kwargs)
                if Path(path) == target and not changed:
                    changed = True
                    target.write_bytes(target.read_bytes() + b"\n")
                return value
            with patch.object(self.session, "read", side_effect=replace_after_read):
                with self.assertRaisesRegex(unity_play.ToolError, "changed during preparation"):
                    self.prepare()

    def native_fixture(self):
        pack = self.root / "native.pack"
        pack.write_bytes(b"immutable native content")
        audio = self.project / "Assets/Heavy.wav"
        audio.write_bytes(b"native audio source")
        catalog_path = self.root / "catalog.json"
        catalog = {"schemaVersion": 1, "engineVersion": "6000.6.3f1", "project": str(self.project),
                   "catalogPath": str(catalog_path), "cacheHit": True, "packs": [{"bundlePath": str(pack)}],
                   "sourceDependencies": [{"path": "Assets/Heavy.wav", "physicalPath": str(audio),
                                            "sha256": hashlib.sha256(audio.read_bytes()).hexdigest()}]}
        catalog_path.write_text(json.dumps(catalog))
        def stage_player(host_path, revision, compiler):
            player = revision / "Player/Host.x86_64"
            player.parent.mkdir()
            player.write_bytes(b"staged native player")
            managed = player.parent / "Host_Data/Managed"
            managed.mkdir(parents=True)
            source = Path(compiler["assemblies"][0]["path"])
            staged = managed / source.name
            staged.write_bytes(source.read_bytes())
            return {"player": str(player), "assemblies": [str(staged)]}
        return catalog_path, catalog, audio, pack, stage_player

    def test_native_content_reload_reuses_original_staged_native_script_dll_paths(self):
        catalog_path, catalog, audio, pack, stage_player = self.native_fixture()
        with patch.object(imported_assets, "inspect_catalog", return_value=catalog) as inspect, \
             patch.object(host, "stage_player", side_effect=stage_player) as stage:
            first = self.prepare(native_catalog=catalog_path)
            self.scene.write_text(simple_graph("Moved native scene"))
            second = self.prepare(first, native_catalog=catalog_path)
            self.assertEqual(inspect.call_count, 1)
            self.assertEqual(stage.call_count, 1)
            self.assertEqual(self.compile_count, 1)
            self.assertTrue(second["reloadCompatible"])
            self.assertEqual(first["player"], second["player"])
            self.assertEqual(first["playerStage"], second["playerStage"])
            manifests = [json.loads(Path(value["manifest"]).read_text()) for value in (first, second)]
            self.assertEqual(manifests[0]["assemblies"], manifests[1]["assemblies"])
            self.assertIn("/Player/Host_Data/Managed/", manifests[1]["assemblies"][0])

    def test_native_external_package_metadata_is_indexed_once_before_first_content_edit(self):
        catalog_path, catalog, audio, pack, stage_player = self.native_fixture()
        package = self.root / "external-package"
        package.mkdir()
        prefab_source = package / "Foreign.prefab"
        prefab_source.write_text(simple_graph())
        Path(str(prefab_source) + ".meta").write_text("guid: " + "e" * 32 + "\n")
        catalog["sourceRoots"] = [{"path": "Packages/com.example.foreign", "source": str(package)}]
        with patch.object(imported_assets, "inspect_catalog", return_value=catalog), \
             patch.object(host, "stage_player", side_effect=stage_player):
            first = self.prepare(native_catalog=catalog_path)
            self.scene.write_text(simple_graph("First content edit"))
            second = self.prepare(first, native_catalog=catalog_path)
            self.assertTrue(second["reloadCompatible"])
            self.assertEqual(second["cacheStats"]["guidIndexHits"], 1)

    def test_shared_verified_native_model_graph_remains_immutable_across_reloads(self):
        catalog_path, catalog, audio, pack, stage_player = self.native_fixture()
        guid = "f" * 32
        model = self.project / "Assets/Model.fbx"
        model.write_bytes(b"model")
        Path(str(model) + ".meta").write_text(f"guid: {guid}\n")
        objects = raw_scene.parse_unity_yaml(model, simple_graph("Model"))
        # The imported graph rewrites this asset-local Mesh reference into an
        # external GUID. That rewrite must happen only in a per-reader copy.
        objects.append({"classId": 33, "fileId": "3", "typeName": "MeshFilter", "stripped": False,
                        "data": {"m_GameObject": {"fileID": "1"}, "m_Mesh": {"fileID": "4300000"}}})
        objects[0]["data"]["m_Component"].append({"component": {"fileID": "3"}})
        catalog["assets"] = [{"guid": guid, "fileId": str(index), "kind": "imported", "classId": class_id,
                              "typeName": "UnityEngine." + type_name, "path": "Assets/Model.fbx"}
                             for index, class_id, type_name in ((1, 1, "GameObject"), (2, 4, "Transform"),
                                                               (3, 33, "MeshFilter"), (4300000, 43, "Mesh"))]
        catalog["modelPrefabs"] = [{"guid": guid, "path": "Assets/Model.fbx", "rootFileId": "1", "objects": objects}]
        original_catalog = json.dumps(catalog, sort_keys=True)
        self.scene.write_text(prefab(100, guid))
        with patch.object(imported_assets, "inspect_catalog", return_value=catalog), \
             patch.object(host, "stage_player", side_effect=stage_player):
            first = self.prepare(native_catalog=catalog_path)
            self.assertEqual(json.dumps(catalog, sort_keys=True), original_catalog)
            second = self.prepare(first, native_catalog=catalog_path)
            self.assertTrue(second["reloadCompatible"])
            self.assertEqual(json.dumps(catalog, sort_keys=True), original_catalog)

    def test_changed_heavy_source_and_changed_native_pack_never_use_content_reload(self):
        catalog_path, catalog, audio, pack, stage_player = self.native_fixture()
        with patch.object(imported_assets, "inspect_catalog", return_value=catalog) as inspect, \
             patch.object(host, "stage_player", side_effect=stage_player):
            previous = self.prepare(native_catalog=catalog_path)
            audio.write_bytes(b"changed native source")
            previous = self.prepare(previous, native_catalog=catalog_path)
            self.assertFalse(previous["reloadCompatible"])
            pack.write_bytes(b"changed native pack")
            previous = self.prepare(previous, native_catalog=catalog_path)
            self.assertFalse(previous["reloadCompatible"])
            self.assertEqual(inspect.call_count, 3)

    def test_automatic_importer_runs_again_for_heavy_edit(self):
        catalog_path, catalog, audio, pack, stage_player = self.native_fixture()
        with patch.object(imported_assets, "export_imported_assets", return_value=catalog) as exporter, \
             patch.object(host, "stage_player", side_effect=stage_player):
            first = self.prepare(native_import=True)
            self.scene.write_text(simple_graph("Changed raw scene"))
            second = self.prepare(first, native_import=True)
            self.assertTrue(second["reloadCompatible"])
            self.assertEqual(exporter.call_count, 1)
            audio.write_bytes(b"faithful importer must handle this")
            third = self.prepare(second, native_import=True)
            self.assertFalse(third["reloadCompatible"])
            self.assertEqual(exporter.call_count, 2)

    def test_unknown_imported_reference_is_rejected_before_reload_publication(self):
        catalog_path, catalog, audio, pack, stage_player = self.native_fixture()
        unknown = self.project / "Assets/Unknown.fbx"
        unknown.write_bytes(b"not imported")
        guid = "f" * 32
        Path(str(unknown) + ".meta").write_text(f"guid: {guid}\n")
        with patch.object(imported_assets, "inspect_catalog", return_value=catalog), \
             patch.object(host, "stage_player", side_effect=stage_player):
            first = self.prepare(native_catalog=catalog_path)
            pointer = (self.output / "current.json").read_bytes()
            self.scene.write_text(simple_graph().replace("m_Component:\n", "m_Component:\n  - component: {fileID: 3}\n")
                                  + f"--- !u!33 &3\nMeshFilter:\n  m_GameObject: {{fileID: 1}}\n"
                                    f"  m_Mesh: {{fileID: 4300000, guid: {guid}, type: 3}}\n")
            with self.assertRaisesRegex(unity_play.ToolError, "unsupported or invalid content|unsupported or invalid|unsupported import|native|importer"):
                self.prepare(first, native_catalog=catalog_path)
            self.assertEqual((self.output / "current.json").read_bytes(), pointer)

    def test_new_native_audio_reference_expands_automatic_closure_and_requires_restart(self):
        catalog_path, catalog, audio, pack, stage_player = self.native_fixture()
        guid = "f" * 32
        Path(str(audio) + ".meta").write_text(f"guid: {guid}\n")
        expanded = {**catalog, "assets": [{"guid": guid, "fileId": "8300000", "kind": "imported",
                                          "classId": 83, "typeName": "UnityEngine.AudioClip", "path": "Assets/Heavy.wav"}]}
        with patch.object(imported_assets, "export_imported_assets", side_effect=[catalog, expanded]) as exporter, \
             patch.object(host, "stage_player", side_effect=stage_player):
            first = self.prepare(native_import=True)
            self.scene.write_text(simple_graph().replace("m_Component:\n", "m_Component:\n  - component: {fileID: 3}\n")
                                  + "--- !u!82 &3\nAudioSource:\n  m_GameObject: {fileID: 1}\n"
                                  + f"  m_audioClip: {{fileID: 8300000, guid: {guid}, type: 3}}\n")
            second = self.prepare(first, native_import=True)
            self.assertEqual(exporter.call_count, 2)
            self.assertFalse(second["reloadCompatible"])
            self.assertEqual(second["reloadFallbackReason"], "native-closure-expanded")
            self.assertEqual(second["nativeClosureRequirements"], [{"path": "Assets/Heavy.wav", "guid": guid, "kind": "asset"}])
            manifest = json.loads(Path(second["manifest"]).read_text())
            self.assertTrue(any(asset["guid"] == guid and asset["fileId"] == "8300000" for asset in manifest["assets"]))
            self.assertEqual(json.loads((self.output / "current.json").read_text())["revision"], second["revision"])

    def test_new_native_model_prefab_reference_expands_automatic_closure(self):
        catalog_path, catalog, audio, pack, stage_player = self.native_fixture()
        guid = "f" * 32
        model = self.project / "Assets/New.fbx"
        model.write_bytes(b"binary FBX")
        Path(str(model) + ".meta").write_text(f"guid: {guid}\n")
        objects = raw_scene.parse_unity_yaml(model, simple_graph("Imported model"))
        expanded = {**catalog, "assets": [{"guid": guid, "fileId": str(index), "kind": "imported",
                                          "classId": class_id, "typeName": "UnityEngine." + type_name,
                                          "path": "Assets/New.fbx"}
                                         for index, class_id, type_name in ((1, 1, "GameObject"), (2, 4, "Transform"))],
                    "modelPrefabs": [{"guid": guid, "path": "Assets/New.fbx", "rootFileId": "1", "objects": objects}]}
        with patch.object(imported_assets, "export_imported_assets", side_effect=[catalog, expanded]) as exporter, \
             patch.object(host, "stage_player", side_effect=stage_player):
            first = self.prepare(native_import=True)
            self.scene.write_text(prefab(100, guid))
            second = self.prepare(first, native_import=True)
            self.assertEqual(exporter.call_count, 2)
            self.assertFalse(second["reloadCompatible"])
            self.assertEqual(second["nativeClosureRequirements"][0]["kind"], "model")
            manifest = json.loads(Path(second["manifest"]).read_text())
            self.assertEqual(len(manifest["objects"]), 2)

    def test_malformed_scene_with_missing_heavy_reference_never_starts_importer_again(self):
        catalog_path, catalog, audio, pack, stage_player = self.native_fixture()
        guid = "f" * 32
        Path(str(audio) + ".meta").write_text(f"guid: {guid}\n")
        with patch.object(imported_assets, "export_imported_assets", return_value=catalog) as exporter, \
             patch.object(host, "stage_player", side_effect=stage_player):
            first = self.prepare(native_import=True)
            pointer = (self.output / "current.json").read_bytes()
            self.scene.write_text(simple_graph().replace("m_Component:\n", "m_Component:\n  - component: {fileID: 404}\n")
                                  + "--- !u!82 &3\nAudioSource:\n  m_GameObject: {fileID: 1}\n"
                                  + f"  m_audioClip: {{fileID: 8300000, guid: {guid}, type: 3}}\n")
            with self.assertRaises(unity_play.ToolError):
                self.prepare(first, native_import=True)
            self.assertEqual(exporter.call_count, 1)
            self.assertEqual((self.output / "current.json").read_bytes(), pointer)

    def test_failed_native_closure_import_preserves_original_tool_error(self):
        catalog_path, catalog, audio, pack, stage_player = self.native_fixture()
        guid = "f" * 32
        Path(str(audio) + ".meta").write_text(f"guid: {guid}\n")
        with patch.object(imported_assets, "export_imported_assets", side_effect=[catalog, unity_play.ToolError("native import failed")]), \
             patch.object(host, "stage_player", side_effect=stage_player):
            first = self.prepare(native_import=True)
            self.scene.write_text(simple_graph().replace("m_Component:\n", "m_Component:\n  - component: {fileID: 3}\n")
                                  + "--- !u!82 &3\nAudioSource:\n  m_GameObject: {fileID: 1}\n"
                                  + f"  m_audioClip: {{fileID: 8300000, guid: {guid}, type: 3}}\n")
            with self.assertRaisesRegex(unity_play.ToolError, "native import failed"):
                self.prepare(first, native_import=True)


if __name__ == "__main__":
    unittest.main()
