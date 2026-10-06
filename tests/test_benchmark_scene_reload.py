import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import benchmark_scene_reload as benchmark
from raw_scene import parse_unity_yaml
from unity_play import ToolError


SCENE = ("%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n"
         "--- !u!1 &1\nGameObject:\n  m_Component:\n  - component: {fileID: 2}\n  m_Name: Floor\n  m_IsActive: 1\n"
         "--- !u!4 &2\nTransform:\n  m_GameObject: {fileID: 1}\n"
         "  m_LocalPosition: {x: 0, y: -0.25, z: 0}\n  m_Father: {fileID: 0}\n"
         "--- !u!114 &900000000000002\nMonoBehaviour:\n  m_GameObject: {fileID: 1}\n"
         "  iterationRevision: 1\n  addedObjects: []\n  unchangedField: preserve\n"
         "--- !u!1660057539 &99\nSceneRoots:\n  m_Roots:\n  - {fileID: 2}\n")


class SceneReloadBenchmarkTests(unittest.TestCase):
    def fixture(self, directory):
        project = Path(directory)
        scene = project / benchmark.hybrid.SCENE
        scene.parent.mkdir(parents=True)
        scene.write_text(SCENE)
        return project, scene

    def test_transform_mutation_preserves_graph_and_serialized_array(self):
        with tempfile.TemporaryDirectory() as directory:
            project, scene = self.fixture(directory)
            position = benchmark.transform_edit(project, "2", 2, [])
            self.assertEqual(position, {"x": .125, "y": -.25, "z": 0})
            documents = parse_unity_yaml(scene)
            self.assertEqual(len(documents), 4)
            probe = next(item for item in documents if item["classId"] == 114)
            self.assertEqual(probe["data"]["iterationRevision"], 2)
            self.assertEqual(probe["data"]["addedObjects"], [])
            self.assertEqual(probe["data"]["unchangedField"], "preserve")
            self.assertFalse(list(scene.parent.glob("*.benchmark.tmp")))

    def test_additions_have_exact_owner_transform_root_and_probe_references(self):
        with tempfile.TemporaryDirectory() as directory:
            project, scene = self.fixture(directory)
            first = benchmark.add_object_edit(project, 2, [])
            second = benchmark.add_object_edit(project, 3, [first])
            documents = parse_unity_yaml(scene)
            by_id = {item["fileId"]: item for item in documents}
            self.assertEqual(len(documents), 8)
            for item in (first, second):
                self.assertEqual(by_id[item["gameObjectId"]]["data"]["m_Name"], item["name"])
                self.assertEqual(by_id[item["transformId"]]["data"]["m_GameObject"]["fileID"], item["gameObjectId"])
            roots = next(item["data"]["m_Roots"] for item in documents if item["classId"] == 1660057539)
            self.assertEqual({entry["fileID"] for entry in roots}, {"2", first["transformId"], second["transformId"]})
            probe = by_id[benchmark.PROBE_COMPONENT]["data"]
            self.assertEqual(probe["iterationRevision"], 3)
            self.assertEqual([entry["fileID"] for entry in probe["addedObjects"]], [first["gameObjectId"], second["gameObjectId"]])
            self.assertEqual(probe["unchangedField"], "preserve")

    def test_addition_rejects_an_id_collision_without_modifying_source(self):
        with tempfile.TemporaryDirectory() as directory:
            project, scene = self.fixture(directory)
            scene.write_text(scene.read_text() + "--- !u!1 &900000000100002\nGameObject:\n  m_Name: Existing\n")
            before = scene.read_bytes()
            with self.assertRaisesRegex(ToolError, "collides"):
                benchmark.add_object_edit(project, 2, [])
            self.assertEqual(scene.read_bytes(), before)

    def proof(self):
        proof = {"schemaVersion": 1, "success": True, "pid": 42, "sceneName": "SampleScene",
                 "iterationRevision": 2, "dataRevision": 3, "awakeReferencesRestored": True,
                 "sceneReferencesRestored": True, "dungeonTargetAwakeObserved": True,
                 "addedObjectNames": ["New object"], "sceneTargetLocalPosition": {"x": .125, "y": -.25, "z": 0},
                 "graphicsDeviceType": "OpenGLCore", "graphicsDeviceName": "NVIDIA GeForce RTX 4060"}
        proof.update({name: str(index + 100) for index, name in enumerate(benchmark.NATIVE_IDS)})
        return proof

    def validate(self, proof, baseline=None):
        benchmark.validate_proof(proof, baseline, pid=42, revision=2, data_revision=3,
                                 position={"x": .125, "y": -.25, "z": 0}, added=[{"name": "New object"}])

    def test_proof_requires_the_same_native_asset_instances(self):
        baseline = self.proof()
        self.validate(self.proof(), baseline)
        for field in benchmark.NATIVE_IDS:
            changed = self.proof()
            changed[field] = "changed-identity"
            with self.subTest(field=field), self.assertRaisesRegex(ToolError, "replaced"):
                self.validate(changed, baseline)

    def test_proof_rejects_stale_values_wrong_pid_missing_added_object_and_software_gpu(self):
        changes = [{"pid": 43}, {"iterationRevision": 1}, {"dataRevision": 2}, {"addedObjectNames": []},
                   {"graphicsDeviceName": "llvmpipe"}, {"awakeReferencesRestored": False}]
        for change in changes:
            proof = {**self.proof(), **change}
            with self.subTest(change=change), self.assertRaises(ToolError):
                self.validate(proof)

    def test_proof_rejects_nonfinite_transform(self):
        proof = self.proof()
        proof["sceneTargetLocalPosition"]["x"] = float("nan")
        with self.assertRaisesRegex(ToolError, "numeric"):
            self.validate(proof)

    def ack_fixture(self, directory):
        manifest = Path(directory) / "scene.json"
        manifest.write_text(json.dumps({"revision": "current"}))
        request = {"requestId": "request", "manifest": str(manifest),
                   "manifestSha256": benchmark.sha256(manifest), "revision": "current",
                   "requestedUtcUnixMilliseconds": 1100}
        ack = {"schemaVersion": 1, "success": True, "requestId": "request", "manifest": str(manifest),
               "pid": 42, "revision": "current", "errors": 0, "exceptions": 0, "fatalError": "",
               "completedUtcUnixMilliseconds": 1200, "elapsedSeconds": .05}
        return request, ack

    def test_ack_requires_one_unchanged_native_registry_load(self):
        with tempfile.TemporaryDirectory() as directory:
            request, ack = self.ack_fixture(directory)
            identity = "a" * 64
            ack.update(nativeLoadCount=1, nativeObjectCount=12, nativeIdentityDigest=identity)
            benchmark.validate_ack(ack, request, pid=42, edit_utc_ms=1000,
                                   native_identity=identity, native_object_count=12)
            for change in ({"nativeLoadCount": 2}, {"nativeObjectCount": 11}, {"nativeIdentityDigest": "b" * 64}):
                with self.subTest(change=change), self.assertRaisesRegex(ToolError, "retained"):
                    benchmark.validate_ack({**ack, **change}, request, pid=42, edit_utc_ms=1000,
                                           native_identity=identity, native_object_count=12)

    def test_ack_validates_manifest_hash_exact_revision_pid_and_request_id(self):
        with tempfile.TemporaryDirectory() as directory:
            request, ack = self.ack_fixture(directory)
            benchmark.validate_ack(ack, request, pid=42, edit_utc_ms=1000)
            for change in ({"pid": 43}, {"revision": "old"}, {"requestId": "old"}, {"success": False}, {"exceptions": 1}):
                with self.subTest(change=change), self.assertRaises(ToolError):
                    benchmark.validate_ack({**ack, **change}, request, pid=42, edit_utc_ms=1000)
            Path(request["manifest"]).write_text(json.dumps({"revision": "changed-after-ack"}))
            with self.assertRaisesRegex(ToolError, "SHA"):
                benchmark.validate_ack(ack, request, pid=42, edit_utc_ms=1000)

    def test_ack_rejects_clock_jumps_and_nonfinite_runtime_measurements(self):
        with tempfile.TemporaryDirectory() as directory:
            request, ack = self.ack_fixture(directory)
            for change in ({"completedUtcUnixMilliseconds": 900}, {"elapsedSeconds": -.1},
                           {"elapsedSeconds": float("inf")}, {"completedUtcUnixMilliseconds": True}):
                with self.subTest(change=change), self.assertRaises(ToolError):
                    benchmark.validate_ack({**ack, **change}, request, pid=42, edit_utc_ms=1000)

    def test_data_edit_preserves_other_authored_fields(self):
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            path = project / benchmark.DATA_PATH
            path.parent.mkdir(parents=True)
            path.write_text("MonoBehaviour:\n  m_Name: Keep\n  revision: 1\n  other: 7\n")
            benchmark.data_edit(project, 5)
            self.assertEqual(path.read_text(), "MonoBehaviour:\n  m_Name: Keep\n  revision: 5\n  other: 7\n")


if __name__ == "__main__":
    unittest.main()
