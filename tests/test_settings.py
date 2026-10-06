"""Temporary build settings never overwrite source changes."""
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from settings import preserve_player_settings
from unity_play import ToolError


ORIGINAL = b"""%YAML 1.1
PlayerSettings:
  runInBackground: 0
  m_BuildTargetGraphicsAPIs:
  - m_BuildTarget: WebGLSupport
    m_APIs: 1c0000000b000000i
    m_Automatic: 0
  otherSetting: 7
  scriptingBackend:
    Android: 1
  afterSetting: 3
"""
GRAPHICS = (b"  - m_BuildTarget: LinuxStandaloneSupport\n"
            b"    m_APIs: 11000000i\n    m_Automatic: 0\n")
AUTOMATIC_GRAPHICS = (b"  - m_BuildTarget: LinuxStandaloneSupport\n"
                      b"    m_APIs: 1100000015000000i\n    m_Automatic: 1\n")


def with_defaults(data):
    return data.replace(b"  otherSetting:", GRAPHICS + b"  otherSetting:").replace(
        b"  afterSetting:", b"    Standalone: 0\n  afterSetting:")


class PreserveSettingsTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.project = Path(self.temp.name)
        self.path = self.project / "ProjectSettings/ProjectSettings.asset"
        self.path.parent.mkdir()
        self.path.write_bytes(ORIGINAL)
        self.path.chmod(0o640)

    def tearDown(self):
        self.temp.cleanup()

    def test_known_insertions_restore_exact_source_bytes_and_permissions(self):
        with preserve_player_settings(self.project):
            self.path.write_bytes(with_defaults(ORIGINAL))
        self.assertEqual(self.path.read_bytes(), ORIGINAL)
        self.assertEqual(self.path.stat().st_mode & 0o777, 0o640)
        self.assertEqual(list(self.path.parent.glob(".unity-play-settings-*")), [])

    def test_implicit_linux_glcore_vulkan_automatic_defaults_restore_exact_bytes(self):
        with preserve_player_settings(self.project):
            self.path.write_bytes(with_defaults(ORIGINAL).replace(GRAPHICS, AUTOMATIC_GRAPHICS))
        self.assertEqual(self.path.read_bytes(), ORIGINAL)

    def test_graphics_values_must_match_an_observed_pair(self):
        for graphics in (
            GRAPHICS.replace(b"m_Automatic: 0", b"m_Automatic: 1"),
            AUTOMATIC_GRAPHICS.replace(b"m_Automatic: 1", b"m_Automatic: 0"),
        ):
            with self.subTest(graphics=graphics):
                changed = with_defaults(ORIGINAL).replace(GRAPHICS, graphics)
                with self.assertRaises(ToolError):
                    with preserve_player_settings(self.project):
                        self.path.write_bytes(changed)
                self.assertEqual(self.path.read_bytes(), changed)
                self.path.write_bytes(ORIGINAL)

    def test_no_change_does_not_write_or_suppress_failure(self):
        with patch("settings.os.replace") as replace:
            with preserve_player_settings(self.project):
                pass
            replace.assert_not_called()
        with self.assertRaisesRegex(RuntimeError, "export failed"):
            with preserve_player_settings(self.project):
                raise RuntimeError("export failed")
        self.assertEqual(self.path.read_bytes(), ORIGINAL)

    def test_errored_export_restores_defaults_and_preserves_original_exception(self):
        with self.assertRaisesRegex(RuntimeError, "export failed"):
            with preserve_player_settings(self.project):
                self.path.write_bytes(with_defaults(ORIGINAL))
                raise RuntimeError("export failed")
        self.assertEqual(self.path.read_bytes(), ORIGINAL)

    def test_manual_edit_alongside_insertions_is_left_untouched(self):
        manual = with_defaults(ORIGINAL).replace(b"otherSetting: 7", b"otherSetting: 8")
        with self.assertRaisesRegex(ToolError, "leaving the file untouched"):
            with preserve_player_settings(self.project):
                self.path.write_bytes(manual)
        self.assertEqual(self.path.read_bytes(), manual)

    def test_existing_entries_cannot_be_changed(self):
        before = with_defaults(ORIGINAL)
        self.path.write_bytes(before)
        changed = before.replace(b"Standalone: 0", b"Standalone: 1")
        with self.assertRaises(ToolError):
            with preserve_player_settings(self.project):
                self.path.write_bytes(changed)
        self.assertEqual(self.path.read_bytes(), changed)

    def test_different_default_values_or_extra_graphics_fields_are_refused(self):
        for data in (with_defaults(ORIGINAL).replace(b"m_APIs: 11000000i", b"m_APIs: 15000000i"),
                     with_defaults(ORIGINAL).replace(b"Standalone: 0", b"Standalone: 1"),
                     with_defaults(ORIGINAL).replace(GRAPHICS, GRAPHICS + b"    unexpected: 1\n")):
            self.path.write_bytes(ORIGINAL)
            with self.assertRaises(ToolError):
                with preserve_player_settings(self.project):
                    self.path.write_bytes(data)
            self.assertEqual(self.path.read_bytes(), data)

    def test_empty_collections_and_crlf_restore_original_form(self):
        original = (b"PlayerSettings:\n  m_BuildTargetGraphicsAPIs: []\n"
                    b"  scriptingBackend: {}\n  otherSetting: 7\n").replace(b"\n", b"\r\n")
        expanded = (b"PlayerSettings:\n  m_BuildTargetGraphicsAPIs:\n" + GRAPHICS
                    + b"  scriptingBackend:\n    Standalone: 0\n  otherSetting: 7\n").replace(b"\n", b"\r\n")
        self.path.write_bytes(original)
        with preserve_player_settings(self.project):
            self.path.write_bytes(expanded)
        self.assertEqual(self.path.read_bytes(), original)

    def test_missing_unchanged_file_is_allowed_but_created_or_deleted_file_is_preserved(self):
        self.path.unlink()
        with preserve_player_settings(self.project):
            pass
        with self.assertRaises(ToolError):
            with preserve_player_settings(self.project):
                self.path.write_bytes(ORIGINAL)
        self.assertEqual(self.path.read_bytes(), ORIGINAL)
        with self.assertRaises(ToolError):
            with preserve_player_settings(self.project):
                self.path.unlink()
        self.assertFalse(self.path.exists())


if __name__ == "__main__":
    unittest.main()
