"""Restore only Unity's known serialization of previously absent build defaults."""
from __future__ import annotations

from contextlib import contextmanager
import os
from pathlib import Path
import re
import tempfile


def _error(message: str):
    # unity_play imports this module when dispatching a build.
    from unity_play import ToolError
    return ToolError(message)


def _section(data: bytes, name: bytes) -> tuple[int, int, bytes] | None:
    """Find one PlayerSettings field, including its indented/list children."""
    matches = list(re.finditer(rb"(?m)^  " + re.escape(name) + rb":[^\r\n]*(?:\r?\n|$)", data))
    if len(matches) != 1:
        return None
    start, end = matches[0].span()
    for line in data[end:].splitlines(keepends=True):
        stripped = line.lstrip(b" ")
        indent = len(line) - len(stripped)
        if stripped.strip() and (indent < 2 or (indent == 2 and not stripped.startswith(b"- "))):
            break
        end += len(line)
    return start, end, data[start:end]


def _without_inserted_defaults(original: bytes, current: bytes) -> bytes:
    for name, presence, insertion, empty in (
        (
            b"m_BuildTargetGraphicsAPIs",
            rb"(?m)^  - m_BuildTarget: LinuxStandaloneSupport(?:\r?\n|$)",
            rb"(?m)^  - m_BuildTarget: LinuxStandaloneSupport\r?\n"
            rb"(?:    m_APIs: 11000000i\r?\n    m_Automatic: 0|"
            rb"    m_APIs: 1100000015000000i\r?\n    m_Automatic: 1)(?:\r?\n|$)",
            b"[]",
        ),
        (
            b"scriptingBackend",
            rb"(?m)^    Standalone:",
            rb"(?m)^    Standalone: 0(?:\r?\n|$)",
            b"{}",
        ),
    ):
        old_section, new_section = _section(original, name), _section(current, name)
        if old_section is None or new_section is None:
            continue
        _, _, before = old_section
        if re.search(presence, before):
            continue  # Existing values must be restored byte-for-byte by the editor.
        start, end, after = new_section
        if len(re.findall(presence, after)) != 1:
            continue
        normalized, count = re.subn(insertion, b"", after)
        if count != 1:
            continue
        before_lines = before.splitlines(keepends=True)
        normalized_lines = normalized.splitlines(keepends=True)
        # Unity expands an empty YAML collection when adding its first entry.
        if (len(before_lines) == 1 and before_lines[0].rstrip().endswith(b": " + empty)
                and len(normalized_lines) == 1
                and normalized_lines[0].rstrip() == b"  " + name + b":"):
            normalized = before
        current = current[:start] + normalized + current[end:]
    return current


@contextmanager
def preserve_player_settings(project: Path):
    """Keep source settings unchanged; refuse to overwrite unrelated edits.

    Unity 6.6's getters return values for absent Linux graphics/Standalone Mono
    entries, but its setters serialize those values even when restoring them.
    Recognized graphics pairs are OpenGLCore with automatic selection disabled,
    and the implicit OpenGLCore/Vulkan defaults with automatic selection enabled.
    Only these exact inserted pairs and Standalone Mono are eligible for cleanup.
    """
    path = Path(project) / "ProjectSettings/ProjectSettings.asset"
    try:
        original = path.read_bytes()
    except FileNotFoundError:
        original = None
    try:
        yield
    finally:
        try:
            current = path.read_bytes()
        except FileNotFoundError:
            current = None
        if current != original:
            if original is None or current is None or _without_inserted_defaults(original, current) != original:
                raise _error(f"PlayerSettings changed outside the known temporary build defaults; "
                             f"leaving the file untouched: {path}")
            mode = path.stat().st_mode & 0o7777
            descriptor, temporary_name = tempfile.mkstemp(prefix=".unity-play-settings-", dir=path.parent)
            temporary = Path(temporary_name)
            try:
                with os.fdopen(descriptor, "wb") as handle:
                    handle.write(original)
                    handle.flush()
                    os.fsync(handle.fileno())
                temporary.chmod(mode)
                if path.read_bytes() != current:
                    raise _error(f"PlayerSettings changed during restoration; leaving the file untouched: {path}")
                os.replace(temporary, path)
            finally:
                temporary.unlink(missing_ok=True)
