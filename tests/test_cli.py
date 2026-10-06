import argparse
import contextlib
import fcntl
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import subprocess
import time
import unittest
from unittest.mock import patch
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import unity_play as cli


class CliTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.project = self.root / "project with spaces"
        (self.project / "ProjectSettings").mkdir(parents=True)
        (self.project / "Assets").mkdir()
        (self.project / "ProjectSettings/ProjectVersion.txt").write_text("m_EditorVersion: 6000.6.3f1\n")

    def tearDown(self):
        self.temp.cleanup()

    def test_build_scene_selection_preserves_order_and_excludes_disabled(self):
        (self.project / "ProjectSettings/EditorBuildSettings.asset").write_text(
            "  - enabled: 1\n    path: Assets/First.unity\n    guid: a\n"
            "  - enabled: 0\n    path: Assets/Disabled.unity\n    guid: b\n"
            "  - enabled: 1\n    path: Assets/Second Scene.unity\n    guid: c\n")
        self.assertEqual(cli.enabled_scenes(self.project), ["Assets/First.unity", "Assets/Second Scene.unity"])

    def test_project_lock_is_detected_and_released(self):
        (self.project / "Temp").mkdir()
        with (self.project / "Temp/UnityLockfile").open("w") as handle:
            fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.assertTrue(cli.project_locked(self.project))
            with self.assertRaises(cli.ToolError):
                cli.require_idle(self.project)
        self.assertFalse(cli.project_locked(self.project))

    def test_install_preflights_all_files_before_writing(self):
        source = self.root / "bridge"
        source.mkdir()
        (source / "new.cs").write_text("new")
        (source / "existing.cs").write_text("generated")
        destination = self.project / "Assets/Dungeon/CLIPlay"
        destination.mkdir(parents=True)
        (destination / "existing.cs").write_text("manual edit")
        with patch.object(cli, "BRIDGE", source):
            with self.assertRaises(cli.ToolError):
                cli.install(argparse.Namespace(project=self.project))
        self.assertFalse((destination / "new.cs").exists())
        self.assertEqual((destination / "existing.cs").read_text(), "manual edit")

    def test_install_does_not_copy_dungeon_scene_dependencies_into_other_projects(self):
        source = self.root / "bridge"
        (source / "Runtime").mkdir(parents=True)
        (source / "Runtime/Observer.cs").write_text("observer")
        (source / "Benchmarks").mkdir()
        (source / "Benchmarks/Ship.unity").write_text("references to Dungeon prefabs")
        (source / "Benchmarks.meta").write_text("benchmark folder guid")
        with patch.object(cli, "BRIDGE", source), patch.object(cli, "emit"):
            self.assertEqual(cli.install(argparse.Namespace(project=self.project)), 0)
        destination = self.project / "Assets/Dungeon/CLIPlay"
        self.assertEqual((destination / "Runtime/Observer.cs").read_text(), "observer")
        self.assertFalse((destination / "Benchmarks").exists())
        self.assertFalse((destination / "Benchmarks.meta").exists())

    def test_build_sync_uses_final_cooked_dlls_and_preserves_unrelated_outputs(self):
        player = self.project / "Builds/CLIPlay/Game.x86_64"
        cooked = self.project / "Library/Bee/artifacts/LinuxPlayerBuildProgram/ManagedStripped"
        managed = player.parent / "Game_Data/Managed"
        cooked.mkdir(parents=True)
        managed.mkdir(parents=True)
        (cooked / "Game.dll").write_bytes(b"final postprocessed build")
        (cooked / "NotShipped.dll").write_bytes(b"unused assembly")
        (cooked / "Game.ref.dll").write_bytes(b"reference only")
        (managed / "Game.dll").write_bytes(b"previous offline refresh")
        (managed / "Plugin.dll").write_bytes(b"unrelated plugin")
        result = cli.sync_built_managed(self.project, player)
        self.assertEqual(result["updated"], ["Game.dll"])
        self.assertEqual((managed / "Game.dll").read_bytes(), b"final postprocessed build")
        self.assertEqual((Path(result["backup"]) / "Game.dll").read_bytes(), b"previous offline refresh")
        self.assertFalse((managed / "NotShipped.dll").exists())
        self.assertFalse((managed / "Game.ref.dll").exists())
        self.assertEqual((managed / "Plugin.dll").read_bytes(), b"unrelated plugin")
        self.assertEqual(cli.sync_built_managed(self.project, player)["updated"], [])

    def test_run_preserves_arguments_with_spaces_and_requires_real_report(self):
        player = self.root / "fake player"
        player.write_text("#!/usr/bin/env python3\nimport sys\nprint(repr(sys.argv))\n")
        player.chmod(0o755)
        args = cli.parser().parse_args(["--project", str(self.project), "run", "--player", str(player), "--headless", "--frames", "3"])
        with contextlib.redirect_stdout(io.StringIO()) as output:
            result = cli.run(args)
        summary = json.loads(output.getvalue())
        self.assertEqual(result, 1)
        self.assertEqual(summary["exitCode"], 1)
        self.assertIn("no report", summary["error"])
        self.assertIn(str(player), Path(summary["log"]).read_text())

    def test_malformed_runtime_report_is_a_tool_error(self):
        player = self.root / "malformed player"
        player.write_text("#!/usr/bin/env python3\nimport pathlib,sys\np=pathlib.Path(sys.argv[sys.argv.index('--unity-cli-report')+1])\np.write_text('null')\n")
        player.chmod(0o755)
        args = cli.parser().parse_args(["--project", str(self.project), "run", "--player", str(player), "--headless"])
        with self.assertRaisesRegex(cli.ToolError, "Malformed"):
            cli.run(args)

    def test_timeout_stops_launched_child(self):
        player = self.root / "blocking-player"
        player.write_text("#!/usr/bin/env python3\nimport time\ntime.sleep(30)\n")
        player.chmod(0o755)
        with self.assertRaisesRegex(cli.ToolError, "exceeded"):
            cli.run_process([str(player)], self.project, self.root / "timeout.log", 0.1)

    def test_cleanup_stops_descendant_even_after_leader_exits(self):
        descendant = self.root / "descendant.py"
        descendant.write_text("import signal,time\nsignal.signal(signal.SIGTERM, signal.SIG_IGN)\ntime.sleep(30)\n")
        leader = self.root / "leader.py"
        leader.write_text("import subprocess,sys,time\np=subprocess.Popen([sys.executable,sys.argv[1]])\nprint(p.pid,flush=True)\ntime.sleep(0.2)\n")
        log = self.root / "descendant.log"
        result = cli.run_process([sys.executable, str(leader), str(descendant)], self.project, log, 2)
        self.assertEqual(result["exitCode"], 0)
        child_pid = int(log.read_text().strip())
        for _ in range(20):
            status = Path(f"/proc/{child_pid}/status")
            if not status.exists() or "State:\tZ" in status.read_text():
                break
            time.sleep(0.05)
        else:
            self.fail("Launched descendant survived process group cleanup")

    def test_reject_nonpositive_or_nonfinite_limits(self):
        for value in ["0", "-1"]:
            with self.assertRaises(argparse.ArgumentTypeError):
                cli.positive_int(value)
        for value in ["0", "-1", "nan", "inf"]:
            with self.assertRaises(argparse.ArgumentTypeError):
                cli.positive_seconds(value)


if __name__ == "__main__":
    unittest.main()
