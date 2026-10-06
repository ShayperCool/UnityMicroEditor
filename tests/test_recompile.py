"""Offline replay must fail closed and preserve the last working Player."""
from __future__ import annotations

import json

from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import recompile
from unity_play import ToolError


FAKE_COMPILER = '''#!/usr/bin/env python3
import pathlib, shlex, sys, time
args = shlex.split(pathlib.Path(sys.argv[2][1:]).read_text())
sources = [pathlib.Path(a) for a in args if a.endswith('.cs')]
content = b'\\n'.join(p.read_bytes() for p in sources)
if b'COMPILER_FAIL' in content:
    print('fixture compiler failure')
    sys.exit(7)
if b'COMPILER_SLOW' in content:
    time.sleep(2)
outputs = {a[1:].split(':', 1)[0]: pathlib.Path(a.split(':', 1)[1]) for a in args if a.startswith(('-out:', '-refout:', '-pdb:'))}
outputs['out'].write_bytes(b'DLL:' + content)
outputs['out'].with_suffix('.pdb').write_bytes(b'SYMBOLS:' + content)
if 'refout' in outputs:
    outputs['refout'].write_bytes(b'REFERENCE ONLY')
'''


class RecompileTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.project = Path(self.temp.name) / "project with spaces"
        self.project.mkdir()
        self.editor = Path(self.temp.name) / "Unity/Editor/Unity"
        self.editor.parent.mkdir(parents=True)
        self.editor.write_text("editor fixture")
        self.data = self.editor.parent / "Data"
        self.dotnet = self.data / "DotNetSdk/dotnet"
        self.dotnet.parent.mkdir(parents=True)
        self.dotnet.write_text(FAKE_COMPILER)
        self.dotnet.chmod(0o755)
        self.compiler = self.data / "DotNetSdk/sdk/8.0.1/Roslyn/bincore/csc.dll"
        self.compiler.parent.mkdir(parents=True)
        self.compiler.write_text("compiler fixture")
        self.reference = self.data / "Managed/UnityEngine.CoreModule.dll"
        self.reference.parent.mkdir(parents=True)
        self.reference.write_bytes(b"reference")
        self.source = self.project / "Assets/Simple/Example.cs"
        self.source.parent.mkdir(parents=True)
        self.source.write_text('class Example { string Text() => "before"; }')
        self.source.with_suffix(".cs.meta").write_text("guid: example")
        self.asmdef = self.source.parent / "Simple.asmdef"
        self.asmdef.write_text('{"name":"Simple"}')
        self.other = self.project / "Assets/Other.cs"
        self.other.write_text("class Other {}")
        self.player = self.project / "Builds/CLIPlay/Game.x86_64"
        self.player.parent.mkdir(parents=True)
        self.player.write_text("player fixture")
        self.managed = self.player.parent / "Game_Data/Managed"
        self.managed.mkdir(parents=True)
        self.target = self.managed / "Simple.dll"
        self.target.write_bytes(b"DLL:" + self.source.read_bytes())
        self.target.with_suffix(".pdb").write_bytes(b"initial symbols")
        self.artifacts = self.project / "Library/Bee/artifacts/LinuxPlayer.dag"
        self.artifacts.mkdir(parents=True)
        self.artifact = self.artifacts / "Simple.dll"
        self.artifact.write_bytes(self.target.read_bytes())
        self.artifact.with_name("Simple.ref.dll").write_bytes(b"baseline reference only")
        self.rsp = self.artifacts / "Simple.rsp"
        self.rsp.write_text(self.response())

    def tearDown(self):
        self.temp.cleanup()

    def response(self, *, defines="UNITY_STANDALONE_LINUX;ENABLE_MONO", extra=""):
        return (
            '-target:library\n-out:"Library/Bee/artifacts/LinuxPlayer.dag/Simple.dll"\n'
            '-refout:"Library/Bee/artifacts/LinuxPlayer.dag/Simple.ref.dll"\n'
            f'-define:{defines}\n-r:"{self.reference}"\n'
            '"Assets/Simple/Example.cs"\n-langversion:9.0\n/deterministic\n/debug:portable\n/nologo\n'
            + extra
        )

    def capture(self):
        return recompile.capture(self.project, self.editor, self.player)

    def edit(self, suffix="after"):
        self.source.write_text(f'class Example {{ string Text() => "{suffix}"; }}')

    def test_replay_updates_only_implementation_and_keeps_backup_and_bee_untouched(self):
        info = self.capture()
        self.assertEqual(info["assemblies"], ["Simple"])
        initial_dll, initial_rsp = self.target.read_bytes(), self.rsp.read_bytes()
        initial_ref = self.artifact.with_name("Simple.ref.dll").read_bytes()
        self.edit()
        result = recompile.refresh(self.project, self.player, "Simple")
        self.assertTrue(result["updated"])
        self.assertEqual(self.target.read_bytes(), b"DLL:" + self.source.read_bytes())
        self.assertEqual((Path(result["backup"]) / "Simple.dll").read_bytes(), initial_dll)
        self.assertEqual(self.artifact.read_bytes(), initial_dll)
        self.assertEqual(self.rsp.read_bytes(), initial_rsp)
        self.assertEqual(self.artifact.with_name("Simple.ref.dll").read_bytes(), initial_ref)
        self.assertFalse((self.managed / "Simple.ref.dll").exists())
        self.assertNotIn(b"REFERENCE ONLY", self.target.read_bytes())
        self.assertEqual(self.target.with_suffix(".pdb").read_bytes(), b"SYMBOLS:" + self.source.read_bytes())
        # A second body edit reuses the original frozen invocation.
        self.edit("again")
        self.assertTrue(recompile.refresh(self.project, self.player, "Simple")["updated"])

    def test_editor_and_il2cpp_responses_never_become_linux_mono_baseline(self):
        for platform in ("UNITY_STANDALONE_LINUX;ENABLE_MONO;UNITY_EDITOR", "UNITY_WEBGL;ENABLE_IL2CPP",
                         "UNITY_STANDALONE_LINUX;ENABLE_MONO;ENABLE_IL2CPP"):
            with self.subTest(platform=platform):
                self.rsp.write_text(self.response(defines=platform))
                self.assertEqual(self.capture()["assemblies"], [])

    def test_stale_platform_response_is_ignored_even_with_same_implementation_bytes(self):
        stale = self.artifacts.parent / "WebGL.dag"
        stale.mkdir()
        (stale / "Simple.rsp").write_text(self.response(defines="UNITY_WEBGL;ENABLE_IL2CPP"))
        self.capture()
        captured = self.player.parent / "compiler/Simple.rsp"
        self.assertEqual(captured.read_text(), self.rsp.read_text())

    def test_il_postprocessed_output_is_not_eligible(self):
        self.target.write_bytes(b"transformed by ILPP")
        info = self.capture()
        self.assertEqual(info["assemblies"], [])
        self.assertIn("differs from shipped DLL", info["skipped"]["Simple"])

    def test_unrecognized_analyzer_and_nested_response_file_are_rejected(self):
        unknown = self.project / "Library/UntrustedGenerator.dll"
        unknown.write_bytes(b"custom source generator")
        self.rsp.write_text(self.response(extra=f'-analyzer:"{unknown}"\n'))
        self.assertIn("requires a Unity build", self.capture()["skipped"]["Simple"])
        self.rsp.write_text(self.response(extra="@other.rsp\n"))
        self.assertIn("Unsupported compiler argument", self.capture()["skipped"]["Simple"])

    def test_unrecognized_compiler_output_flag_does_not_escape_temp_directory(self):
        self.rsp.write_text(self.response(extra='-doc:"Assets/overwritten.xml"\n'))
        self.assertIn("not supported", self.capture()["skipped"]["Simple"])
        self.assertFalse((self.project / "Assets/overwritten.xml").exists())

    def test_multiple_arguments_per_line_cannot_bypass_output_redirection(self):
        self.rsp.write_text(self.response().replace('-target:library\n-out:', '-target:library -out:'))
        self.assertIn("one-argument-per-line", self.capture()["skipped"]["Simple"])

    def test_changed_dependency_and_assets_fail_before_compilation(self):
        self.capture()
        self.edit()
        original = self.target.read_bytes()
        self.reference.write_bytes(b"different reference")
        with self.assertRaisesRegex(ToolError, "Compiler reference/input"):
            recompile.refresh(self.project, self.player, "Simple")
        self.assertEqual(self.target.read_bytes(), original)
        self.reference.write_bytes(b"reference")
        self.asmdef.write_text('{"name":"Changed"}')
        with self.assertRaisesRegex(ToolError, "Assets/meta/assembly definitions/settings"):
            recompile.refresh(self.project, self.player, "Simple")
        self.assertFalse((self.project / "Temp").exists())

    def test_new_removed_and_unrelated_sources_require_baseline_rebuild(self):
        self.capture()
        self.edit()
        added = self.source.parent / "New.cs"
        added.write_text("class New {}")
        with self.assertRaisesRegex(ToolError, "sources added/removed"):
            recompile.refresh(self.project, self.player, "Simple")
        added.unlink()
        self.other.write_text("class ChangedOther {}")
        with self.assertRaisesRegex(ToolError, "outside selected assembly"):
            recompile.refresh(self.project, self.player, "Simple")
        self.other.write_text("class Other {}")
        self.source.unlink()
        with self.assertRaisesRegex(ToolError, "sources added/removed"):
            recompile.refresh(self.project, self.player, "Simple")

    def test_compiler_error_and_timeout_preserve_last_working_dll_and_manifest(self):
        self.capture()
        initial = self.target.read_bytes()
        manifest = self.player.parent / "code-refresh.json"
        initial_config = manifest.read_bytes()
        self.edit("COMPILER_FAIL")
        with self.assertRaisesRegex(ToolError, "exit code 7"):
            recompile.refresh(self.project, self.player, "Simple")
        self.assertEqual(self.target.read_bytes(), initial)
        self.assertEqual(manifest.read_bytes(), initial_config)
        self.edit("COMPILER_SLOW")
        with self.assertRaisesRegex(ToolError, "exceeded"):
            recompile.refresh(self.project, self.player, "Simple", timeout=0.05)
        self.assertEqual(self.target.read_bytes(), initial)
        self.assertEqual(manifest.read_bytes(), initial_config)

    def test_dry_run_has_no_filesystem_side_effects(self):
        self.capture()
        self.edit()
        initial = self.target.read_bytes()
        result = recompile.refresh(self.project, self.player, "Simple", dry_run=True)
        self.assertTrue(result["dryRun"])
        self.assertEqual(result["changedSources"], ["Assets/Simple/Example.cs"])
        self.assertEqual(self.target.read_bytes(), initial)
        self.assertFalse((self.project / "Temp").exists())

    def test_running_target_is_blocked_including_symlink_launch(self):
        self.capture()
        self.edit()
        shutil.copyfile(sys.executable, self.player)
        self.player.chmod(0o755)
        alias = self.player.parent / "alias"
        alias.symlink_to(self.player)
        process = subprocess.Popen([str(alias), "-c", "import time; time.sleep(30)"])
        try:
            self.assertIn(process.pid, recompile.player_processes(self.player))
            with self.assertRaisesRegex(ToolError, "Stop the target Player"):
                recompile.refresh(self.project, self.player, "Simple")
        finally:
            process.terminate()
            process.wait(timeout=5)

    def test_snapshot_and_player_tampering_require_new_baseline(self):
        self.capture()
        self.edit()
        snapshot = self.player.parent / "compiler/Simple.rsp"
        snapshot.write_text(snapshot.read_text() + "-define:DIFFERENT\n")
        with self.assertRaisesRegex(ToolError, "Captured response file"):
            recompile.refresh(self.project, self.player, "Simple")
        self.capture()
        self.target.write_bytes(b"someone else's assembly")
        with self.assertRaisesRegex(ToolError, "Player DLL"):
            recompile.refresh(self.project, self.player, "Simple")

    def test_reference_implementation_is_never_accepted_as_compiler_output(self):
        self.rsp.write_text(self.response().replace('-out:"Library/Bee/artifacts/LinuxPlayer.dag/Simple.dll"',
                                                 '-out:"Library/Bee/artifacts/LinuxPlayer.dag/Simple.ref.dll"'))
        self.assertIn("reference assembly", self.capture()["skipped"]["Simple"])

    def test_verified_cooked_asset_changes_allow_subsequent_offline_code_edits(self):
        tuning = self.project / "Assets/Tuning.asset"
        tuning.write_text("serialized setting 1")
        self.capture()
        tuning.write_text("serialized setting 2")
        content = {"success": True, "ready": True, "target": "StandaloneLinux64", "revision": "fixture",
                   "sourceDependencies": [{"path": "Assets/Tuning.asset", "sha256": recompile.digest(tuning)}]}
        accepted = recompile.accept_content(self.project, self.player, content)
        self.assertEqual(accepted["paths"], ["Assets/Tuning.asset"])
        self.edit()
        self.assertTrue(recompile.refresh(self.project, self.player, "Simple")["updated"])

    def test_content_does_not_advance_meta_settings_or_uncooked_asset_changes(self):
        tuning = self.project / "Assets/Tuning.asset"
        tuning.write_text("serialized setting 1")
        self.capture()
        manifest = self.player.parent / "code-refresh.json"
        initial = manifest.read_bytes()
        tuning.write_text("serialized setting 2")
        self.source.with_suffix(".cs.meta").write_text("guid: changed")
        content = {"success": True, "ready": True, "target": "StandaloneLinux64",
                   "sourceDependencies": [{"path": "Assets/Tuning.asset", "sha256": recompile.digest(tuning)},
                                          {"path": "Assets/Simple/Example.cs.meta", "sha256": recompile.digest(self.source.with_suffix('.cs.meta'))}]}
        with self.assertRaisesRegex(ToolError, "Uncooked assets/meta"):
            recompile.accept_content(self.project, self.player, content)
        self.assertEqual(manifest.read_bytes(), initial)

    def test_content_verifies_package_cache_but_advances_only_assets(self):
        tuning = self.project / "Assets/Tuning.asset"
        tuning.write_text("serialized setting 1")
        package = self.project / "Library/PackageCache/com.example.demo@123/Tuning.asset"
        package.parent.mkdir(parents=True)
        package.write_text("package setting")
        self.capture()
        tuning.write_text("serialized setting 2")
        content = {"success": True, "ready": True, "target": "StandaloneLinux64", "sourceDependencies": [
            {"path": "Assets/Tuning.asset", "sha256": recompile.digest(tuning)},
            {"path": "Packages/com.example.demo/Tuning.asset", "physicalPath": package.relative_to(self.project).as_posix(),
             "sha256": recompile.digest(package)}]}
        accepted = recompile.accept_content(self.project, self.player, content)
        self.assertEqual(accepted["paths"], ["Assets/Tuning.asset"])
        manifest = self.player.parent / "code-refresh.json"
        config = json.loads(manifest.read_text())
        self.assertNotIn("Packages/com.example.demo/Tuning.asset", config["assets"])
        self.assertNotIn(package.relative_to(self.project).as_posix(), config["assets"])
        before = manifest.read_bytes()
        package.write_text("changed package setting")
        with self.assertRaisesRegex(ToolError, "Cooked source dependency changed"):
            recompile.accept_content(self.project, self.player, content)
        self.assertEqual(manifest.read_bytes(), before)


if __name__ == "__main__":
    unittest.main()
