import importlib.util
import hashlib
import json
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
spec = importlib.util.spec_from_file_location("benchmark_hybrid", ROOT / "benchmark_hybrid.py")
benchmark = importlib.util.module_from_spec(spec)
spec.loader.exec_module(benchmark)
from unity_play import ToolError


def wav(channels=2, width=2, frames=5000):
    payload = bytes(channels * width * frames)
    fmt = struct.pack("<HHIIHH", 1, channels, 44100, 44100 * channels * width, channels * width, width * 8)
    tail = b"LIST\x04\x00\x00\x00keep"
    body = b"WAVEfmt " + struct.pack("<I", len(fmt)) + fmt + b"data" + struct.pack("<I", len(payload)) + payload + tail
    return b"RIFF" + struct.pack("<I", len(body)) + body


class HybridBenchmarkTests(unittest.TestCase):
    def package_fixture(self, directory, *, source_kind="git"):
        root = Path(directory)
        project, cache = root / "source-copy", root / "fixture-cache"
        baseline = root / "baseline-cache/ImportProject"
        logical, name = "Packages/org.example.runtime", "org.example.runtime"
        origin, target = baseline.parent / "PackageSources/old", cache / "PackageSources/new"
        origin.mkdir(parents=True)
        (origin / "package.json").write_text(json.dumps({"name": name, "version": "1.0.0"}))
        (origin / "runtime.cs").write_text("public class Runtime {}")
        shutil.copytree(origin, target)
        inventory = benchmark._inventory(target)
        digest = hashlib.sha256(json.dumps(inventory, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
        packages = project / "Packages"
        packages.mkdir(parents=True)
        (packages / "manifest.json").write_text("{}")
        requirement = {"source": source_kind, "version": "https://example.com/repo.git" if source_kind == "git" else "1.0.0"}
        if source_kind == "git":
            requirement["hash"] = "abc123"
        (packages / "packages-lock.json").write_text(json.dumps({"dependencies": {name: requirement}}))
        snapshot = {"name": name, "source": str(target), "packageId": name + "@" + requirement["version"],
                    "packageSource": source_kind, "gitHash": requirement.get("hash"), "sha256": digest}
        sidecar = {"schemaVersion": 1, "originalPackages": {file: benchmark.sha256(packages / file)
                   for file in ("manifest.json", "packages-lock.json")}, "snapshots": [snapshot]}
        fixture_import = cache / "ImportProject"
        fixture_import.mkdir()
        (fixture_import / ".unity-play-package-snapshots.json").write_text(json.dumps(sidecar))
        original = {"project": str(root / "original-project"), "importProject": str(baseline),
                    "sourceRoots": [{"path": logical, "source": str(origin)}],
                    "sourceDependencies": [{"path": logical + "/runtime.cs", "physicalPath": str(origin / "runtime.cs"),
                    "sha256": benchmark.sha256(origin / "runtime.cs"), "bytes": (origin / "runtime.cs").stat().st_size}]}
        return original, project, cache, target, sidecar

    def test_owned_package_roots_and_dependencies_keep_fixture_paths_before_heavy_import(self):
        with tempfile.TemporaryDirectory() as directory:
            original, project, cache, target, _ = self.package_fixture(directory)
            roots = benchmark._owned_package_roots(original, project, cache, Path(directory) / "Unity/Editor/Unity")
            self.assertEqual(roots[0]["source"], str(target))
            dependencies = benchmark._remap_dependencies(original, project, roots)
            self.assertEqual(dependencies[0]["physicalPath"], str(target / "runtime.cs"))
            self.assertEqual(dependencies[0]["sha256"], original["sourceDependencies"][0]["sha256"])

    def test_owned_package_roots_reject_wrong_git_commit_provenance(self):
        with tempfile.TemporaryDirectory() as directory:
            original, project, cache, _, sidecar = self.package_fixture(directory)
            sidecar["snapshots"][0]["gitHash"] = "wrong-commit"
            (cache / "ImportProject/.unity-play-package-snapshots.json").write_text(json.dumps(sidecar))
            with self.assertRaisesRegex(ToolError, "Git provenance"):
                benchmark._owned_package_roots(original, project, cache, Path(directory) / "Unity/Editor/Unity")

    def test_owned_package_roots_reject_modified_bytes_even_when_sidecar_checksums_are_unchanged(self):
        with tempfile.TemporaryDirectory() as directory:
            original, project, cache, target, _ = self.package_fixture(directory)
            (target / "runtime.cs").write_text("public class Changed {}")
            with self.assertRaisesRegex(ToolError, "differs from the native catalog bytes"):
                benchmark._owned_package_roots(original, project, cache, Path(directory) / "Unity/Editor/Unity")

    def test_owned_package_roots_reject_external_snapshot_path(self):
        with tempfile.TemporaryDirectory() as directory:
            original, project, cache, _, sidecar = self.package_fixture(directory)
            sidecar["snapshots"][0]["source"] = original["sourceRoots"][0]["source"]
            (cache / "ImportProject/.unity-play-package-snapshots.json").write_text(json.dumps(sidecar))
            with self.assertRaisesRegex(ToolError, "owned path"):
                benchmark._owned_package_roots(original, project, cache, Path(directory) / "Unity/Editor/Unity")

    def test_builtin_package_roots_move_into_cloned_owned_library(self):
        with tempfile.TemporaryDirectory() as directory:
            original, project, cache, _, sidecar = self.package_fixture(directory)
            sidecar["snapshots"] = []
            (cache / "ImportProject/.unity-play-package-snapshots.json").write_text(json.dumps(sidecar))
            origin = Path(original["importProject"]) / "Library/PackageCache/org.example.runtime@123"
            shutil.copytree(Path(original["sourceRoots"][0]["source"]), origin)
            target = cache / "ImportProject/Library/PackageCache" / origin.name
            shutil.copytree(origin, target)
            original["sourceRoots"][0]["source"] = str(origin)
            roots = benchmark._owned_package_roots(original, project, cache, Path(directory) / "Unity/Editor/Unity")
            self.assertEqual(roots[0]["source"], str(target))

    def test_native_package_dependency_rejects_foreign_physical_binding(self):
        with tempfile.TemporaryDirectory() as directory:
            original, project, cache, target, _ = self.package_fixture(directory)
            original["sourceDependencies"][0]["physicalPath"] = str(Path(directory) / "foreign.cs")
            roots = [{"path": original["sourceRoots"][0]["path"], "source": str(target)}]
            with self.assertRaisesRegex(ToolError, "physical path differs"):
                benchmark._remap_dependencies(original, project, roots)

    def test_pcm_edit_preserves_encoding_duration_and_unrelated_chunks(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "audio.wav"
            original = wav()
            path.write_bytes(original)
            report = benchmark.edit_pcm(path)
            changed = path.read_bytes()
            offset, size, channels, width = benchmark._wav_layout(changed)
            self.assertEqual((offset, size, channels, width), benchmark._wav_layout(original))
            self.assertEqual(len(changed), len(original))
            self.assertEqual(changed[:offset], original[:offset])
            self.assertEqual(changed[offset + size:], original[offset + size:])
            self.assertEqual(changed[offset + 4096 * channels * width:], original[offset + 4096 * channels * width:])
            self.assertNotEqual(report["beforeSha256"], report["afterSha256"])
            self.assertNotEqual(struct.unpack_from("<h", changed, offset)[0], 0)

    def test_pcm_edit_supports_24_bit_channel_frames(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "audio.wav"
            path.write_bytes(wav(channels=3, width=3, frames=30))
            report = benchmark.edit_pcm(path)
            self.assertEqual(report["changedSampleValues"], 90)
            offset, _, _, width = benchmark._wav_layout(path.read_bytes())
            self.assertEqual(width, 3)
            self.assertGreater(int.from_bytes(path.read_bytes()[offset:offset+width], "little", signed=True), 0)

    def test_pcm_edit_rejects_truncated_data(self):
        with self.assertRaisesRegex(ToolError, "truncated"):
            benchmark._wav_layout(wav()[:-17])

    def test_pcm_edit_rejects_compressed_data(self):
        data = bytearray(wav())
        struct.pack_into("<H", data, 20, 3)
        with self.assertRaisesRegex(ToolError, "integer PCM"):
            benchmark._wav_layout(data)

    def test_scene_edit_changes_only_the_selected_64_bit_transform(self):
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            scene = project / benchmark.SCENE
            scene.parent.mkdir(parents=True)
            first = "--- !u!4 &900000000000001\nTransform:\n  m_LocalPosition: {x: 0, y: 2, z: 3}\n"
            second = "--- !u!4 &900000000000002\nTransform:\n  m_LocalPosition: {x: 7, y: 8, z: 9}\n"
            scene.write_text("%YAML 1.1\n" + first + second)
            result = benchmark.edit_scene(project, "900000000000001")
            self.assertEqual(result, {"x": .125, "y": 2, "z": 3})
            self.assertIn(second, scene.read_text())

    def test_fork_rejects_any_changed_native_input_before_publishing(self):
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            with patch.object(benchmark, "source_inventory", return_value={}), patch.object(
                    benchmark, "_dependency_signature", return_value={"nativeInputs": {"audio": "new"}}):
                with self.assertRaisesRegex(ToolError, "nativeInputs"):
                    benchmark._fork_catalog({"assets": []}, project, project / "cache", {"nativeInputs": {"audio": "old"}})
            self.assertFalse((project / "cache").exists())

    def test_proof_rejects_a_success_flag_without_actual_scene_state(self):
        proof = {"schemaVersion": 1, "success": True, "codeRevision": 2, "sceneName": "SampleScene",
                 "sceneTargetLocalPosition": {"x": 0, "y": 0, "z": 0}, "sceneReferencesRestored": True,
                 "dungeonTargetAwakeObserved": True, "dungeonTargetType": "Dungeon.DungeonShootingTarget",
                 "audioFingerprint": "12345678", "audioReadValues": 1024, "audioRms": .1}
        benchmark._proof_matches(proof, 2, {"x": 0, "y": 0, "z": 0})
        with self.assertRaisesRegex(ToolError, "source scene position"):
            benchmark._proof_matches(proof, 2, {"x": .125, "y": 0, "z": 0})
        proof["dungeonTargetAwakeObserved"] = False
        with self.assertRaisesRegex(ToolError, "not initialized"):
            benchmark._proof_matches(proof, 2, {"x": 0, "y": 0, "z": 0})

    def test_proof_rejects_nan_scene_values_and_infinite_audio(self):
        proof = {"schemaVersion": 1, "success": True, "codeRevision": 1, "sceneName": "SampleScene",
                 "sceneTargetLocalPosition": {"x": float("nan")}}
        with self.assertRaisesRegex(ToolError, "source scene position"):
            benchmark._proof_matches(proof, 1, {"x": 0})
        proof.update(sceneTargetLocalPosition={"x": 0}, sceneReferencesRestored=True,
                     dungeonTargetAwakeObserved=True, dungeonTargetType="Dungeon.DungeonShootingTarget",
                     audioFingerprint="12345678", audioReadValues=1024, audioRms=float("inf"))
        with self.assertRaisesRegex(ToolError, "not initialized"):
            benchmark._proof_matches(proof, 1, {"x": 0})

    def test_cli_latency_includes_overhead_outside_the_raw_prepare_phase(self):
        record = {"runtime": {"readyUtcUnixMilliseconds": 1009000},
                  "resources": {"startedUtcUnixMilliseconds": 1007000},
                  "rawBuild": {"prepareSeconds": 5}}
        proof = {"observedUtcUnixMilliseconds": 1009500}
        result = benchmark._timings(record, proof, 1000000, 12)
        self.assertEqual(result["commandToReadySeconds"], 9)
        self.assertEqual(result["commandToProbeSeconds"], 9.5)
        self.assertEqual(result["prepareToProbeSeconds"], 7.5)

    def test_latency_rejects_stale_proof_after_a_repeated_launch(self):
        record = {"runtime": {"readyUtcUnixMilliseconds": 1009000},
                  "resources": {"startedUtcUnixMilliseconds": 1007000},
                  "rawBuild": {"prepareSeconds": 5}}
        with self.assertRaisesRegex(ToolError, "inconsistent"):
            benchmark._timings(record, {"observedUtcUnixMilliseconds": 500000}, 1000000, 12)

    def test_missing_gpu_pid_is_not_reported_as_a_zero_memory_scene(self):
        resources = {"scope": "single-process", "steady": {"available": True, "sampleCount": 10,
                     "startElapsedSeconds": 1, "endElapsedSeconds": 3, "meanRssMiB": 250,
                     "meanGpuMiB": 0}, "samples": [{"status": "available", "elapsedSeconds": 2,
                     "gpu": {"status": "available", "devices": [{"pidListed": False}]}}]}
        measurement = benchmark._resource_measurement(resources)
        self.assertFalse(measurement["gpuMemoryMeasured"])
        self.assertIn("did not list", measurement["gpuMemoryUnavailableReason"])
        resources["samples"][0]["gpu"]["devices"][0]["pidListed"] = True
        resources["steady"]["meanGpuMiB"] = 120
        self.assertTrue(benchmark._resource_measurement(resources)["gpuMemoryMeasured"])

    def test_resource_measurement_rejects_short_or_empty_steady_window(self):
        resources = {"scope": "single-process", "steady": {"available": True, "sampleCount": 2,
                     "startElapsedSeconds": 1, "endElapsedSeconds": 1.3, "meanRssMiB": 250}}
        with self.assertRaisesRegex(ToolError, "shorter than"):
            benchmark._resource_measurement(resources)
        resources["steady"].update(endElapsedSeconds=3, meanRssMiB=None)
        with self.assertRaisesRegex(ToolError, "no measured RSS"):
            benchmark._resource_measurement(resources)

    def test_outer_timeout_stops_editor_and_player_descendants_before_parent_exit(self):
        process = Mock()
        process.wait.side_effect = subprocess.TimeoutExpired(["unity-play"], 5)
        with patch.object(benchmark.subprocess, "Popen", return_value=process) as popen, patch.object(benchmark, "stop_child") as stop:
            with self.assertRaises(subprocess.TimeoutExpired):
                benchmark._run_cli(["unity-play"], None, None, {}, 5)
            self.assertTrue(popen.call_args.kwargs["start_new_session"])
            stop.assert_called_once_with(process)


if __name__ == "__main__":
    unittest.main()
