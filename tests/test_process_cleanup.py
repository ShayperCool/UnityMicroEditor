import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import unity_play as cli


class DetachedProcessCleanupTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.pid_file = self.root / "child.pid"
        self.ready_file = self.root / "child.ready"
        child = self.root / "child.py"
        child.write_text(
            "import pathlib,signal,sys,time\n"
            "signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
            "pathlib.Path(sys.argv[1]).write_text('ready')\n"
            "time.sleep(60)\n")
        self.leader = self.root / "leader.py"
        self.leader.write_text(
            "import pathlib,subprocess,sys,time\n"
            "p=subprocess.Popen([sys.executable,sys.argv[1],sys.argv[3]], start_new_session=True)\n"
            "pathlib.Path(sys.argv[2]).write_text(str(p.pid))\n"
            "while not pathlib.Path(sys.argv[3]).exists(): time.sleep(0.01)\n"
            "time.sleep(float(sys.argv[4]))\n")
        self.command = [sys.executable, str(self.leader), str(child),
                        str(self.pid_file), str(self.ready_file)]

    def tearDown(self):
        if self.pid_file.exists():
            pid = int(self.pid_file.read_text())
            state = cli._process_state(pid)
            if state is not None:
                cli._signal_known(pid, state[0], signal.SIGKILL)
        self.temp.cleanup()

    def assert_child_stopped(self):
        self.assertTrue(self.ready_file.exists(), "The detached child did not reach its TERM handler")
        pid = int(self.pid_file.read_text())
        for _ in range(40):
            state = cli._process_state(pid)
            if state is None or state[3] in ("Z", "X"):
                return
            time.sleep(0.025)
        self.fail("The detached child survived cleanup")

    def test_timeout_kills_detached_child_and_leaves_unrelated_process_alive(self):
        unrelated = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"],
                                     start_new_session=True)
        try:
            with patch.object(cli, "_PROCESS_STOP_GRACE_SECONDS", 0.2):
                with self.assertRaisesRegex(cli.ToolError, "exceeded"):
                    cli.run_process(self.command + ["60"], self.root, self.root / "timeout.log", 1.5)
            self.assert_child_stopped()
            self.assertIsNone(unrelated.poll(), "Cleanup signaled an unrelated process")
        finally:
            unrelated.kill()
            unrelated.wait()

    def test_normal_exit_kills_previously_tracked_detached_child(self):
        # Keep the leader alive through one periodic ancestry scan before it exits.
        with patch.object(cli, "_PROCESS_STOP_GRACE_SECONDS", 0.2):
            result = cli.run_process(self.command + ["1.3"], self.root, self.root / "normal.log", 5)
        self.assertEqual(result["exitCode"], 0)
        self.assert_child_stopped()

    def test_source_change_stops_owned_tree_and_preserves_unrelated_process(self):
        unrelated = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"],
                                     start_new_session=True)
        try:
            started = time.monotonic()
            with patch.object(cli, "_PROCESS_STOP_GRACE_SECONDS", 0.2):
                result = cli.run_process(self.command + ["60"], self.root, self.root / "change.log", 5,
                                         stop_when=lambda: time.monotonic() - started > 1.4)
            self.assertTrue(result["stoppedForSourceChange"])
            self.assert_child_stopped()
            self.assertIsNone(unrelated.poll())
        finally:
            unrelated.kill()
            unrelated.wait()

    def test_changed_pid_identity_is_not_signaled(self):
        unrelated = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"],
                                     start_new_session=True)
        try:
            state = cli._process_state(unrelated.pid)
            self.assertIsNotNone(state)
            cli._signal_known(unrelated.pid, state[0] + 1, signal.SIGKILL)
            self.assertIsNone(unrelated.poll())
        finally:
            unrelated.kill()
            unrelated.wait()


if __name__ == "__main__":
    unittest.main()
