"""Prepare changed raw content and reload it in the running host when compatible."""
from __future__ import annotations

from pathlib import Path
import hashlib
import json
import os
import sys
import time
import uuid


class RawSourceWatch:
    def __init__(self, project: Path, unity: Path, host: Path, scene: str, output: Path, build: dict,
                 *, session_cache=None, reload_path: Path | None = None, reload_timeout: float = 30.0):
        self.project, self.unity, self.host, self.scene, self.output = (Path(project).resolve(), Path(unity).resolve(),
                                                                    Path(host).resolve(), scene, Path(output).resolve())
        self._output_name = str(self.output)
        self._output_prefix = self._output_name + os.sep
        self._library_name = str(self.project / "Library")
        self._library_prefix = self._library_name + os.sep
        native = build.get("nativeImport", {})
        self.native_arguments = {}
        if native.get("enabled"):
            self.native_arguments = {"native_import": native.get("automatic", False),
                                     "native_catalog": None if native.get("automatic") else Path(native["catalog"]),
                                     "native_cache": Path(native["cacheDirectory"]) if native.get("cacheDirectory") else None}
        self.inputs = list(build["compiler"].get("compilerInputs", {}))
        self.build = build
        self.session_cache = session_cache
        self.reload_path = Path(reload_path).resolve() if reload_path is not None else None
        self.reload_timeout = reload_timeout
        self.pending_reload = None
        self.reload_history = []
        self.player_pid = None
        self.roots = [self.project / name for name in ("Assets", "Packages", "ProjectSettings")]
        self.roots.extend(Path(name).parent for name in self.inputs
                          if Path(name).name == "package.json" and not Path(name).is_relative_to(self.project))
        from raw_cache import RawPreparationSession
        self._snapshot_cache = RawPreparationSession()
        self._snapshot_inputs = tuple(Path(path) for path in self.inputs if not self.excluded(Path(path)))
        self.next_build = None
        self.next_poll = 0.0
        self.last_attempt = self.snapshot()
        # A source edit between initial preparation and constructing this watcher
        # must not become the baseline for an older running content revision.
        if not self.matches_build(build):
            self.last_attempt = None

    def excluded(self, path: Path, *, resolve: bool = True) -> bool:
        name = str(path.resolve()) if resolve else str(path)
        return (name == self._output_name or name.startswith(self._output_prefix)
                or name == self._library_name or name.startswith(self._library_prefix))

    def source_paths(self):
        for root in self.roots:
            if not root.is_dir() or self.excluded(root):
                continue
            for directory, subdirectories, filenames in os.walk(root):
                folder = Path(directory)
                subdirectories[:] = [name for name in subdirectories
                                     if not name.startswith(".") and not name.endswith("~")
                                     and not self.excluded(folder / name)]
                for name in filenames:
                    path = folder / name
                    # Directory symlinks are checked above. Ordinary filenames
                    # need no repeated walk of their whole absolute path.
                    if (not name.startswith(".") and not name.endswith("~")
                            and not self.excluded(path, resolve=path.is_symlink())):
                        yield path

    def matches_build(self, build: dict) -> bool:
        expected = dict(build["compiler"].get("compilerInputs", {}))
        manifest_path = build.get("manifest")
        if manifest_path:
            try:
                manifest = json.loads(Path(manifest_path).read_text())
                for dependency in manifest.get("sourceDependencies", []):
                    expected[str(Path(dependency.get("sourcePath", self.project / dependency["path"])))] = dependency["sha256"]
            except (OSError, ValueError, TypeError, KeyError):
                return False
        for name, digest in expected.items():
            path = Path(name)
            if self.excluded(path):
                continue
            try:
                if hashlib.sha256(path.read_bytes()).hexdigest() != digest:
                    return False
            except OSError:
                return False
        # The compiler inventories even excluded Editor code, so a new script or
        # assembly definition can also invalidate the just-prepared revision.
        for path in self.source_paths():
            if path.suffix in {".cs", ".asmdef", ".asmref", ".dll", ".rsp"} and str(path.resolve()) not in expected:
                return False
        return True

    def snapshot(self) -> tuple:
        # scandir reuses each directory entry's stat result. Keep a complete
        # listing and link/target identities without resolving every pathname
        # or constructing tens of thousands of Path.parents collections.
        self._snapshot_cache.begin_attempt()
        inventory = self._snapshot_cache.inventory(self.roots, files=self._snapshot_inputs,
                                                    exclude=(self.output, self.project / "Library"))
        return tuple(sorted(inventory.items()))

    def on_process_started(self, pid: int) -> None:
        self.player_pid = pid

    def _record_reload(self, pending: dict, acknowledgement: dict) -> None:
        event = {"schemaVersion": 1, "requestId": pending["request"]["requestId"],
                 "revision": pending["build"]["revision"],
                 "sourceChangedUtcUnixMilliseconds": pending["changedUtc"],
                 "prepareSeconds": pending["prepareSeconds"], "acknowledgement": acknowledgement}
        completed = acknowledgement.get("completedUtcUnixMilliseconds")
        if isinstance(completed, (int, float)):
            event["editObservedToReadySeconds"] = (completed - pending["changedUtc"]) / 1000
        self.reload_history.append(event)
        if self.reload_path is not None:
            history = Path(str(self.reload_path) + ".events.jsonl")
            with history.open("a") as stream:
                stream.write(json.dumps(event, ensure_ascii=False) + "\n")

    def _poll_reload(self) -> bool:
        pending = self.pending_reload
        if pending is None:
            return False
        acknowledgement = None
        try:
            value = json.loads(Path(str(self.reload_path) + ".ack.json").read_text())
            request = pending["request"]
            if (isinstance(value, dict) and value.get("schemaVersion") == 1
                    and value.get("requestId") == request["requestId"]
                    and value.get("manifest") == request["manifest"]
                    and value.get("revision") == pending["build"]["revision"]
                    and type(value.get("success")) is bool
                    and (not value["success"] or (value.get("errors", 0) == 0 and value.get("exceptions", 0) == 0))
                    and (self.player_pid is None or value.get("pid") == self.player_pid)):
                acknowledgement = value
        except (OSError, ValueError, TypeError):
            pass
        if acknowledgement is None:
            if time.monotonic() - pending["sentAt"] < self.reload_timeout:
                return True
            acknowledgement = {"success": False, "restartRequired": True,
                               "fatalError": "Player did not acknowledge scene reload before the timeout."}
        self._record_reload(pending, acknowledgement)
        if acknowledgement["success"]:
            self.build = pending["build"]
            print(f"raw-watch: reloaded revision {self.build['revision']} in Player {acknowledgement.get('pid')}",
                  file=sys.stderr, flush=True)
        else:
            print(f"raw-watch: reload rejected; {acknowledgement.get('fatalError', 'runtime verification failed')}",
                  file=sys.stderr, flush=True)
            if acknowledgement.get("restartRequired") is True:
                self.next_build = pending["build"]
                print("raw-watch: restarting Player after an unconfirmed scene update", file=sys.stderr, flush=True)
        self.pending_reload = None
        return False

    def _request_reload(self, replacement: dict, changed_utc: int, prepare_seconds: float) -> None:
        from unity_play import write_json

        manifest = Path(replacement["manifest"])
        request = {"schemaVersion": 1, "requestId": uuid.uuid4().hex, "manifest": str(manifest),
                   "revision": replacement["revision"],
                   "manifestSha256": hashlib.sha256(manifest.read_bytes()).hexdigest(),
                   "requestedUtcUnixMilliseconds": time.time_ns() // 1_000_000,
                   "sourceChangedUtcUnixMilliseconds": changed_utc}
        write_json(self.reload_path, request)
        self.pending_reload = {"request": request, "build": replacement, "changedUtc": changed_utc,
                               "prepareSeconds": prepare_seconds, "sentAt": time.monotonic()}
        print(f"raw-watch: prepared revision {replacement['revision']}; reloading active scene",
              file=sys.stderr, flush=True)

    def poll(self) -> bool:
        if self.next_build is not None:
            return True
        if self._poll_reload():
            return False
        if self.next_build is not None:
            return True
        if time.monotonic() < self.next_poll:
            return False
        self.next_poll = time.monotonic() + 0.3
        candidate = self.snapshot()
        if candidate == self.last_attempt:
            return False
        changed_utc = time.time_ns() // 1_000_000
        old_values = dict((entry[0], entry[1:]) for entry in (self.last_attempt or ()))
        new_values = dict((entry[0], entry[1:]) for entry in candidate)
        changed_paths = {Path(name) for name in old_values.keys() | new_values.keys()
                         if old_values.get(name) != new_values.get(name)}
        self.last_attempt = candidate
        from raw import prepare_raw
        from unity_play import ToolError
        started = time.monotonic()
        try:
            arguments = dict(self.native_arguments)
            if self.session_cache is not None:
                arguments.update(previous_build=self.build, changed_paths=changed_paths, session_cache=self.session_cache)
            replacement = prepare_raw(self.project, self.unity, self.host, self.scene, self.output, **arguments)
        except (ToolError, ValueError, OSError) as error:
            print(f"raw-watch: keeping active scene; {error}", file=sys.stderr, flush=True)
            return False
        if self.reload_path is not None and replacement.get("reloadCompatible") is True:
            self._request_reload(replacement, changed_utc, time.monotonic() - started)
            return False
        self.next_build = replacement
        print(f"raw-watch: prepared revision {replacement['revision']}; restarting host", file=sys.stderr, flush=True)
        return True
