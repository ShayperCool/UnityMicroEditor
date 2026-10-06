import json
import hashlib
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import raw
import raw_compile
import raw_scene
import unity_play


class RawPublicationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.project = self.root / "project"
        (self.project / "ProjectSettings").mkdir(parents=True)
        (self.project / "ProjectSettings/ProjectVersion.txt").write_text("m_EditorVersion: 6000.6.3f1\n")
        self.host = self.root / "host/Host.x86_64"
        (self.host.parent / "Host_Data/Managed").mkdir(parents=True)
        self.host.write_bytes(b"native player")
        (self.host.parent / "Host_Data/Managed/Dungeon.CLIPlay.dll").write_bytes(b"bridge")
        (self.host.parent / "host.json").write_text(json.dumps(
            {"engineVersion": "6000.6.3f1", "inputProjectAssetsBundled": False}))
        self.output = self.root / "output"
        self.output.mkdir()
        (self.output / "current.json").write_text('{"manifest":"previous"}\n')
        self.pointer = (self.output / "current.json").read_bytes()

    def tearDown(self):
        self.temp.cleanup()

    def test_compile_error_preserves_previous_pointer_and_cleans_failed_revision(self):
        with patch.object(raw_scene, "prepare_scene", return_value={}), \
             patch.object(raw_compile, "compile_project", side_effect=raw_compile.RawCompileError("syntax error")):
            with self.assertRaisesRegex(unity_play.ToolError, "syntax error"):
                raw.prepare_raw(self.project, self.root / "Unity", self.host, "scene", self.output)
        self.assertEqual((self.output / "current.json").read_bytes(), self.pointer)
        self.assertEqual(list((self.output / "revisions").iterdir()), [])

    def test_lifecycle_protocol_requires_every_assembly_and_all_phases(self):
        compiler = {"capabilities": {"lifecycleBarrier": True}, "assemblies": [
            {"path": "first.dll", "lifecycleBarrier": {"schemaVersion": 1, "verified": True,
                "phases": ["Awake", "OnEnable", "OnDisable", "OnDestroy"]}},
            {"path": "second.dll"}]}
        with patch.object(raw_scene, "prepare_scene", return_value={}), \
             patch.object(raw_compile, "compile_project", return_value=compiler):
            with self.assertRaisesRegex(unity_play.ToolError, "Incomplete lifecycle verification.*second"):
                raw.prepare_raw(self.project, self.root / "Unity", self.host, "scene", self.output)
        self.assertEqual((self.output / "current.json").read_bytes(), self.pointer)
        self.assertEqual(list((self.output / "revisions").iterdir()), [])

    def test_verified_protocol_carries_exact_assembly_paths_and_legacy_stays_disabled(self):
        proof = {"schemaVersion": 1, "verified": True,
                 "phases": ["Awake", "OnEnable", "OnDisable", "OnDestroy"]}
        compiler = {"capabilities": {"lifecycleBarrier": True}, "assemblies": [
            {"path": "first.dll", "lifecycleBarrier": proof},
            {"path": "second.dll", "lifecycleBarrier": proof}]}
        manifest = {}
        raw._attach_lifecycle_protocol(manifest, compiler)
        self.assertEqual(manifest["lifecycleBarrierVersion"], 1)
        self.assertEqual(manifest["lifecycleBarrierAssemblies"], ["first.dll", "second.dll"])
        raw._attach_lifecycle_protocol(manifest, {"assemblies": []})
        self.assertEqual(manifest["lifecycleBarrierVersion"], 0)
        self.assertEqual(manifest["lifecycleBarrierAssemblies"], [])

    def test_published_texture_is_snapshot_and_preserves_authored_name(self):
        source = self.project / "Assets/Test.png"
        source.parent.mkdir()
        source.write_bytes(b"source texture")
        graph = {"assets": [{"kind": "texture", "path": "Assets/Test.png", "name": "Test"}],
                 "sourceDependencies": [{"path": "Assets/Test.png", "sha256": hashlib.sha256(source.read_bytes()).hexdigest()}]}
        compiler = {"assemblies": [], "scripts": []}
        with patch.object(raw_scene, "prepare_scene", return_value=graph), \
             patch.object(raw_compile, "compile_project", return_value=compiler):
            result = raw.prepare_raw(self.project, self.root / "Unity", self.host, "scene", self.output)
        source.write_bytes(b"changed later")
        published = json.loads(Path(result["manifest"]).read_text())
        self.assertEqual(Path(published["assets"][0]["path"]).read_bytes(), b"source texture")
        self.assertEqual(published["assets"][0]["name"], "Test")
        self.assertEqual(json.loads((self.output / "current.json").read_text())["revision"], result["revision"])

    def test_texture_copy_must_match_parsed_source_hash_even_if_source_is_restored(self):
        source = self.project / "Assets/Test.png"
        source.parent.mkdir()
        source.write_bytes(b"source texture")
        graph = {"assets": [{"kind": "texture", "path": "Assets/Test.png", "name": "Test"}],
                 "sourceDependencies": [{"path": "Assets/Test.png", "sha256": hashlib.sha256(source.read_bytes()).hexdigest()}]}
        def raced_copy(source_path, destination):
            destination.write_bytes(b"intermediate edited texture")
        with patch.object(raw_scene, "prepare_scene", return_value=graph), \
             patch.object(raw_compile, "compile_project", return_value={"assemblies": []}), \
             patch.object(raw.shutil, "copyfile", side_effect=raced_copy):
            with self.assertRaisesRegex(unity_play.ToolError, "texture changed"):
                raw.prepare_raw(self.project, self.root / "Unity", self.host, "scene", self.output)
        self.assertEqual((self.output / "current.json").read_bytes(), self.pointer)
        self.assertEqual(list((self.output / "revisions").iterdir()), [])


if __name__ == "__main__":
    unittest.main()
