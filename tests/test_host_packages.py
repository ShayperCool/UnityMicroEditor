import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import host
from unity_play import ToolError


class HostPackageTests(unittest.TestCase):
    def test_native_upm_provenance_preserves_locked_git_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / 'resolver'
            package = source / 'Library/PackageCache/vendor@abcdef'
            package.mkdir(parents=True)
            (package / 'package.json').write_text('{"name":"vendor","version":"1.2.3"}')
            (source / 'Packages').mkdir()
            (source / 'Packages/packages-lock.json').write_text('{"dependencies":{"vendor":{"hash":"abcdef","source":"git"}}}')
            (source / 'Library/PackageManager').mkdir()
            package_id = 'vendor@https://example.invalid/vendor.git#abcdef'
            (source / 'Library/PackageManager/projectResolution.json').write_text(json.dumps({
                'outputs': {package_id: {'name': 'vendor', 'resolvedPath': str(package)}}}))
            destination = root / 'owned-packages'
            with patch('unity_play.require_idle'):
                host._seed_package_sources(source, destination)
            records = json.loads((destination / '.unity-play-package-resolution.json').read_text())['packages']
            self.assertEqual(records['vendor'], {'packageId': package_id, 'root': package.name, 'gitHash': 'abcdef'})
            self.assertEqual((destination / package.name / 'package.json').read_bytes(), (package / 'package.json').read_bytes())

    def test_cold_resolution_runs_only_isolated_project_and_keeps_file_dependencies(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / 'input'
            (source / 'Packages').mkdir(parents=True)
            (source / 'ProjectSettings').mkdir()
            (source / 'ProjectSettings/ProjectVersion.txt').write_text('m_EditorVersion: 6000.6.3f1\n')
            original = '{"dependencies":{"local":"file:../../local-package","remote":"1.0.0"}}'
            (source / 'Packages/manifest.json').write_text(original)
            isolated = root / 'owned-resolver'
            def resolve(command, cwd, log, timeout):
                self.assertEqual(cwd, isolated)
                self.assertEqual(command[command.index('-projectPath') + 1], str(isolated))
                manifest = json.loads((isolated / 'Packages/manifest.json').read_text())
                self.assertEqual(manifest['dependencies']['local'], 'file:' + str((source / 'Packages/../../local-package').resolve()))
                output = isolated / 'Library/PackageManager/projectResolution.json'
                output.parent.mkdir(parents=True)
                output.write_text('{}')
                return {'exitCode': 0}
            with patch('unity_play.require_idle'), patch('unity_play.run_process', side_effect=resolve):
                host._resolve_packages(source, root / 'Unity', isolated, 60)
            self.assertFalse((source / 'Library').exists())
            self.assertEqual((source / 'Packages/manifest.json').read_text(), original)

    def test_package_resolution_failure_reports_native_log(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / 'input'
            (source / 'Packages').mkdir(parents=True)
            (source / 'Packages/manifest.json').write_text('{"dependencies":{}}')
            (source / 'ProjectSettings').mkdir()
            (source / 'ProjectSettings/ProjectVersion.txt').write_text('m_EditorVersion: 6000.6.3f1\n')
            with patch('unity_play.require_idle'), patch('unity_play.run_process', return_value={'exitCode': 1}):
                with self.assertRaisesRegex(ToolError, 'package-resolve.log'):
                    host._resolve_packages(source, root / 'Unity', root / 'owned', 60)
