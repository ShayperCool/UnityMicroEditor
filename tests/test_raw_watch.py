import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from raw_watch import RawSourceWatch
from unity_play import ToolError
import unity_play as cli


class RawWatchTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.project = self.root / "project"
        (self.project / "Assets").mkdir(parents=True)
        self.code = self.project / "Assets/Mover.cs"
        self.code.write_text("original code")
        self.scene = self.project / "Assets/Scene.unity"
        self.scene.write_text("original scene")
        self.output = self.root / "raw-output"
        self.output.mkdir()
        self.manifest = self.output / "original.json"
        self.manifest.write_text(json.dumps({"sourceDependencies": [
            {"path": "Assets/Scene.unity", "sha256": self.digest(self.scene)}]}))
        self.build = {"revision": "initial", "manifest": str(self.manifest),
                      "compiler": {"compilerInputs": {str(self.code): self.digest(self.code)}}}
        self.now = 100.0

    def tearDown(self):
        self.temporary.cleanup()

    @staticmethod
    def digest(path):
        return hashlib.sha256(path.read_bytes()).hexdigest()

    def watcher(self):
        return RawSourceWatch(self.project, self.root / "Unity", self.root / "Host.x86_64",
                              "Assets/Scene.unity", self.output, self.build)

    def poll(self, watcher):
        self.now += 1
        with patch("raw_watch.time.monotonic", return_value=self.now):
            with contextlib.redirect_stderr(io.StringIO()):
                return watcher.poll()

    def test_unchanged_inputs_do_not_prepare_or_request_restart(self):
        watcher = self.watcher()
        with patch("raw.prepare_raw") as prepare:
            self.assertFalse(self.poll(watcher))
        prepare.assert_not_called()
        self.assertIsNone(watcher.next_build)

    def test_failed_prepare_keeps_running_revision_and_retries_only_after_new_edit(self):
        watcher = self.watcher()
        self.code.write_text("broken edit")
        with patch("raw.prepare_raw", side_effect=ToolError("compile failed")) as prepare:
            self.assertFalse(self.poll(watcher))
            self.assertIsNone(watcher.next_build)
            self.assertEqual(self.build["revision"], "initial")
            self.assertFalse(self.poll(watcher))
            self.assertEqual(prepare.call_count, 1)
            self.code.write_text("second broken edit")
            self.assertFalse(self.poll(watcher))
            self.assertEqual(prepare.call_count, 2)

    def test_successful_prepare_requests_restart_once_and_retains_replacement(self):
        watcher = self.watcher()
        self.scene.write_text("updated scene")
        replacement = {"revision": "replacement"}
        with patch("raw.prepare_raw", return_value=replacement) as prepare:
            self.assertTrue(self.poll(watcher))
            self.assertIs(watcher.next_build, replacement)
            self.assertTrue(self.poll(watcher))
        prepare.assert_called_once_with(self.project, self.root / "Unity", self.root / "Host.x86_64",
                                        "Assets/Scene.unity", self.output)

    def test_external_compiler_input_edit_is_observed(self):
        sdk = self.root / "external-sdk/compiler.dll"
        sdk.parent.mkdir()
        sdk.write_bytes(b"original SDK")
        self.build["compiler"]["compilerInputs"][str(sdk)] = self.digest(sdk)
        watcher = self.watcher()
        sdk.write_bytes(b"updated SDK")
        with patch("raw.prepare_raw", return_value={"revision": "sdk-change"}) as prepare:
            self.assertTrue(self.poll(watcher))
        prepare.assert_called_once()

    def test_new_source_in_external_local_package_is_observed(self):
        package = self.root / "external-package"
        package.mkdir()
        descriptor = package / "package.json"
        descriptor.write_text('{"name":"example.package","version":"1.0.0"}')
        self.build["compiler"]["compilerInputs"][str(descriptor)] = self.digest(descriptor)
        watcher = self.watcher()
        (package / "NewScript.cs").write_text("new package source")
        with patch("raw.prepare_raw", return_value={"revision": "package-change"}):
            self.assertTrue(self.poll(watcher))

    def test_library_and_nested_output_changes_are_excluded(self):
        self.output = self.project / "ProjectSettings/raw-output"
        self.output.mkdir(parents=True)
        library = self.project / "Library"
        library.mkdir()
        database = library / "artifact.db"
        database.write_bytes(b"initial")
        self.build["compiler"]["compilerInputs"][str(database)] = self.digest(database)
        watcher = self.watcher()
        database.write_bytes(b"changed Unity database")
        (self.output / "scene.json").write_text("new revision")
        (self.output / "compiled.cs").write_text("tool output, not source")
        with patch("raw.prepare_raw") as prepare:
            self.assertFalse(self.poll(watcher))
        prepare.assert_not_called()

    def test_edit_between_prepare_and_watcher_construction_is_not_missed(self):
        self.code.write_text("edit before watcher construction")
        watcher = self.watcher()
        with patch("raw.prepare_raw", return_value={"revision": "missed-edit"}):
            self.assertTrue(self.poll(watcher))

    def test_new_script_between_prepare_and_watcher_construction_is_not_missed(self):
        (self.project / "Assets/New.cs").write_text("new source before watcher construction")
        watcher = self.watcher()
        with patch("raw.prepare_raw", return_value={"revision": "new-script"}):
            self.assertTrue(self.poll(watcher))

    def test_scene_edit_between_prepare_and_watcher_construction_is_not_missed(self):
        self.scene.write_text("scene edit before watcher construction")
        watcher = self.watcher()
        with patch("raw.prepare_raw", return_value={"revision": "scene-edit"}):
            self.assertTrue(self.poll(watcher))

    def test_same_size_content_change_with_restored_mtime_is_observed(self):
        watcher = self.watcher()
        previous = self.code.stat()
        self.code.write_text("changed  code")
        self.assertEqual(self.code.stat().st_size, previous.st_size)
        os.utime(self.code, ns=(previous.st_atime_ns, previous.st_mtime_ns))
        with patch("raw.prepare_raw", return_value={"revision": "timestamp-preserved"}):
            self.assertTrue(self.poll(watcher))

    def test_removed_source_retries_after_it_is_restored(self):
        watcher = self.watcher()
        self.code.unlink()
        with patch("raw.prepare_raw", side_effect=ToolError("source missing")) as prepare:
            self.assertFalse(self.poll(watcher))
            self.assertFalse(self.poll(watcher))
            self.assertEqual(prepare.call_count, 1)
        self.code.write_text("restored code")
        with patch("raw.prepare_raw", return_value={"revision": "restored"}):
            self.assertTrue(self.poll(watcher))

    def test_poll_is_throttled_during_rapid_repeated_calls(self):
        watcher = self.watcher()
        with patch("raw_watch.time.monotonic", return_value=100.0):
            self.assertFalse(watcher.poll())
            self.code.write_text("edit within throttle interval")
            with patch("raw.prepare_raw") as prepare:
                self.assertFalse(watcher.poll())
        prepare.assert_not_called()
        with patch("raw.prepare_raw", return_value={"revision": "after-throttle"}):
            self.assertTrue(self.poll(watcher))

    def test_cli_restarts_from_prepared_revision_and_preserves_original_scene_selection(self):
        args = SimpleNamespace(project=self.project, unity=None, scene="Assets/Scene.unity",
                               watch=True, screenshot=None, interactive=False, dry_run=False)
        prepared, launches = [], []
        def prepare(project, unity, host, scene, output, **kwargs):
            prepared.append(scene)
            if len(prepared) == 1:
                return self.build
            return {"revision": "replacement", "manifest": str(self.manifest),
                    "compiler": {"compilerInputs": {str(self.code): self.digest(self.code)}}}
        def run(run_args):
            launches.append(run_args.raw_manifest)
            self.assertTrue(run_args.interactive)
            self.assertIsNone(run_args.scene)
            if len(launches) == 1:
                self.code.write_text("changed while host runs")
                self.assertTrue(run_args.source_watch())
                cli.emit({"stoppedForSourceChange": True, "exitCode": 0})
            else:
                cli.emit({"exitCode": 0})
            return 0
        captured = io.StringIO()
        with patch("raw.prepare_raw", side_effect=prepare), patch.object(cli, "run", side_effect=run):
            with patch.object(cli, "editor_path", return_value=self.root / "Unity"):
                with patch.object(cli, "universal_host", return_value=self.root / "Host.x86_64"):
                    with patch.object(cli, "raw_directory", return_value=self.output):
                        with contextlib.redirect_stdout(captured), contextlib.redirect_stderr(io.StringIO()):
                            self.assertEqual(cli.raw_run(args), 0)
        self.assertEqual(prepared, ["Assets/Scene.unity", "Assets/Scene.unity"])
        self.assertEqual(len(launches), 2)
        # The outer CLI emits both run reports after parsing each inner capture.
        decoder = json.JSONDecoder()
        pending = captured.getvalue().lstrip()
        first, used = decoder.raw_decode(pending)
        second, _ = decoder.raw_decode(pending[used:].lstrip())
        self.assertEqual(first["rawBuild"]["revision"], "initial")
        self.assertEqual(second["rawBuild"]["revision"], "replacement")

    def live_watcher(self):
        watcher = RawSourceWatch(self.project, self.root / "Unity", self.root / "Host.x86_64",
                                 "Assets/Scene.unity", self.output, self.build,
                                 session_cache=object(), reload_path=self.output / "reload.json")
        watcher.on_process_started(1234)
        return watcher

    def replacement(self, *, compatible=True):
        manifest = self.output / "replacement.json"
        manifest.write_text(json.dumps({"revision": "replacement", "objects": []}))
        return {"revision": "replacement", "manifest": str(manifest), "reloadCompatible": compatible,
                "compiler": self.build["compiler"]}

    def acknowledge(self, watcher, replacement, *, success=True, pid=1234, request_id=None):
        request = json.loads(watcher.reload_path.read_text())
        value = {"schemaVersion": 1, "requestId": request_id or request["requestId"],
                 "manifest": replacement["manifest"], "revision": replacement["revision"],
                 "pid": pid, "success": success, "completedUtcUnixMilliseconds": request["requestedUtcUnixMilliseconds"] + 20}
        Path(str(watcher.reload_path) + ".ack.json").write_text(json.dumps(value))

    def test_content_edit_sends_immutable_manifest_without_stopping_player(self):
        watcher = self.live_watcher()
        replacement = self.replacement()
        self.scene.write_text("updated scene")
        with patch("raw.prepare_raw", return_value=replacement) as prepare:
            self.assertFalse(self.poll(watcher))
        self.assertIsNone(watcher.next_build)
        self.assertIs(watcher.build, self.build)
        request = json.loads(watcher.reload_path.read_text())
        self.assertEqual(request["manifestSha256"], self.digest(Path(replacement["manifest"])))
        self.assertEqual(prepare.call_args.kwargs["changed_paths"], {self.scene})
        self.assertIs(prepare.call_args.kwargs["previous_build"], self.build)
        self.acknowledge(watcher, replacement)
        self.assertFalse(self.poll(watcher))
        self.assertIs(watcher.build, replacement)
        self.assertIsNone(watcher.pending_reload)
        self.assertTrue(watcher.reload_history[0]["acknowledgement"]["success"])

    def test_stale_ack_and_ack_from_another_player_cannot_commit_revision(self):
        watcher = self.live_watcher()
        replacement = self.replacement()
        self.scene.write_text("updated scene")
        with patch("raw.prepare_raw", return_value=replacement):
            self.assertFalse(self.poll(watcher))
        self.acknowledge(watcher, replacement, request_id="previous-request")
        self.assertFalse(self.poll(watcher))
        self.assertIs(watcher.build, self.build)
        self.assertIsNotNone(watcher.pending_reload)
        self.acknowledge(watcher, replacement, pid=5678)
        self.assertFalse(self.poll(watcher))
        self.assertIs(watcher.build, self.build)
        self.acknowledge(watcher, replacement)
        self.assertFalse(self.poll(watcher))
        self.assertIs(watcher.build, replacement)

    def test_rejected_live_reload_keeps_previous_build_and_does_not_retry_unchanged_source(self):
        watcher = self.live_watcher()
        replacement = self.replacement()
        self.scene.write_text("updated scene")
        with patch("raw.prepare_raw", return_value=replacement) as prepare:
            self.assertFalse(self.poll(watcher))
            self.acknowledge(watcher, replacement, success=False)
            self.assertFalse(self.poll(watcher))
            self.assertFalse(self.poll(watcher))
        self.assertIs(watcher.build, self.build)
        self.assertIsNone(watcher.next_build)
        self.assertIsNone(watcher.pending_reload)
        self.assertEqual(prepare.call_count, 1)

    def test_incompatible_revision_still_restarts_player(self):
        watcher = self.live_watcher()
        replacement = self.replacement(compatible=False)
        self.code.write_text("updated code")
        with patch("raw.prepare_raw", return_value=replacement):
            self.assertTrue(self.poll(watcher))
        self.assertIs(watcher.next_build, replacement)
        self.assertIsNone(watcher.pending_reload)
        self.assertFalse(watcher.reload_path.exists())

    def test_missing_ack_has_bounded_timeout_without_claiming_success(self):
        watcher = self.live_watcher()
        replacement = self.replacement()
        self.scene.write_text("updated scene")
        with patch("raw.prepare_raw", return_value=replacement):
            self.assertFalse(self.poll(watcher))
        watcher.pending_reload["sentAt"] = 0
        self.assertTrue(self.poll(watcher))
        self.assertIs(watcher.build, self.build)
        self.assertIsNone(watcher.pending_reload)
        self.assertIs(watcher.next_build, replacement)
        self.assertFalse(watcher.reload_history[0]["acknowledgement"]["success"])

    def test_failed_activation_requests_restart_from_prepared_revision(self):
        watcher = self.live_watcher()
        replacement = self.replacement()
        self.scene.write_text("updated scene")
        with patch("raw.prepare_raw", return_value=replacement):
            self.assertFalse(self.poll(watcher))
        self.acknowledge(watcher, replacement, success=False)
        ack_path = Path(str(watcher.reload_path) + ".ack.json")
        ack = json.loads(ack_path.read_text())
        ack["restartRequired"] = True
        ack_path.write_text(json.dumps(ack))
        self.assertTrue(self.poll(watcher))
        self.assertIs(watcher.next_build, replacement)
        self.assertIs(watcher.build, self.build)

    def test_source_edit_while_reload_pending_is_prepared_after_ack(self):
        watcher = self.live_watcher()
        replacement = self.replacement()
        self.scene.write_text("first scene edit")
        with patch("raw.prepare_raw", return_value=replacement) as prepare:
            self.assertFalse(self.poll(watcher))
            self.scene.write_text("second scene edit")
            self.assertFalse(self.poll(watcher))
            self.assertEqual(prepare.call_count, 1)
            self.acknowledge(watcher, replacement)
            self.assertFalse(self.poll(watcher))
            self.assertEqual(prepare.call_count, 2)
            self.assertIs(prepare.call_args.kwargs["previous_build"], replacement)

    def test_snapshot_detects_retargeted_link_to_same_size_and_timestamp(self):
        first = self.root / "first.cs"
        second = self.root / "second.cs"
        first.write_text("first source")
        second.write_text("other source")
        stamp = first.stat()
        os.utime(second, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
        alias = self.project / "Assets/Alias.cs"
        alias.symlink_to(first)
        self.build["compiler"]["compilerInputs"][str(alias)] = self.digest(alias)
        watcher = self.watcher()
        alias.unlink()
        alias.symlink_to(second)
        with patch("raw.prepare_raw", return_value={"revision": "changed-alias"}) as prepare:
            self.assertTrue(self.poll(watcher))
        prepare.assert_called_once()

    def test_directory_timestamp_alone_does_not_reload_unchanged_files(self):
        watcher = self.watcher()
        assets = self.project / "Assets"
        previous = assets.stat()
        os.utime(assets, ns=(previous.st_atime_ns, previous.st_mtime_ns + 1_000_000))
        with patch("raw.prepare_raw") as prepare:
            self.assertFalse(self.poll(watcher))
        prepare.assert_not_called()


if __name__ == "__main__":
    unittest.main()
