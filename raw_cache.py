"""Process-local caches for a validated raw preparation session.

The cache never substitutes a timestamp for the first content verification.
Reuse requires the same device, inode, size, mtime and ctime, and preparation
checks the complete source inventory again before publishing a revision.
"""
from __future__ import annotations

import copy
from dataclasses import dataclass
import hashlib
import os
from pathlib import Path
import stat
import time


class RawCacheError(ValueError):
    pass


def _stamp(info):
    return (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns,
            info.st_ctime_ns, info.st_mode)


def file_identity(path: Path):
    """Include the link itself as well as its current target identity."""
    info = path.lstat()
    if stat.S_ISLNK(info.st_mode):
        return (_stamp(info), str(path.resolve()), _stamp(path.stat()))
    return (_stamp(info),)


def _absolute(path):
    return str(Path(os.path.abspath(path)))


@dataclass(frozen=True)
class _FileRecord:
    identity: tuple
    content: bytes
    digest: str


@dataclass(frozen=True)
class _YamlRecord:
    digest: str
    documents: list


@dataclass(frozen=True)
class _GuidRecord:
    by_guid: dict
    by_path: dict
    indexed_meta: dict
    diagnostics: list


class RawPreparationSession:
    """One explicitly retained session; it is not persisted or globally shared."""

    content_suffixes = {".unity", ".prefab", ".mat", ".asset", ".png", ".jpg", ".jpeg"}

    def __init__(self):
        self._files = {}
        self._yaml = {}
        self._guid_indexes = {}
        self._trusted = None
        self.stats = {}
        self._observed = {}
        self.begin_attempt()

    def begin_attempt(self):
        self.stats = {"fileHits": 0, "fileReads": 0, "bytesRead": 0,
                      "yamlHits": 0, "yamlParses": 0, "guidIndexHits": 0,
                      "inventorySeconds": 0.0}
        self._observed = {}

    def read(self, path: Path, *, maximum=None):
        path = Path(path)
        key = _absolute(path)
        before = file_identity(path)
        old = self._files.get(key)
        if isinstance(old, _FileRecord) and old.identity == before:
            content, digest = old.content, old.digest
            self.stats["fileHits"] += 1
        else:
            with path.open("rb") as stream:
                opened = _stamp(os.fstat(stream.fileno()))
                content = stream.read() if maximum is None else stream.read(maximum + 1)
                finished = _stamp(os.fstat(stream.fileno()))
            after = file_identity(path)
            target_stamp = after[-1] if len(after) > 1 else after[0]
            if before != after or opened != finished or finished != target_stamp:
                # The explicit form below avoids trusting a replacement inode or
                # a write to the already-open file while the path was restored.
                raise RawCacheError(f"Raw source changed while reading; retry: {path}")
            digest = hashlib.sha256(content).hexdigest()
            self._files[key] = _FileRecord(after, content, digest)
            self.stats["fileReads"] += 1
            self.stats["bytesRead"] += len(content)
        if maximum is not None and len(content) > maximum:
            raise RawCacheError(f"Text asset exceeds {maximum} bytes: {path}")
        self._observed.setdefault(key, before)
        return content, digest

    def documents(self, path, parser, text):
        key = _absolute(path)
        content, digest = self.read(path)
        try:
            current_text = content.decode("utf-8-sig")
        except UnicodeError as error:
            raise RawCacheError(f"Raw source changed to binary content before parsing; retry: {path}") from error
        if current_text != text:
            # The reader may have observed older bytes immediately before this
            # call. Never associate its old graph with the newer byte digest.
            raise RawCacheError(f"Raw source changed between reading and parsing; retry: {path}")
        cached = self._yaml.get(key)
        if isinstance(cached, _YamlRecord) and cached.digest == digest:
            self.stats["yamlHits"] += 1
            return copy.deepcopy(cached.documents)
        documents = parser(path, current_text)
        self._yaml[key] = _YamlRecord(digest, copy.deepcopy(documents))
        self.stats["yamlParses"] += 1
        return documents

    def assert_observed_stable(self):
        for name, expected in self._observed.items():
            try:
                current = file_identity(Path(name))
            except OSError as error:
                raise RawCacheError(f"Raw source disappeared during preparation; retry: {name}") from error
            if current != expected:
                raise RawCacheError(f"Raw source changed during preparation; retry: {name}")

    def assert_digest(self, path, expected):
        _, actual = self.read(path)
        if actual != expected:
            raise RawCacheError(f"Raw source changed during preparation; retry: {path}")

    def inventory(self, roots, *, files=(), exclude=()):
        started = time.monotonic()
        roots = sorted(set(_absolute(path) for path in roots))
        exclusions = {_absolute(path) for path in exclude}
        result = {}

        def excluded(name):
            # Called only for top-level roots. Recursive traversal never enters
            # an excluded directory, so each child needs one exact set lookup.
            while name:
                if name in exclusions:
                    return True
                name = name.rpartition(os.sep)[0]
            return False

        def scan(name):
            if name in exclusions:
                return
            try:
                link = os.lstat(name)
            except FileNotFoundError:
                result[name + os.sep] = None
                return
            if stat.S_ISLNK(link.st_mode):
                result[name] = file_identity(Path(name))
                if not Path(name).is_dir():
                    return
            elif not stat.S_ISDIR(link.st_mode):
                result[name] = (_stamp(link),)
                return
            # Root and directory identity catches directory replacements and
            # retargeted ancestor links. File creation/deletion is detected by
            # the complete listing; directory mtimes are deliberately omitted.
            result[name + os.sep] = (link.st_dev, link.st_ino, link.st_mode)
            with os.scandir(name) as entries:
                for entry in entries:
                    if entry.path in exclusions:
                        continue
                    info = entry.stat(follow_symlinks=False)
                    if stat.S_ISDIR(info.st_mode):
                        scan(entry.path)
                    elif stat.S_ISLNK(info.st_mode):
                        result[entry.path] = file_identity(Path(entry.path))
                    else:
                        result[entry.path] = (_stamp(info),)

        retained = []
        for name in roots:
            if excluded(name):
                continue
            if not any(name == parent or name.startswith(parent + os.sep) for parent in retained):
                scan(name)
                retained.append(name)
        for path in files:
            name = _absolute(path)
            if name not in result:
                try:
                    result[name] = file_identity(Path(name))
                except FileNotFoundError:
                    result[name] = None
        self.stats["inventorySeconds"] += time.monotonic() - started
        return result

    def index_key(self, roots, inventory):
        roots = tuple(_absolute(root) for root in roots)
        root_set = set(roots)
        containment = {}
        def inside(name):
            clean = name.rstrip(os.sep)
            if clean in root_set:
                return True
            folder = clean.rpartition(os.sep)[0]
            original = folder
            while folder:
                if folder in root_set:
                    containment[original] = True
                    return True
                if folder in containment:
                    containment[original] = containment[folder]
                    return containment[folder]
                folder = folder.rpartition(os.sep)[0]
            containment[original] = False
            return False
        relevant = []
        for name, identity in inventory.items():
            if not inside(name):
                continue
            if name.endswith(os.sep) or identity is not None and len(identity) > 1:
                # A replacement directory or retargeted link can change source
                # containment even when the metadata bytes remain identical.
                relevant.append((name, identity))
            if not name.endswith(".meta"):
                continue
            source = name[:-5]
            source_identity = inventory.get(source)
            if source_identity is not None and len(source_identity) == 1:
                # Ordinary file content and atomic replacement do not change
                # GUID-to-lexical-path mappings. Existence/type still matters.
                source_identity = (stat.S_IFMT(source_identity[0][-1]),)
            relevant.append((name, identity, source_identity))
        return roots, tuple(sorted(relevant))

    def cached_index(self, key):
        value = self._guid_indexes.get(key)
        if isinstance(value, _GuidRecord):
            self.stats["guidIndexHits"] += 1
            return value.by_guid, value.by_path, value.indexed_meta, value.diagnostics
        return None

    def store_index(self, key, by_guid, by_path, indexed_meta, diagnostics):
        self._guid_indexes.clear()
        self._guid_indexes[key] = _GuidRecord(dict(by_guid), dict(by_path), dict(indexed_meta), copy.deepcopy(diagnostics))

    def invalidate(self):
        self._trusted = None

    def cache_stats(self):
        return {**self.stats, "cachedFileCount": len(self._files),
                "cachedBytes": sum(len(record.content) for record in self._files.values()
                                   if isinstance(record, _FileRecord))}
