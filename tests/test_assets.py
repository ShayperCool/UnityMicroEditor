"""Content publishes only after a successful, verified complete scene export."""
from __future__ import annotations

import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import assets
from recompile import digest, project_files
from unity_play import ToolError, write_json


class AssetExportTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.project = Path(self.temp.name) / "project"
        self.project.mkdir()
        settings = self.project / "ProjectSettings"
        settings.mkdir()
        (settings / "ProjectVersion.txt").write_text("m_EditorVersion: 6000.6.3f1\n")
        self.editor = Path(self.temp.name) / "Unity"
        self.editor.write_text("fake editor")
        bridge = self.project / "Assets/Dungeon/CLIPlay/Editor/AssetBuild.cs"
        bridge.parent.mkdir(parents=True)
        bridge.write_text("class Exporter {}")
        self.scene = self.project / "Assets/Demo.unity"
        self.scene.write_text("scene revision 1")
        self.code = self.project / "Assets/Simple.cs"
        self.code.write_text("class Simple {}")
        self.player = self.project / "Builds/CLIPlay/Game.x86_64"
        self.player.parent.mkdir(parents=True)
        self.player.write_bytes(b"player")
        self.managed = self.player.parent / "Game_Data/Managed"
        self.managed.mkdir(parents=True)
        (self.managed / "Simple.dll").write_bytes(b"managed")
        self.capture_baseline()
        write_json(self.player.parent / "build.json", {"engineVersion": "6000.6.3f1", "graphicsApi": "glcore"})
        self.modify_raw = None
        self.after_build = None
        self.failure = False

    def tearDown(self):
        self.temp.cleanup()

    def capture_baseline(self):
        sources, asset_hashes = project_files(self.project)
        write_json(self.player.parent / "code-refresh.json", {
            "schema": 1, "project": str(self.project), "player": str(self.player), "sources": sources, "assets": asset_hashes,
            "assemblies": {"Simple": {"dll": str(self.managed / "Simple.dll"), "dllSha256": digest(self.managed / "Simple.dll")}},
        })

    def editor_process(self, command, project, log, timeout, environment):
        cache = Path(environment["UNITY_CLI_CONTENT_OUTPUT"])
        if self.failure:
            return {"exitCode": 1, "log": str(log), "wallSeconds": .1, "peakRssMiB": 1}
        bundle = cache / "cli-play-scenes"
        bundle.write_bytes(b"bundle:" + self.scene.read_bytes())
        (cache / f"{cache.name}.manifest").write_text("preserved native and managed types")
        raw = {
            "schemaVersion": 1, "success": True, "target": "StandaloneLinux64", "engineVersion": "6000.6.3f1",
            "graphicsApi": "glcore", "scenes": ["Assets/Demo.unity"], "sceneBundle": "cli-play-scenes",
            "scriptingBackend": "Mono2x", "development": True,
            "bundles": [{"name": "cli-play-scenes", "file": "cli-play-scenes", "sha256": digest(bundle),
                         "crc": 42, "bytes": bundle.stat().st_size, "dependencies": []}],
            "sourceDependencies": [{"path": "Assets/Demo.unity", "sha256": digest(self.scene)}],
            "requiredAssemblies": ["Simple"], "typePreservationManifest": str(cache / f"{cache.name}.manifest"),
            "elapsedSeconds": .05,
        }
        if self.modify_raw:
            self.modify_raw(raw)
        write_json(Path(environment["UNITY_CLI_CONTENT_MANIFEST"]), raw)
        if self.after_build:
            self.after_build()
        return {"exitCode": 0, "log": str(log), "wallSeconds": .1, "peakRssMiB": 1}

    def export(self, **options):
        with patch.object(assets, "run_process", side_effect=self.editor_process):
            return assets.export(self.project, self.editor, self.player, ["Assets/Demo.unity"], **options)

    def test_complete_content_is_immutable_and_current_changes_atomically(self):
        first = self.export()
        original_manifest = Path(first["manifest"]).read_bytes()
        self.assertTrue(first["published"])
        self.assertEqual(assets.inspect_content(Path(first["current"]))["revision"], first["revision"])
        self.scene.write_text("scene revision 2")
        second = self.export()
        self.assertTrue(second["codeRefreshAssetChecks"]["accepted"])
        self.assertEqual(second["codeRefreshAssetChecks"]["paths"], ["Assets/Demo.unity"])
        self.assertNotEqual(first["revision"], second["revision"])
        self.assertEqual(Path(first["manifest"]).read_bytes(), original_manifest)
        self.assertEqual(assets.inspect_content(Path(second["current"]))["revision"], second["revision"])
        self.assertEqual(self.export()["revision"], second["revision"])
        self.assertTrue(Path(second["typePreservationManifest"]).is_file())

    def test_failed_editor_and_invalid_bundle_leave_prior_pointer_unchanged(self):
        first = self.export()
        pointer = Path(first["current"]).read_bytes()
        self.failure = True
        with self.assertRaisesRegex(ToolError, "prior content is unchanged"):
            self.export()
        self.assertEqual(Path(first["current"]).read_bytes(), pointer)
        self.failure = False
        self.modify_raw = lambda raw: raw["bundles"][0].update(sha256="0" * 64)
        with self.assertRaisesRegex(ToolError, "SHA256 mismatch"):
            self.export()
        self.assertEqual(Path(first["current"]).read_bytes(), pointer)

    def test_unknown_dependency_and_escaping_paths_are_rejected(self):
        self.modify_raw = lambda raw: raw["bundles"][0].update(dependencies=["absent"])
        with self.assertRaisesRegex(ToolError, "Unknown bundle dependency"):
            self.export()
        self.modify_raw = lambda raw: raw["bundles"][0].update(file="../../elsewhere")
        with self.assertRaisesRegex(ToolError, "escapes"):
            self.export()
        self.assertFalse(assets.current_manifest(self.player).exists())

    def test_cyclic_bundle_dependencies_are_rejected(self):
        self.modify_raw = lambda raw: raw["bundles"][0].update(dependencies=["cli-play-scenes"])
        with self.assertRaisesRegex(ToolError, "cycle"):
            self.export()

    def test_source_or_scene_edit_during_export_does_not_publish(self):
        self.after_build = lambda: self.code.write_text("class ModifiedCode {}")
        with self.assertRaisesRegex(ToolError, "C# source changed during"):
            self.export()
        self.capture_baseline()
        self.after_build = lambda: self.scene.write_text("edited after cooking")
        with self.assertRaisesRegex(ToolError, "SHA256 mismatch"):
            self.export()
        self.assertFalse(assets.current_manifest(self.player).exists())

    def test_preexisting_code_edits_and_missing_player_types_require_shell_build(self):
        self.code.write_text("class NewClass {}")
        with self.assertRaisesRegex(ToolError, "C# source changed since"):
            self.export()
        self.capture_baseline()
        (self.managed / "Simple.dll").unlink()
        with self.assertRaisesRegex(ToolError, "Player DLL"):
            self.export()
        (self.managed / "Simple.dll").write_bytes(b"managed")
        self.modify_raw = lambda raw: raw.update(requiredAssemblies=["MissingTypeAssembly"])
        with self.assertRaisesRegex(ToolError, "assembly absent"):
            self.export()

    def test_changed_compilation_settings_are_rejected_before_editor_launch(self):
        first = self.export()
        pointer = Path(first["current"]).read_bytes()
        paths = ["Assets/Demo.asmdef", "Assets/Demo.asmref", "Assets/Demo.asmdef.meta",
                 "Packages/manifest.json", "Packages/packages-lock.json",
                 "ProjectSettings/EditorSettings.asset", "ProjectSettings/ProjectVersion.txt"]
        for relative in paths:
            with self.subTest(path=relative):
                path = self.project / relative
                original = path.read_bytes() if path.exists() else None
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("changed compilation contract")
                try:
                    with patch.object(assets, "run_process") as editor:
                        with self.assertRaisesRegex(ToolError, "compilation settings"):
                            assets.export(self.project, self.editor, self.player, ["Assets/Demo.unity"])
                        editor.assert_not_called()
                    self.assertEqual(Path(first["current"]).read_bytes(), pointer)
                finally:
                    if original is None:
                        path.unlink()
                    else:
                        path.write_bytes(original)

    def test_compilation_contract_changes_during_export_preserve_current(self):
        first = self.export()
        pointer = Path(first["current"]).read_bytes()
        paths = ["Assets/Demo.asmdef", "Assets/Demo.asmref", "Packages/manifest.json",
                 "Packages/packages-lock.json", "ProjectSettings/EditorSettings.asset"]
        for relative in paths:
            with self.subTest(path=relative):
                path = self.project / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                self.after_build = lambda: path.write_text("changed after cooking")
                try:
                    with self.assertRaisesRegex(ToolError, "compilation settings.*during content export"):
                        self.export()
                    self.assertEqual(Path(first["current"]).read_bytes(), pointer)
                finally:
                    path.unlink(missing_ok=True)
        self.after_build = None

    def test_all_eligible_player_dll_hashes_are_checked_before_and_after_export(self):
        other = self.managed / "Unused.dll"
        other.write_bytes(b"unused original")
        baseline = self.player.parent / "code-refresh.json"
        value = json.loads(baseline.read_text())
        value["assemblies"]["Unused"] = {"dll": str(other), "dllSha256": digest(other)}
        write_json(baseline, value)
        first = self.export()
        pointer = Path(first["current"]).read_bytes()
        other.write_bytes(b"changed before export")
        with self.assertRaisesRegex(ToolError, "Player DLL.*SHA256 mismatch"):
            self.export()
        self.assertEqual(Path(first["current"]).read_bytes(), pointer)
        other.write_bytes(b"unused original")
        self.after_build = lambda: other.write_bytes(b"changed during export")
        with self.assertRaisesRegex(ToolError, "Player DLL.*SHA256 mismatch"):
            self.export()
        self.assertEqual(Path(first["current"]).read_bytes(), pointer)

    def test_baseline_identity_and_concurrent_baseline_changes_do_not_publish(self):
        first = self.export()
        pointer = Path(first["current"]).read_bytes()
        manifest = self.player.parent / "code-refresh.json"
        baseline_bytes = manifest.read_bytes()
        for key, replacement in (("schema", 2), ("project", "other-project"), ("player", "other-player")):
            with self.subTest(field=key):
                value = json.loads(baseline_bytes)
                value[key] = replacement
                write_json(manifest, value)
                with self.assertRaisesRegex(ToolError, "different project/Player"):
                    self.export()
                self.assertEqual(Path(first["current"]).read_bytes(), pointer)
                manifest.write_bytes(baseline_bytes)
        def changed_baseline():
            value = json.loads(baseline_bytes)
            value["assets"]["Assets/Unrelated.asset"] = "0" * 64
            write_json(manifest, value)
        self.after_build = changed_baseline
        with self.assertRaisesRegex(ToolError, "compiler baseline changed during content export"):
            self.export()
        self.assertEqual(Path(first["current"]).read_bytes(), pointer)

    def test_prepare_before_shell_build_never_publishes_current(self):
        self.player.unlink()
        (self.player.parent / "code-refresh.json").unlink()
        result = self.export(prepare_only=True)
        self.assertFalse(result["published"])
        self.assertIsNone(result["current"])
        self.assertTrue(Path(result["manifest"]).is_file())
        self.assertFalse(assets.current_manifest(self.player).exists())

    def test_dry_run_does_not_create_cache_or_content(self):
        result = self.export(dry_run=True)
        self.assertTrue(result["dryRun"])
        self.assertFalse((self.project / "Library").exists())
        self.assertFalse((self.project / "Builds/CLIPlay/.asset-cache").exists())
        self.assertFalse((self.project / "Logs").exists())
        self.assertFalse(assets.current_manifest(self.player).exists())

    def test_pointer_and_revision_tampering_is_detected(self):
        result = self.export()
        Path(result["manifest"]).write_text("modified manifest")
        with self.assertRaisesRegex(ToolError, "SHA256 mismatch"):
            assets.inspect_content(Path(result["current"]))

    def test_virtual_package_source_uses_verified_physical_cache_file(self):
        logical = "Packages/com.unity.render-pipelines.core/Editor/Icon.asset"
        physical = "Library/PackageCache/com.unity.render-pipelines.core@abc123/Editor/Icon.asset"
        source = self.project / physical
        source.parent.mkdir(parents=True)
        source.write_text("cached package asset")
        self.modify_raw = lambda raw: raw["sourceDependencies"].append(
            {"path": logical, "physicalPath": physical, "sha256": digest(source)})
        result = self.export()
        self.assertTrue(result["published"])
        self.assertTrue(result["codeRefreshAssetChecks"]["accepted"])
        pointer = Path(result["current"]).read_bytes()
        self.after_build = lambda: source.write_text("changed after cooking")
        with self.assertRaisesRegex(ToolError, "SHA256 mismatch"):
            self.export()
        self.assertEqual(Path(result["current"]).read_bytes(), pointer)

    def test_embedded_package_source_is_supported_without_physical_override(self):
        source = self.project / "Packages/com.example.demo/Material.asset"
        source.parent.mkdir(parents=True)
        source.write_text("embedded package asset")
        self.capture_baseline()
        self.modify_raw = lambda raw: raw["sourceDependencies"].append(
            {"path": source.relative_to(self.project).as_posix(), "sha256": digest(source)})
        self.assertTrue(self.export()["published"])

    def test_package_remapping_cannot_escape_or_substitute_another_source(self):
        logical = "Packages/com.example.demo/Material.asset"
        invalid = ["../outside", "/tmp/Material.asset", "Assets/Demo.unity",
                   "Library/PackageCache/com.example.other@123/Material.asset",
                   "Library/PackageCache/com.example.demo@123/Other.asset",
                   "Library/PackageCache/com.example.demo@123/../Material.asset"]
        for physical in invalid:
            with self.subTest(physical=physical):
                self.modify_raw = lambda raw: raw["sourceDependencies"].append(
                    {"path": logical, "physicalPath": physical, "sha256": "0" * 64})
                with self.assertRaises(ToolError):
                    self.export()
        self.assertFalse(assets.current_manifest(self.player).exists())

    def test_package_cache_symlink_cannot_escape_project(self):
        external = Path(self.temp.name) / "external"
        external.mkdir()
        source = external / "Material.asset"
        source.write_text("external file")
        cache = self.project / "Library/PackageCache"
        cache.mkdir(parents=True)
        (cache / "com.example.demo@123").symlink_to(external, target_is_directory=True)
        self.modify_raw = lambda raw: raw["sourceDependencies"].append({
            "path": "Packages/com.example.demo/Material.asset",
            "physicalPath": "Library/PackageCache/com.example.demo@123/Material.asset", "sha256": digest(source)})
        with self.assertRaisesRegex(ToolError, "escapes its project"):
            self.export()
        self.assertFalse(assets.current_manifest(self.player).exists())


if __name__ == "__main__":
    unittest.main()
