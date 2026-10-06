import ctypes
import contextlib
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import resources
import unity_play as cli


class ResourceSamplerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.proc = Path(self.temp.name)
        self.process = self.proc / "42"
        self.process.mkdir()
        self.now = 0.0
        self.write_stat(100)
        self.memory(1024, 768, 512)

    def tearDown(self):
        self.temp.cleanup()

    def write_stat(self, cpu_ticks, identity=99, state="S"):
        fields = ["0"] * 50
        fields[0], fields[11], fields[12] = state, str(cpu_ticks), "0"
        fields[19], fields[21] = str(identity), "256"
        (self.process / "stat").write_text("42 (worker (probe)) " + " ".join(fields))

    def memory(self, rss_kib, pss_kib, private_kib):
        (self.process / "smaps_rollup").write_text(
            f"Rss: {rss_kib} kB\nPss: {pss_kib} kB\nPrivate_Clean: 0 kB\nPrivate_Dirty: {private_kib} kB\n")

    def sampler(self, **options):
        sampler = resources.ResourceSampler(42, gpu=False, clock=lambda: self.now,
                                            proc_root=self.proc, **options)
        sampler.ticks = 100
        sampler.page_bytes = 4096
        # Fixture ticks are independent of the host's sysconf value.
        sampler.previous = 0.0, 1.0
        return sampler

    def test_cpu_delta_and_process_memory_are_measured_separately(self):
        sampler = self.sampler()
        self.now = 0.25
        self.write_stat(150)
        sample = sampler.sample()
        self.assertEqual(sample["cpuCores"], 2.0)
        self.assertEqual(sample["cpuPercent"], 200.0)
        self.assertEqual(sample["rssMiB"], 1.0)
        self.assertEqual(sample["pssMiB"], 0.75)
        self.assertEqual(sample["privateMiB"], 0.5)
        self.assertIsNone(sample["gpuMiB"])

    def test_phase_split_apportions_cpu_intervals_and_preserves_distinct_memory_peaks(self):
        sampler = self.sampler()
        self.now = 0.2
        self.write_stat(120)
        sampler.sample()
        self.now = 0.4
        self.memory(4096, 3072, 2048)
        self.write_stat(160)
        sampler.sample()
        report = sampler.summary(ready_elapsed=0.3)
        self.assertEqual(report["startup"]["sampleCount"], 1)
        self.assertEqual(report["steady"]["sampleCount"], 1)
        self.assertEqual(report["startup"]["peakRssMiB"], 1.0)
        self.assertEqual(report["steady"]["peakRssMiB"], 4.0)
        self.assertAlmostEqual(report["startup"]["cpuObservedSeconds"], 0.4)
        self.assertAlmostEqual(report["steady"]["cpuObservedSeconds"], 0.2)
        self.assertAlmostEqual(report["overall"]["meanCpuCores"], 1.5)
        self.assertEqual(sampler.summary(ready_elapsed=0.4)["startup"]["sampleCount"], 1)

    def test_missing_smaps_does_not_invent_pss_or_private_memory(self):
        sampler = self.sampler()
        (self.process / "smaps_rollup").unlink()
        self.now = 0.2
        sample = sampler.sample()
        self.assertEqual(sample["rssMiB"], 1.0)
        self.assertIsNone(sample["pssMiB"])
        self.assertIsNone(sample["privateMiB"])
        self.assertFalse(sampler.summary()["steady"]["available"])

    def test_completed_boundary_excludes_shutdown_from_steady_statistics(self):
        sampler = self.sampler()
        for elapsed, ticks, memory in [(0.2, 120, 1024), (0.4, 160, 4096), (0.6, 170, 16384)]:
            self.now = elapsed
            self.write_stat(ticks)
            self.memory(memory, memory, memory)
            sampler.sample()
        report = sampler.summary(ready_elapsed=0.3, completed_elapsed=0.5)
        self.assertEqual(report["steady"]["sampleCount"], 1)
        self.assertEqual(report["steady"]["peakRssMiB"], 4.0)
        self.assertEqual(report["shutdown"]["peakRssMiB"], 16.0)
        self.assertAlmostEqual(report["steady"]["cpuObservedSeconds"], 0.25)
        self.assertAlmostEqual(report["steady"]["meanCpuPercent"], 125.0)
        self.assertAlmostEqual(report["steady"]["peakCpuPercent"], 200.0)
        self.assertEqual(len(report["samples"]), 3)
        with self.assertRaises(ValueError):
            sampler.summary(ready_elapsed=0.3, completed_elapsed=0.2)

    def test_pid_reuse_and_exit_do_not_pollute_resource_summary(self):
        sampler = self.sampler()
        self.now = 0.2
        sampler.sample()
        self.write_stat(10000, identity=100)
        self.now = 0.4
        self.assertEqual(sampler.sample()["status"], "pid-reused")
        (self.process / "stat").unlink()
        self.now = 0.6
        self.assertEqual(sampler.sample()["status"], "process-gone")
        self.assertEqual(sampler.summary()["availableSampleCount"], 1)

    def test_gpu_failure_is_an_unavailable_measurement(self):
        class UnavailableGpu:
            def sample(self, _):
                raise resources.ResourceError("not supported")
        sampler = self.sampler(gpu_reader=UnavailableGpu())
        self.now = 0.2
        sample = sampler.sample()
        self.assertEqual(sample["gpu"]["status"], "unavailable")
        self.assertIsNone(sampler.summary()["overall"]["peakGpuMiB"])


