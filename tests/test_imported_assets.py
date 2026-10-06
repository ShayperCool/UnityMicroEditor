import copy
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import imported_assets
from unity_play import ToolError, write_json


class ImportedAssetTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.project = self.root / "source"
        for folder in ("Assets", "Packages", "ProjectSettings"):
            (self.project / folder).mkdir(parents=True)
        (self.project / "ProjectSettings/ProjectVersion.txt").write_text("m_EditorVersion: 6000.6.3f1\n")
        (self.project / "Packages/manifest.json").write_text('{"dependencies": {}}\n')
        self.scene = self.project / "Assets/Scene.unity"
        self.scene.write_text("m_Name: Scene\nclip: {fileID: 8300000, guid: " + "a" * 32 + ", type: 3}\n")
        self.audio = self.project / "Assets/Test.wav"
        self.audio.write_bytes(b"native waveform bytes")
        self.audio.with_name(self.audio.name + ".meta").write_text("guid: " + "a" * 32 + "\nAudioImporter:\n  forceToMono: 0\n")
        self.code = self.project / "Assets/Logic.cs"
        self.code.write_text("class Logic { void Update() {} }\n")
        self.code.with_name(self.code.name + ".meta").write_text("guid: " + "c" * 32 + "\n")
        self.editor = self.root / "Unity"
        self.editor.write_text("test-editor")
        self.cache = self.root / "cache"
        self.modify_export = None
        self.after_export = None
        self.exit_code = 0

    def tearDown(self):
        self.temporary.cleanup()

    def editor_export(self, command, project, log, timeout, env):
        self.assertEqual(Path(command[command.index("-projectPath") + 1]), project)
        self.assertNotEqual(project, self.project)
        request = json.loads(Path(env["UNITY_PLAY_NATIVE_IMPORT_REQUEST"]).read_text())
        output = Path(request["outputDirectory"])
        output.mkdir(parents=True, exist_ok=True)
        pack = output / "unity-play-native-assets"
        pack.write_bytes(b"native pack: " + (project / "Assets/Test.wav").read_bytes())
        sources = [project / "Assets/Test.wav", project / "Assets/Test.wav.meta"]
        report = {"schemaVersion": 1, "success": True, "engineVersion": "6000.6.3f1", "target": "StandaloneLinux64",
                  "exportId": request["exportId"], "requestSha256": imported_assets.sha256(Path(env["UNITY_PLAY_NATIVE_IMPORT_REQUEST"])),
                  "scriptingBackend": "Mono2x", "graphicsApi": "glcore", "containsScenes": False, "sceneLoader": "raw",
                  "assets": [{"guid": "a" * 32, "fileId": "8300000", "classId": 83,
                              "kind": "imported", "typeName": "UnityEngine.AudioClip", "name": "Test", "path": "Assets/Test.wav"}],
                  "packs": [{"bundlePath": str(pack), "sha256": imported_assets.sha256(pack), "bytes": pack.stat().st_size,
                             "crc": 123, "registryAsset": "assets/__unityplayimports/registry.asset", "dependencies": []}],
                  "sourceDependencies": [{"path": source.relative_to(project).as_posix(), "physicalPath": str(source),
                                          "sha256": imported_assets.sha256(source), "bytes": source.stat().st_size} for source in sources],
                  "sourceRoots": [], "modelPrefabs": [], "elapsedSeconds": 0.02,
                  "requiredAssemblies": ["UnityEngine.AudioModule"], "rootAssets": ["Assets/Test.wav"]}
        if self.modify_export:
            self.modify_export(report)
        write_json(Path(request["catalogPath"]), report)
        if self.after_export:
            self.after_export()
        return {"exitCode": self.exit_code, "elapsedSeconds": 0.03}

    def export(self):
        with patch.object(imported_assets, "run_process", side_effect=self.editor_export), patch.object(imported_assets, "require_idle"):
            return imported_assets.export_imported_assets(self.project, self.editor, ["Assets/Scene.unity"], cache_dir=self.cache)

    def test_import_project_keeps_source_bytes_and_uses_tool_owned_registry(self):
        before = imported_assets.source_inventory(self.project)
        result = self.export()
        self.assertEqual(imported_assets.source_inventory(self.project), before)
        self.assertFalse((self.project / "Library").exists())
        self.assertFalse((self.project / "Assets/__UnityPlayImports").exists())
        self.assertEqual(result["sceneLoader"], "raw")
        self.assertFalse(result["containsScenes"])
        self.assertFalse(result["cacheHit"])
        self.assertTrue((self.cache / "ImportProject/Assets/__UnityPlayImports/Editor/NativeAssetExporter.cs").is_file())

    def test_scene_transform_and_method_body_changes_reuse_native_cache_without_editor(self):
        first = self.export()
        self.scene.write_text(self.scene.read_text() + "m_LocalPosition: {x: 3, y: 2, z: 1}\n")
        self.code.write_text("class Logic { void Update() { /* changed behavior */ } }\n")
        with patch.object(imported_assets, "run_process") as editor:
            second = imported_assets.export_imported_assets(self.project, self.editor, ["Assets/Scene.unity"], cache_dir=self.cache)
            editor.assert_not_called()
        self.assertTrue(second["cacheHit"])
        self.assertEqual(second["revision"], first["revision"])

    def test_prefab_transform_edits_reuse_cache_but_component_identity_changes_refresh_metadata(self):
        prefab = self.project / "Assets/Player.prefab"
        prefab.write_text("""%YAML 1.1
--- !u!1 &100
GameObject:
  m_Name: Player
  m_Component:
  - component: {fileID: 101}
--- !u!4 &101
Transform:
  m_GameObject: {fileID: 100}
  m_Father: {fileID: 0}
  m_Children: []
  m_LocalPosition: {x: 0, y: 0, z: 0}
""")
        prefab.with_name(prefab.name + ".meta").write_text("guid: " + "d" * 32 + "\n")
        self.scene.write_text(self.scene.read_text() + "player: {fileID: 100, guid: " + "d" * 32 + ", type: 3}\n")
        first = self.export()
        prefab.write_text(prefab.read_text().replace("m_LocalPosition: {x: 0", "m_LocalPosition: {x: 10"))
        with patch.object(imported_assets, "run_process") as editor:
            second = imported_assets.export_imported_assets(self.project, self.editor, ["Assets/Scene.unity"], cache_dir=self.cache)
            editor.assert_not_called()
        self.assertEqual(second["revision"], first["revision"])
        prefab.write_text(prefab.read_text().replace("  - component: {fileID: 101}", "  - component: {fileID: 101}\n  - component: {fileID: 102}") +
                          "--- !u!33 &102\nMeshFilter:\n  m_GameObject: {fileID: 100}\n  m_Mesh: {fileID: 0}\n")
        third = self.export()
        self.assertFalse(third["cacheHit"])
        self.assertNotEqual(third["revision"], first["revision"])

    def test_lightmap_index_change_invalidates_native_scene_metadata(self):
        first = self.export()
        self.scene.write_text(self.scene.read_text() + "  m_LightmapIndex: 0\n")
        second = self.export()
        self.assertFalse(second["cacheHit"])
        self.assertNotEqual(first["revision"], second["revision"])

    def test_stale_array_override_probe_keeps_exact_owner_and_requested_size(self):
        prefab = self.project / "Assets/Variant.prefab"
        prefab.write_text("""%YAML 1.1
--- !u!1001 &100100000
PrefabInstance:
  m_Modification:
    m_Modifications:
    - target: {fileID: 1234, guid: cccccccccccccccccccccccccccccccc, type: 3}
      propertyPath: layers.Array.size
      value: 2
      objectReference: {fileID: 0}
    - target: {fileID: 1234, guid: cccccccccccccccccccccccccccccccc, type: 3}
      propertyPath: layers.Array.data[0].name
      value: valid
      objectReference: {fileID: 0}
    - target: {fileID: 1234, guid: cccccccccccccccccccccccccccccccc, type: 3}
      propertyPath: layers.Array.data[2].name
      value: stale
      objectReference: {fileID: 0}
""")
        probes = imported_assets._override_probes(self.project, imported_assets.source_inventory(self.project))
        self.assertEqual(probes, [{"path": "Assets/Variant.prefab", "instanceFileId": "100100000", "targetGuid": "c" * 32,
                                  "targetFileId": "1234", "propertyPath": "layers.Array.data[2].name", "arrayPath": "layers", "expectedArraySize": 2}])

    def test_audio_and_importer_settings_changes_create_distinct_immutable_revision(self):
        first = self.export()
        old_catalog = Path(first["catalogPath"]).read_bytes()
        old_pack = Path(first["packs"][0]["bundlePath"]).read_bytes()
        self.audio.write_bytes(b"changed waveform")
        second = self.export()
        self.assertNotEqual(second["revision"], first["revision"])
        self.assertEqual(Path(first["catalogPath"]).read_bytes(), old_catalog)
        self.assertEqual(Path(first["packs"][0]["bundlePath"]).read_bytes(), old_pack)
        meta = self.audio.with_name(self.audio.name + ".meta")
        meta.write_text(meta.read_text().replace("forceToMono: 0", "forceToMono: 1"))
        third = self.export()
        self.assertNotEqual(third["revision"], second["revision"])

    def test_new_scene_asset_reference_invalidates_discovery_without_layout_hash(self):
        first = self.export()
        other = self.project / "Assets/Other.wav"
        other.write_bytes(b"second waveform")
        other.with_name(other.name + ".meta").write_text("guid: " + "b" * 32 + "\n")
        self.scene.write_text(self.scene.read_text() + "other: {fileID: 8300000, guid: " + "b" * 32 + ", type: 3}\n")
        result = self.export()
        self.assertFalse(result["cacheHit"])
        self.assertNotEqual(result["revision"], first["revision"])

    def test_source_change_during_export_never_advances_current_pointer(self):
        first = self.export()
        before = (self.cache / "current.json").read_bytes()
        self.audio.write_bytes(b"trigger new import")
        self.after_export = lambda: self.scene.write_text("changed during editor export\n")
        with self.assertRaisesRegex(ToolError, "Authored sources changed during native export"):
            self.export()
        self.assertEqual((self.cache / "current.json").read_bytes(), before)
        self.assertEqual(Path(first["packs"][0]["bundlePath"]).read_bytes(), b"native pack: native waveform bytes")

    def prepare_finished_interrupted_export(self):
        with patch.object(imported_assets, "require_idle"):
            prepared = imported_assets.prepare_import(self.project, self.editor, ["Assets/Scene.unity"], cache_dir=self.cache)
        self.editor_export(prepared["command"], Path(prepared["importProject"]), Path(prepared["launchLog"]), 30,
                           prepared["environment"])
        return prepared

    def test_completed_interrupted_export_recovers_original_persisted_snapshot_without_editor(self):
        prepared = self.prepare_finished_interrupted_export()
        manifest = json.loads(Path(prepared["preparedManifest"]).read_text())
        self.assertEqual(manifest["sourceInventory"], imported_assets.source_inventory(self.project))
        with patch.object(imported_assets, "run_process") as editor, patch.object(imported_assets, "require_idle") as idle:
            recovered = imported_assets.recover_import(self.cache)
        editor.assert_not_called()
        idle.assert_called_once_with(Path(prepared["importProject"]))
        self.assertTrue(recovered["recovered"])
        self.assertEqual(recovered["exportId"], prepared["exportId"])
        self.assertTrue((self.cache / "current.json").is_file())

    def test_recovery_rejects_source_mutation_after_interruption_and_preserves_prior_revision(self):
        self.export()
        pointer = (self.cache / "current.json").read_bytes()
        self.audio.write_bytes(b"new native source")
        self.prepare_finished_interrupted_export()
        self.code.write_text("class Logic { void Update() { /* changed after interrupted export */ } }\n")
        with patch.object(imported_assets, "require_idle"), self.assertRaisesRegex(ToolError, "Authored sources changed"):
            imported_assets.recover_import(self.cache)
        self.assertEqual((self.cache / "current.json").read_bytes(), pointer)

    def test_recovery_rejects_stale_completion_request_nonce_and_hash(self):
        for key in ("exportId", "requestSha256"):
            with self.subTest(key=key):
                prepared = self.prepare_finished_interrupted_export()
                report = json.loads(Path(prepared["exportCatalog"]).read_text())
                report[key] = "0" * (32 if key == "exportId" else 64)
                write_json(Path(prepared["exportCatalog"]), report)
                with patch.object(imported_assets, "require_idle"), self.assertRaisesRegex(ToolError, "does not match the prepared request"):
                    imported_assets.recover_import(self.cache)

    def test_recovery_rejects_request_or_prepared_manifest_mutation(self):
        for key in ("request", "preparedManifest"):
            with self.subTest(key=key):
                prepared = self.prepare_finished_interrupted_export()
                file = Path(prepared[key])
                file.write_text(file.read_text() + "\n")
                with patch.object(imported_assets, "require_idle"), self.assertRaisesRegex(ToolError, "SHA256 mismatch"):
                    imported_assets.recover_import(self.cache)

    def test_recovery_does_not_reprepare_an_incomplete_or_active_attempt(self):
        with patch.object(imported_assets, "require_idle"):
            prepared = imported_assets.prepare_import(self.project, self.editor, ["Assets/Scene.unity"], cache_dir=self.cache)
        before = Path(prepared["preparedManifest"]).read_bytes()
        with patch.object(imported_assets, "require_idle"), self.assertRaisesRegex(ToolError, "catalog is unreadable"):
            imported_assets.recover_import(self.cache)
        with patch.object(imported_assets, "require_idle", side_effect=ToolError("import project is running")):
            with self.assertRaisesRegex(ToolError, "import project is running"):
                imported_assets.recover_import(self.cache)
        self.assertEqual(Path(prepared["preparedManifest"]).read_bytes(), before)
        self.assertFalse((self.cache / "current.json").exists())

    def test_failed_editor_and_corrupt_pack_keep_existing_pointer(self):
        self.export()
        pointer = (self.cache / "current.json").read_bytes()
        self.audio.write_bytes(b"changed for failed export")
        self.exit_code = 1
        with self.assertRaisesRegex(ToolError, "previous content is unchanged"):
            self.export()
        self.assertEqual((self.cache / "current.json").read_bytes(), pointer)
        self.exit_code = 0
        self.modify_export = lambda report: report["packs"][0].update(sha256="0" * 64)
        with self.assertRaisesRegex(ToolError, "SHA256 mismatch"):
            self.export()
        self.assertEqual((self.cache / "current.json").read_bytes(), pointer)

    def test_duplicate_names_keep_exact_long_signed_native_ids(self):
        def modify(report):
            first = report["assets"][0]
            first["fileId"] = "-9223372036854775807"
            report["assets"].append({**first, "fileId": "9223372036854775806"})
        self.modify_export = modify
        result = self.export()
        self.assertEqual([item["name"] for item in result["assets"]], ["Test", "Test"])
        self.assertEqual([item["fileId"] for item in result["assets"]], ["-9223372036854775807", "9223372036854775806"])

    def test_ambiguous_native_identity_and_scene_pack_claim_are_rejected(self):
        self.modify_export = lambda report: report["assets"].append(copy.deepcopy(report["assets"][0]))
        with self.assertRaisesRegex(ToolError, "identity is duplicated"):
            self.export()
        self.modify_export = lambda report: report.update(containsScenes=True)
        with self.assertRaisesRegex(ToolError, "invalid runtime/schema contract"):
            self.export()

    def test_deferred_prefab_binding_preserves_original_owner_and_array_property(self):
        self.modify_export = lambda report: report.update(deferredBindings=[{
            "ownerGuid": "a" * 32, "ownerFileId": "8300000", "propertyPath": "modifiers.Array.data[0].characterPrefab",
            "targetGuid": "b" * 32, "targetFileId": "-9012345678901234567", "targetPath": "Assets/Player.prefab"}])
        result = self.export()
        binding = result["deferredBindings"][0]
        self.assertEqual(binding["ownerGuid"], "a" * 32)
        self.assertEqual(binding["targetFileId"], "-9012345678901234567")
        self.assertEqual(binding["propertyPath"], "modifiers.Array.data[0].characterPrefab")

    def test_deferred_binding_to_unknown_owner_or_duplicate_property_is_rejected(self):
        binding = {"ownerGuid": "f" * 32, "ownerFileId": "8300000", "propertyPath": "characterPrefab",
                   "targetGuid": "b" * 32, "targetFileId": "100100000"}
        self.modify_export = lambda report: report.update(deferredBindings=[binding])
        with self.assertRaisesRegex(ToolError, "binding identity/property is invalid"):
            self.export()
        binding["ownerGuid"] = "a" * 32
        self.modify_export = lambda report: report.update(deferredBindings=[binding, copy.deepcopy(binding)])
        with self.assertRaisesRegex(ToolError, "binding property is duplicated"):
            self.export()

    def global_settings_export(self, report):
        record = {"kind": "imported", "guid": "d" * 32, "fileId": "11400000", "classId": 114,
                  "typeName": "UnityEngine.Rendering.Universal.UniversalRenderPipelineGlobalSettings",
                  "name": "GlobalSettings", "path": "Assets/Settings/GlobalSettings.asset"}
        report["assets"].append(record)
        report["globalSettingsAssets"] = [{"guid": record["guid"], "fileId": record["fileId"],
                                           "path": record["path"], "assetName": record["path"].lower()}]
        report["packs"][0]["addressableAssets"] = [report["packs"][0]["registryAsset"], record["path"].lower()]

    def test_pipeline_global_settings_preserve_original_identity_and_explicit_address(self):
        self.modify_export = self.global_settings_export
        result = self.export()
        self.assertEqual(result["globalSettingsAssets"], [{"guid": "d" * 32, "fileId": "11400000",
                         "path": "Assets/Settings/GlobalSettings.asset", "assetName": "assets/settings/globalsettings.asset"}])
        self.assertIn("assets/settings/globalsettings.asset", result["packs"][0]["addressableAssets"])

    def test_global_settings_implicit_dependency_or_wrong_address_is_rejected(self):
        for mutation, error in ((lambda report: report["packs"][0].pop("addressableAssets"), "not an explicit addressable"),
                                (lambda report: report["globalSettingsAssets"][0].update(assetName="assets/__unityplayimports/clone.asset"), "identity/address is invalid"),
                                (lambda report: report["globalSettingsAssets"].append(copy.deepcopy(report["globalSettingsAssets"][0])), "identity/address is invalid")):
            with self.subTest(error=error):
                def modify(report):
                    self.global_settings_export(report)
                    mutation(report)
                self.modify_export = modify
                with self.assertRaisesRegex(ToolError, error):
                    self.export()

    def excluded_clip_export(self, report):
        excluded = {"guid": "e" * 32, "fileId": "110800000", "path": "Assets/Model.fbx",
                    "typeName": "UnityEngine.AnimationClip", "name": "__preview__Jump", "hideFlags": 17,
                    "hideFlagsName": "HideInHierarchy, DontSaveInBuild", "dontSaveInBuild": True,
                    "reason": "HideFlags.DontSaveInBuild"}
        real = {"guid": "e" * 32, "fileId": "-9223372036854775807", "path": "Assets/Model.fbx",
                "typeName": "UnityEngine.AnimationClip", "name": "Jump", "hideFlags": 1,
                "hideFlagsName": "HideInHierarchy", "dontSaveInBuild": False}
        report["excludedRuntimeAssets"] = [excluded]
        report["modelAnimationClips"] = [{key: value for key, value in excluded.items() if key != "reason"}, real]
        report["assets"].append({**{key: real[key] for key in ("guid", "fileId", "path", "typeName", "name")},
                                "kind": "imported", "classId": 74})

    def test_build_excluded_clip_has_exact_flag_proof_and_real_clip_identity_stays_runtime(self):
        self.modify_export = self.excluded_clip_export
        result = self.export()
        self.assertEqual(result["excludedRuntimeAssets"][0]["fileId"], "110800000")
        self.assertNotIn("110800000", [item["fileId"] for item in result["assets"]])
        self.assertIn("-9223372036854775807", [item["fileId"] for item in result["assets"]])
        self.assertEqual([item["dontSaveInBuild"] for item in result["modelAnimationClips"]], [True, False])

    def test_preview_name_without_native_build_flag_never_authorizes_exclusion(self):
        def modify(report):
            self.excluded_clip_export(report)
            report["excludedRuntimeAssets"][0].update(hideFlags=1, dontSaveInBuild=False)
        self.modify_export = modify
        with self.assertRaisesRegex(ToolError, "lacks exact hide-flags proof"):
            self.export()

    def test_excluded_asset_cannot_also_enter_registry_or_change_reason(self):
        for mutation in (lambda report: report["assets"].append({**report["excludedRuntimeAssets"][0], "kind": "imported"}),
                         lambda report: report["excludedRuntimeAssets"][0].update(reason="preview-name")):
            def modify(report):
                self.excluded_clip_export(report)
                mutation(report)
            self.modify_export = modify
            with self.assertRaisesRegex(ToolError, "lacks exact hide-flags proof"):
                self.export()

    def test_model_clip_observation_cannot_claim_unexported_real_clip(self):
        def modify(report):
            self.excluded_clip_export(report)
            report["assets"].pop()
        self.modify_export = modify
        with self.assertRaisesRegex(ToolError, "no matching runtime/excluded identity"):
            self.export()

    def cooked_omission_export(self, report):
        self.excluded_clip_export(report)
        omitted = report["excludedRuntimeAssets"][0]
        omitted.update(hideFlags=9, hideFlagsName="HideInHierarchy, NotEditable", dontSaveInBuild=False,
                       reason="nativeCookedOmission", cookedProof={
            "method": "AssetBundle.LoadFromFile/LoadAsset<ImportedAssetRegistry>", "engineVersion": "6000.6.3f1",
            "target": "StandaloneLinux64", "bundleSha256": "3" * 64, "bundleCrc": 123,
            "registryAsset": "assets/__unityplayimports/registry.asset", "nativeNullObserved": True,
            "sourceTypeAvailable": True, "retainedTypesVerified": True})
        report["modelAnimationClips"][0].update(hideFlags=9, hideFlagsName="HideInHierarchy, NotEditable", dontSaveInBuild=False)
        report["cookedRegistryVerified"] = True
        report["nativeBuildPasses"] = [{"pass": 1, "registryEntries": len(report["assets"]) + 1, "cookedNullEntries": 1},
                                      {"pass": 2, "registryEntries": len(report["assets"]), "cookedNullEntries": 0}]

    def test_cooked_null_proof_without_build_flag_requires_successful_second_registry_readback(self):
        self.modify_export = self.cooked_omission_export
        result = self.export()
        self.assertEqual(result["excludedRuntimeAssets"][0]["hideFlags"], 9)
        self.assertEqual(result["excludedRuntimeAssets"][0]["reason"], "nativeCookedOmission")
        self.assertFalse(result["modelAnimationClips"][0]["dontSaveInBuild"])
        self.assertNotIn("110800000", [entry["fileId"] for entry in result["assets"]])

    def test_cooked_omission_rejects_incomplete_source_type_or_missing_native_null_proof(self):
        for field in ("sourceTypeAvailable", "nativeNullObserved", "retainedTypesVerified", "bundleSha256"):
            with self.subTest(field=field):
                def modify(report):
                    self.cooked_omission_export(report)
                    report["excludedRuntimeAssets"][0]["cookedProof"].pop(field)
                self.modify_export = modify
                with self.assertRaisesRegex(ToolError, "invalid public registry readback proof"):
                    self.export()

    def test_cooked_omission_rejects_unverified_or_still_missing_second_pack(self):
        for mutation in (lambda report: report.update(cookedRegistryVerified=False),
                         lambda report: report["nativeBuildPasses"].pop(),
                         lambda report: report["nativeBuildPasses"][1].update(cookedNullEntries=1)):
            def modify(report):
                self.cooked_omission_export(report)
                mutation(report)
            self.modify_export = modify
            with self.assertRaisesRegex(ToolError, "verified rebuilt registry without null"):
                self.export()

    def test_library_seed_requires_idle_source_and_never_links_source_cache(self):
        library = self.project / "Library"
        library.mkdir()
        (library / "ArtifactDB").write_bytes(b"source artifacts")
        execute = subprocess.run
        seed_commands = []
        def copy(command, **kwargs):
            if "--reflink=always" in command:
                seed_commands.append(command)
                shutil.copytree(command[-2], command[-1])
                return subprocess.CompletedProcess(command, 0, stdout="", stderr="")
            return execute(command, **kwargs)
        with patch.object(imported_assets, "require_idle") as idle, patch.object(imported_assets.subprocess, "run", side_effect=copy):
            prepared = imported_assets.prepare_import(self.project, self.editor, ["Assets/Scene.unity"], cache_dir=self.cache, seed_library=True)
        idle.assert_any_call(self.project)
        self.assertTrue(prepared["librarySeeded"])
        self.assertEqual(len(seed_commands), 1)
        self.assertEqual(seed_commands[0][-2], str(library))
        copied = Path(prepared["importProject"]) / "Library/ArtifactDB"
        copied.write_bytes(b"owned importer changes")
        self.assertEqual((library / "ArtifactDB").read_bytes(), b"source artifacts")

    def test_reserved_directory_and_source_symlinks_are_rejected(self):
        (self.project / "Assets/Link.wav").symlink_to(self.audio)
        with self.assertRaisesRegex(ToolError, "source symlink"):
            self.export()
        (self.project / "Assets/Link.wav").unlink()
        (self.project / "Assets/__UnityPlayImports").mkdir()
        (self.project / "Assets/__UnityPlayImports/user.asset").write_text("authored data")
        with self.assertRaisesRegex(ToolError, "reserved native importer"):
            self.export()

    def test_unowned_import_project_and_scene_prefab_requests_are_rejected(self):
        isolated = self.cache / "ImportProject"
        isolated.mkdir(parents=True)
        (isolated / "authored.txt").write_text("human owned content")
        with self.assertRaisesRegex(ToolError, "not owned by the native importer"):
            self.export()
        for path in ("Assets/Scene.unity", "Assets/Prefab.prefab", "../Assets/Test.wav"):
            with self.subTest(path=path), self.assertRaises(ToolError):
                imported_assets.prepare_import(self.project, self.editor, [], cache_dir=self.root / "other", asset_paths=[path])

    def test_repeated_prepare_preserves_library_and_prunes_only_source_mirror(self):
        with patch.object(imported_assets, "require_idle"):
            first = imported_assets.prepare_import(self.project, self.editor, ["Assets/Scene.unity"], cache_dir=self.cache)
            library = Path(first["importProject"]) / "Library/ArtifactDB"
            library.parent.mkdir()
            library.write_bytes(b"persistent artifact cache")
            old = self.project / "Assets/Delete.me"
            old.write_text("temporary source")
            imported_assets.prepare_import(self.project, self.editor, ["Assets/Scene.unity"], cache_dir=self.cache)
            old.unlink()
            result = imported_assets.prepare_import(self.project, self.editor, ["Assets/Scene.unity"], cache_dir=self.cache)
        self.assertEqual(library.read_bytes(), b"persistent artifact cache")
        self.assertFalse((Path(result["importProject"]) / "Assets/Delete.me").exists())
        self.assertTrue((Path(result["importProject"]) / "Assets/__UnityPlayImports/Editor/NativeAssetExporter.cs").exists())

    def cached_package_fixture(self):
        manifest = {"dependencies": {"example.registry": "1.2.3", "example.git": "https://example.com/pkg.git#v2",
                                     "com.unity.modules.audio": "1.0.0"}}
        locked = {"example.registry": {"version": "1.2.3", "source": "registry"},
                  "example.git": {"version": "https://example.com/pkg.git#v2", "source": "git", "hash": "1" * 40},
                  "com.unity.modules.audio": {"version": "1.0.0", "source": "builtin"}}
        write_json(self.project / "Packages/manifest.json", manifest)
        write_json(self.project / "Packages/packages-lock.json", {"dependencies": locked})
        resolution = {}
        for name, version in (("example.registry", "1.2.3"), ("example.git", "2.0.0")):
            folder = self.project / "Library/PackageCache" / (name + "@cache-fingerprint")
            folder.mkdir(parents=True)
            write_json(folder / "package.json", {"name": name, "version": version})
            (folder / "Runtime.cs").write_text("public class CachedPackage {}\n")
            resolution[name + "@" + locked[name]["version"]] = {
                "name": name, "version": version, "source": locked[name]["source"], "resolvedPath": str(folder),
                "fingerprint": "2" * 40} # Unity's package content fingerprint is not the Git commit hash.
        write_json(self.project / "Library/PackageManager/projectResolution.json", {"outputs": resolution})
        return manifest, locked

    def test_exact_cached_packages_use_immutable_snapshots_without_rewriting_source_packages(self):
        manifest, locked = self.cached_package_fixture()
        before = imported_assets.source_inventory(self.project)
        with patch.object(imported_assets, "require_idle"):
            prepared = imported_assets.prepare_import(self.project, self.editor, ["Assets/Scene.unity"], cache_dir=self.cache)
        self.assertTrue(prepared["packageSnapshots"]["enabled"])
        self.assertEqual(imported_assets.source_inventory(self.project), before)
        copied = json.loads((Path(prepared["importProject"]) / "Packages/manifest.json").read_text())["dependencies"]
        self.assertEqual(copied["com.unity.modules.audio"], "1.0.0")
        for name in ("example.registry", "example.git"):
            self.assertTrue(copied[name].startswith("file:" + str(self.cache / "PackageSources")))
            self.assertTrue((Path(copied[name][5:]) / "Runtime.cs").is_file())
        proof = json.loads((self.cache / "PackageSources/.unity-play-package-resolution.json").read_text())["packages"]
        self.assertEqual(proof["example.git"]["gitHash"], locked["example.git"]["hash"])
        self.assertEqual(json.loads((self.project / "Packages/manifest.json").read_text()), manifest)

    def test_cached_package_snapshots_still_work_without_source_library(self):
        self.cached_package_fixture()
        with patch.object(imported_assets, "require_idle"):
            first = imported_assets.prepare_import(self.project, self.editor, ["Assets/Scene.unity"], cache_dir=self.cache)
            shutil.rmtree(self.project / "Library")
            second = imported_assets.prepare_import(self.project, self.editor, ["Assets/Scene.unity"], cache_dir=self.cache)
        self.assertTrue(second["packageSnapshots"]["enabled"])
        self.assertEqual([x["source"] for x in first["packageSnapshots"]["snapshots"]],
                         [x["source"] for x in second["packageSnapshots"]["snapshots"]])
        self.assertFalse((self.project / "Library").exists())

    def test_stale_git_resolution_cannot_acquire_a_new_lock_hash_during_mirror_sync(self):
        self.cached_package_fixture()
        with patch.object(imported_assets, "require_idle"):
            first = imported_assets.prepare_import(self.project, self.editor, ["Assets/Scene.unity"], cache_dir=self.cache)
        isolated = Path(first["importProject"])
        shutil.copytree(self.project / "Library", isolated / "Library")
        lock_path = self.project / "Packages/packages-lock.json"
        lock = json.loads(lock_path.read_text())
        lock["dependencies"]["example.git"]["hash"] = "9" * 40
        write_json(lock_path, lock)
        shutil.rmtree(self.project / "Library")
        with patch.object(imported_assets, "require_idle"):
            second = imported_assets.prepare_import(self.project, self.editor, ["Assets/Scene.unity"], cache_dir=self.cache)
        self.assertFalse(second["packageSnapshots"]["enabled"])
        self.assertIn("example.git", second["packageSnapshots"]["missing"])
        self.assertEqual(json.loads((isolated / "Packages/manifest.json").read_text()),
                         json.loads((self.project / "Packages/manifest.json").read_text()))

    def test_missing_exact_registry_version_leaves_normal_upm_manifest_intact(self):
        self.cached_package_fixture()
        resolution_path = self.project / "Library/PackageManager/projectResolution.json"
        resolution = json.loads(resolution_path.read_text())
        resolution["outputs"]["example.registry@1.2.3"]["version"] = "1.2.4"
        package = Path(resolution["outputs"]["example.registry@1.2.3"]["resolvedPath"]) / "package.json"
        write_json(package, {"name": "example.registry", "version": "1.2.4"})
        write_json(resolution_path, resolution)
        with patch.object(imported_assets, "require_idle"):
            prepared = imported_assets.prepare_import(self.project, self.editor, ["Assets/Scene.unity"], cache_dir=self.cache)
        self.assertFalse(prepared["packageSnapshots"]["enabled"])
        self.assertIn("example.registry", prepared["packageSnapshots"]["missing"])
        self.assertEqual((Path(prepared["importProject"]) / "Packages/manifest.json").read_bytes(),
                         (self.project / "Packages/manifest.json").read_bytes())


if __name__ == "__main__":
    unittest.main()