class GpuMeasurementTests(unittest.TestCase):
    def nvml(self, records):
        instance = resources._Nvml.__new__(resources._Nvml)
        instance.devices = [(0, None, "test GPU")]
        instance.query_name = "test-graphics-query"
        def query(_, count, output):
            count._obj.value = len(records)
            for index, (pid, memory) in enumerate(records):
                output[index].pid = pid
                output[index].usedGpuMemory = memory
            return 0
        instance.query = query
        return instance

    def test_nvml_reports_only_target_pid_allocation(self):
        nvml = self.nvml([(7, 500 * 1024 * 1024), (42, 16 * 1024 * 1024)])
        report = nvml.sample(42)
        self.assertEqual(report["memoryMiB"], 16)
        self.assertEqual(nvml.sample(99)["memoryMiB"], 0)
        self.assertFalse(nvml.sample(99)["devices"][0]["pidListed"])
        self.assertEqual(ctypes.sizeof(resources._NvmlProcessInfo), 24)

    def test_nvml_unavailable_memory_is_not_a_large_numeric_value(self):
        nvml = self.nvml([(42, resources._Nvml.MEMORY_UNAVAILABLE)])
        self.assertIsNone(nvml.sample(42)["memoryMiB"])

    def test_software_renderer_requires_explicit_override(self):
        output = "direct rendering: Yes\nOpenGL vendor string: Mesa\nOpenGL renderer string: llvmpipe (LLVM 18)\n"
        with patch.object(resources.shutil, "which", return_value="/usr/bin/glxinfo"), \
             patch.object(resources.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, output, "")):
            with self.assertRaisesRegex(resources.ResourceError, "Software renderer"):
                resources.gpu_preflight(":0")
            self.assertTrue(resources.gpu_preflight(":0", allow_software=True)["softwareRendering"])

    def test_real_renderer_is_preserved_and_missing_display_is_rejected(self):
        output = "direct rendering: Yes\nOpenGL vendor string: NVIDIA Corporation\nOpenGL renderer string: NVIDIA GeForce RTX 4060/PCIe/SSE2\n"
        with patch.object(resources.shutil, "which", return_value="/usr/bin/glxinfo"), \
             patch.object(resources.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, output, "")):
            info = resources.gpu_preflight(":0")
            self.assertTrue(info["accelerated"])
            self.assertIn("RTX 4060", info["renderer"])
        with patch.dict(resources.os.environ, {}, clear=True):
            with self.assertRaisesRegex(resources.ResourceError, "existing DISPLAY"):
                resources.gpu_preflight()


class GraphicalCliPreflightTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.project = Path(self.temp.name)
        self.player = self.project / "player"
        self.player.write_text("The mocked process launcher never executes this file.\n")
        self.player.chmod(0o755)

    def tearDown(self):
        self.temp.cleanup()

    def args(self, *flags):
        return cli.parser().parse_args(["--project", str(self.project), "run", "--player",
                                        str(self.player), "--frames", "3", *flags])

    def fake_run(self, command, *_args, **_kwargs):
        report = Path(command[command.index("--unity-cli-report") + 1])
        report.write_text(json.dumps({"frames": 3, "success": True,
                                     "graphicsDeviceName": "llvmpipe (LLVM)",
                                     "graphicsDeviceType": "OpenGLCore"}))
        return {"exitCode": 0, "log": "mocked"}

    def test_software_preflight_rejection_never_launches_player(self):
        with patch.dict(resources.os.environ, {"DISPLAY": ":0"}), \
             patch.object(resources, "gpu_preflight", side_effect=resources.ResourceError("Software renderer")), \
             patch.object(cli, "run_process") as launch:
            with self.assertRaisesRegex(cli.ToolError, "Software renderer"):
                cli.run(self.args())
            launch.assert_not_called()

    def test_player_software_fallback_is_rejected_despite_hardware_preflight(self):
        with patch.dict(resources.os.environ, {"DISPLAY": ":0"}), \
             patch.object(resources, "gpu_preflight", return_value={"renderer": "NVIDIA RTX", "accelerated": True}), \
             patch.object(cli, "run_process", side_effect=self.fake_run), \
             contextlib.redirect_stdout(io.StringIO()) as output:
            self.assertEqual(cli.run(self.args()), 1)
        self.assertIn("software GPU", json.loads(output.getvalue())["error"])

    def test_explicit_software_override_reaches_preflight_and_runtime(self):
        with patch.dict(resources.os.environ, {"DISPLAY": ":0"}), \
             patch.object(resources, "gpu_preflight", return_value={"renderer": "llvmpipe", "accelerated": False}) as preflight, \
             patch.object(cli, "run_process", side_effect=self.fake_run), \
             contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(cli.run(self.args("--allow-software")), 0)
            preflight.assert_called_once_with(allow_software=True)


if __name__ == "__main__":
    unittest.main()
