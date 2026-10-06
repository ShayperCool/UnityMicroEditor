"""Small, dependency-free Linux Unity Player CLI. Python 3.10+."""
from __future__ import annotations

import argparse
import contextlib
import fcntl
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import time
import uuid

TOOL_ROOT = Path(__file__).resolve().parent
BRIDGE = TOOL_ROOT / "Packages/com.unitymicroeditor.runtime"
BUILD_METHOD = "Dungeon.CLIPlay.Editor.PlayerBuild.BuildFromCommandLine"


class ToolError(Exception):
    pass


def unity_version(project: Path) -> str:
    file = project / "ProjectSettings/ProjectVersion.txt"
    if not file.is_file():
        raise ToolError(f"Not a Unity project: {file} is missing")
    match = re.search(r"^m_EditorVersion:\s*(\S+)", file.read_text(), re.MULTILINE)
    if not match:
        raise ToolError(f"Cannot read Unity version from {file}")
    return match.group(1)


def editor_path(project: Path, explicit: str | None) -> Path:
    configured = explicit or os.environ.get("UNITY")
    if configured:
        found = shutil.which(configured)
        return Path(found or configured).expanduser().resolve()
    version = unity_version(project)
    candidates = [
        Path.home() / f"Unity/Hub/Editor/{version}/Editor/Unity",
        Path(f"/opt/unity/Editor/{version}/Editor/Unity"),
        Path(f"/opt/Unity/Hub/Editor/{version}/Editor/Unity"),
    ]
    return next((path for path in candidates if path.is_file()), candidates[0])


def editor_processes(project: Path) -> list[dict]:
    processes = []
    for proc in Path("/proc").glob("[0-9]*"):
        try:
            args = (proc / "cmdline").read_bytes().decode(errors="replace").split("\0")
            if not args or Path(args[0]).name != "Unity":
                continue
            for index, arg in enumerate(args[:-1]):
                if arg.lower() == "-projectpath" and Path(args[index + 1]).resolve() == project:
                    processes.append({"pid": int(proc.name), "batchmode": "-batchmode" in args})
                    break
        except (OSError, ValueError):
            continue
    return processes


def project_locked(project: Path) -> bool:
    lock = project / "Temp/UnityLockfile"
    try:
        with lock.open("rb") as handle:
            try:
                fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
                fcntl.flock(handle, fcntl.LOCK_UN)
                return False
            except BlockingIOError:
                return True
    except FileNotFoundError:
        return False


def require_idle(project: Path) -> None:
    processes = editor_processes(project)
    if processes or project_locked(project):
        raise ToolError(f"Project is open in Unity. Close its Editor before building: {processes or project}")


def default_player(project: Path) -> Path:
    return project / "Builds/CLIPlay/UnityPlay.x86_64"


def selected_player(args) -> Path:
    return Path(args.player).expanduser().resolve() if args.player else default_player(args.project)


def enabled_scenes(project: Path) -> list[str]:
    file = project / "ProjectSettings/EditorBuildSettings.asset"
    if not file.is_file():
        return []
    return re.findall(r"- enabled: 1\s+path: (.+)", file.read_text())


def write_json(path: Path, value: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, ensure_ascii=False) + "\n")
    temporary.replace(path)


def emit(value: dict, as_json: bool = True) -> None:
    if as_json:
        print(json.dumps(value, indent=2, ensure_ascii=False))
    else:
        for key, item in value.items():
            print(f"{key}: {json.dumps(item, ensure_ascii=False) if isinstance(item, (dict, list)) else item}")


def doctor(args) -> int:
    project, editor = args.project, editor_path(args.project, args.unity)
    data = editor.parent / "Data"
    support = data / "PlaybackEngines/LinuxStandaloneSupport"
    processes = editor_processes(project)
    info = {
        "project": str(project), "unityVersion": unity_version(project),
        "editor": str(editor), "editorAvailable": editor.is_file(),
        "linuxMonoSupport": (support / "Variations/linux64_player_development_mono/LinuxPlayer").is_file(),
        "dotnet": str(data / "DotNetSdk/dotnet"),
        "libraryExists": (project / "Library").is_dir(),
        "editorProcesses": processes, "projectLocked": project_locked(project),
        "bridgeInstalled": (BRIDGE / "Editor/PlayerBuild.cs").is_file(),
        "enabledScenes": enabled_scenes(project),
        "cachedPlayer": str(default_player(project)), "playerExists": default_player(project).is_file(),
        "runtimeNeedsEditor": False,
        "contentNeedsImportAndBuild": True,
    }
    emit(info, args.json)
    return 0


def modules(args) -> int:
    editor = editor_path(args.project, args.unity)
    data = editor.parent / "Data"
    variation = data / "PlaybackEngines/LinuxStandaloneSupport/Variations/linux64_player_development_mono"
    managed = data / "PlaybackEngines/LinuxStandaloneSupport/Variations/mono/Managed"
    if not managed.is_dir():
        managed = data / "Managed/UnityEngine"
    native = []
    for name in ("LinuxPlayer", "UnityPlayer.so"):
        file = variation / name
        if file.is_file():
            native.append({"name": name, "path": str(file), "bytes": file.stat().st_size})
    bindings = [{"name": file.name, "bytes": file.stat().st_size} for file in sorted(managed.glob("UnityEngine*.dll"))]
    exports = []
    if shutil.which("nm") and (variation / "UnityPlayer.so").is_file():
        result = subprocess.run(["nm", "-D", "--defined-only", str(variation / "UnityPlayer.so")], capture_output=True, text=True)
        if result.returncode == 0:
            exports = [line.split()[-1] for line in result.stdout.splitlines() if line.split()]
    emit({"nativeRuntime": native, "managedBindings": bindings, "nativeExports": exports,
          "note": "UnityEngine.*Module.dll contains managed code and native bindings. Physics/render/audio are not independent native engines."})
    return 0


def install(args) -> int:
    require_idle(args.project)
    if not BRIDGE.is_dir():
        raise ToolError(f"Bridge source is missing: {BRIDGE}")
    destination = args.project / "Assets/Dungeon/CLIPlay"
    files = [file for file in BRIDGE.rglob("*") if file.is_file()
             and file.relative_to(BRIDGE).parts[0] not in {"Benchmarks", "Benchmarks.meta"}]
    # Preflight every file before writing anything. Existing manual edits survive.
    for source in files:
        target = destination / source.relative_to(BRIDGE)
        if target.exists() and source.read_bytes() != target.read_bytes():
            raise ToolError(f"Refusing to overwrite existing bridge file: {target}")
    created = []
    for source in files:
        target = destination / source.relative_to(BRIDGE)
        if not target.exists():
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, target)
            created.append(str(target))
    source_meta = BRIDGE.with_suffix(".meta")
    target_meta = destination.with_suffix(".meta")
    if source_meta.exists() and not target_meta.exists():
        shutil.copyfile(source_meta, target_meta)
        created.append(str(target_meta))
    emit({"installed": str(destination), "created": created})
    return 0


_PROCESS_SCAN_SECONDS = 1.0
_PROCESS_STOP_GRACE_SECONDS = 5.0


def _process_state(pid: int) -> tuple[int, int, int, str] | None:
    """Read start ticks, parent PID, group ID and state without reading arguments."""
    try:
        text = Path(f"/proc/{pid}/stat").read_text()
        # The comm field may contain whitespace and parentheses.
        fields = text[text.rfind(")") + 2:].split()
        return int(fields[19]), int(fields[1]), int(fields[2]), fields[0]
    except (OSError, ValueError, IndexError):
        return None


def _scan_descendants(known: dict[int, int]) -> None:
    snapshot = {}
    for entry in Path("/proc").iterdir():
        if entry.name.isdigit():
            state = _process_state(int(entry.name))
            if state is not None:
                snapshot[int(entry.name)] = state

    children: dict[int, list[int]] = {}
    for pid, state in snapshot.items():
        children.setdefault(state[1], []).append(pid)
    pending = [pid for pid, started in known.items()
               if pid in snapshot and snapshot[pid][0] == started]
    seen = set(pending)
    while pending:
        for child in children.get(pending.pop(), []):
            if child in seen:
                continue
            seen.add(child)
            known[child] = snapshot[child][0]
            pending.append(child)


def _known_alive(pid: int, started: int) -> bool:
    state = _process_state(pid)
    return state is not None and state[0] == started and state[3] not in ("Z", "X")


def _signal_known(pid: int, started: int, sig: int) -> None:
    if not _known_alive(pid, started):
        return
    try:
        if hasattr(os, "pidfd_open") and hasattr(signal, "pidfd_send_signal"):
            # The second identity check excludes PID reuse before pidfd_open;
            # the descriptor excludes reuse between that check and signaling.
            descriptor = os.pidfd_open(pid)
            try:
                if _known_alive(pid, started):
                    signal.pidfd_send_signal(descriptor, sig)
            finally:
                os.close(descriptor)
        elif _known_alive(pid, started):
            os.kill(pid, sig)
    except (ProcessLookupError, PermissionError):
        pass


def stop_child(process: subprocess.Popen, known: dict[int, int] | None = None) -> None:
    known = {} if known is None else known
    if process.returncode is None and process.pid not in known:
        state = _process_state(process.pid)
        if state is not None:
            known[process.pid] = state[0]
    _scan_descendants(known)
    # The leader may already have exited, and Unity workers may have used setsid.
    with contextlib.suppress(ProcessLookupError):
        os.killpg(process.pid, signal.SIGTERM)
    for pid, started in known.items():
        _signal_known(pid, started, signal.SIGTERM)

    deadline = time.monotonic() + _PROCESS_STOP_GRACE_SECONDS
    next_scan = time.monotonic() + _PROCESS_SCAN_SECONDS
    while time.monotonic() < deadline:
        process.poll()
        if not any(_known_alive(pid, started) for pid, started in known.items()):
            break
        if time.monotonic() >= next_scan:
            _scan_descendants(known)
            for pid, started in known.items():
                _signal_known(pid, started, signal.SIGTERM)
            next_scan = time.monotonic() + _PROCESS_SCAN_SECONDS
        time.sleep(0.05)

    _scan_descendants(known)
    with contextlib.suppress(ProcessLookupError):
        os.killpg(process.pid, signal.SIGKILL)
    for pid, started in known.items():
        _signal_known(pid, started, signal.SIGKILL)
    process.wait()


def run_process(command: list[str], project: Path, log: Path, timeout: float | None, env: dict | None = None,
                *, ready_path: Path | None = None, measure: bool = False, gpu: bool = False, stop_when=None) -> dict:
    log.parent.mkdir(parents=True, exist_ok=True)
    started, peak_rss = time.monotonic(), 0
    started_utc_ms = time.time_ns() / 1_000_000
    ready = None
    sampler = None
    stopped_for_change = False
    with log.open("w") as output:
        process = subprocess.Popen(command, cwd=project, env=env, stdout=output, stderr=subprocess.STDOUT, start_new_session=True)
        watcher = getattr(stop_when, "__self__", None)
        if watcher is not None and hasattr(watcher, "on_process_started"):
            watcher.on_process_started(process.pid)
        if measure:
            from resources import ResourceSampler
            sampler = ResourceSampler(process.pid, gpu=gpu, started_at=started)
        state = _process_state(process.pid)
        known = {process.pid: state[0]} if state is not None else {}
        next_scan = 0.0
        try:
            while True:
                if time.monotonic() >= next_scan:
                    _scan_descendants(known)
                    next_scan = time.monotonic() + _PROCESS_SCAN_SECONDS
                if process.poll() is not None:
                    break
                if timeout is not None and time.monotonic() - started > timeout:
                    raise ToolError(f"Process exceeded {timeout:g}s. Log: {log}")
                if stop_when is not None and stop_when():
                    stopped_for_change = True
                    break
                try:
                    match = re.search(r"^VmRSS:\s+(\d+)", Path(f"/proc/{process.pid}/status").read_text(), re.MULTILINE)
                    if match:
                        peak_rss = max(peak_rss, int(match.group(1)))
                except OSError:
                    pass
                if sampler is not None:
                    sampler.sample()
                if ready is None and ready_path is not None and ready_path.is_file():
                    try:
                        candidate = json.loads(ready_path.read_text())
                        if isinstance(candidate, dict) and isinstance(candidate.get("readyUtcUnixMilliseconds"), (int, float)):
                            ready = candidate
                    except (OSError, ValueError):
                        pass
                time.sleep(0.1)
        except BaseException:
            stop_child(process, known)
            if sampler is not None:
                sampler.close()
            raise
        else:
            stop_child(process, known)
    result = {"exitCode": process.returncode, "wallSeconds": round(time.monotonic() - started, 3),
              "peakRssMiB": round(peak_rss / 1024, 1), "log": str(log), "pid": process.pid}
    if stopped_for_change:
        result["stoppedForSourceChange"] = True
    if ready is None and ready_path is not None and ready_path.is_file():
        try:
            ready = json.loads(ready_path.read_text())
        except (OSError, ValueError):
            pass
    ready_elapsed = None
    if isinstance(ready, dict) and isinstance(ready.get("readyUtcUnixMilliseconds"), (int, float)):
        ready_elapsed = (ready["readyUtcUnixMilliseconds"] - started_utc_ms) / 1000
        if ready_elapsed >= 0:
            result.update(startupToSceneReadySeconds=round(ready_elapsed, 4), ready=ready)
    if sampler is not None:
        completed_elapsed = None
        if ready_path is not None:
            final_path = Path(str(ready_path).removesuffix(".ready.json"))
            if final_path.is_file():
                try:
                    completed_ms = json.loads(final_path.read_text()).get("completedFramesUtcUnixMilliseconds", 0)
                    if completed_ms > started_utc_ms:
                        completed_elapsed = (completed_ms - started_utc_ms) / 1000
                except (OSError, ValueError, AttributeError, TypeError):
                    pass
        try:
            result["resources"] = sampler.summary(ready_elapsed=ready_elapsed, completed_elapsed=completed_elapsed)
        finally:
            sampler.close()
    return result


def build(args) -> int:
    require_idle(args.project)
    editor = editor_path(args.project, args.unity)
    if not editor.is_file():
        raise ToolError(f"Unity Editor not found: {editor}. Set --unity or UNITY.")
    bridge = BRIDGE / "Editor/PlayerBuild.cs"
    if not bridge.is_file():
        raise ToolError("Run 'unity-play install' for this project first.")
    scenes = args.scene or enabled_scenes(args.project)
    if not scenes and not args.smoke:
        raise ToolError("No enabled scenes. Provide --scene Assets/path/to/Scene.unity or --smoke.")
    for scene in scenes if not args.smoke else []:
        path = (args.project / scene).resolve()
        if not path.is_relative_to(args.project / "Assets") or not path.is_file() or path.suffix != ".unity":
            raise ToolError(f"Scene must be an existing .unity file inside project Assets: {scene}")
    player = selected_player(args)
    manifest = player.parent / "build.json"
    log = args.project / "Logs/unity-cli-build.log"
    values = {"UNITY_CLI_PLAY_OUTPUT": str(player), "UNITY_CLI_PLAY_SCENES": json.dumps(scenes),
              "UNITY_CLI_PLAY_BUILD_MANIFEST": str(manifest), "UNITY_CLI_PLAY_SMOKE": "1" if args.smoke else "0",
              "UNITY_CLI_PLAY_GRAPHICS": args.graphics_api}
    if args.asset_bundle_manifest:
        path = Path(args.asset_bundle_manifest).expanduser().resolve()
        if not path.is_file():
            raise ToolError(f"AssetBundle text manifest does not exist: {path}")
        values["UNITY_CLI_PLAY_ASSET_BUNDLE_MANIFEST"] = str(path)
    command = [str(editor), "-batchmode", "-quit", "-nographics", "-projectPath", str(args.project),
               "-buildTarget", "linux64", "-executeMethod", BUILD_METHOD, "-logFile", str(log)]
    if args.dry_run:
        emit({"command": command, "environment": values, "player": str(player)})
        return 0
    player.parent.mkdir(parents=True, exist_ok=True)
    manifest.unlink(missing_ok=True)
    from settings import preserve_player_settings
    with preserve_player_settings(args.project):
        result = run_process(command, args.project, args.project / "Logs/unity-cli-build-launch.log", args.timeout,
                             {**os.environ, **values})
    result.update({"player": str(player), "buildLog": str(log), "manifest": str(manifest)})
    if manifest.is_file():
        result["build"] = json.loads(manifest.read_text())
        if not result["build"].get("success"):
            result["exitCode"] = 1
    if result["exitCode"] == 0 and (not player.is_file() or not manifest.is_file()):
        result["exitCode"] = 1
        result["error"] = "Editor exited without writing both a Player and build manifest. Inspect the build log."
    if result["exitCode"] == 0:
        result["managedSync"] = sync_built_managed(args.project, player)
        # Capture only this build's exact compiler configuration; never use Editor rsp files.
        from recompile import capture
        result["codeRefresh"] = capture(args.project, editor, player)
    emit(result)
    return 0 if result["exitCode"] == 0 else 1


def sync_built_managed(project: Path, player: Path) -> dict:
    """Use final cooked Player DLLs, including ILPP/stripping, after an Editor build.

    Bee may leave a manually refreshed destination untouched if its compiler
    inputs are unchanged. Editor ScriptAssemblies and raw compiler outputs are
    never sources for this synchronization.
    """
    cooked = project / "Library/Bee/artifacts/LinuxPlayerBuildProgram/ManagedStripped"
    target = player.parent / f"{player.stem}_Data/Managed"
    if not cooked.is_dir() or not target.is_dir():
        return {"available": False, "updated": [], "note": "Final Linux Player assembly layout was not found; Unity's normal copy remains in use."}
    from recompile import digest, player_processes
    if player_processes(player):
        raise ToolError("Stop the target Player before synchronizing the completed build.")
    updated = []
    backup = player.parent / "code-backups" / ("build-sync-" + uuid.uuid4().hex[:12])
    for source in sorted(cooked.glob("*.dll")):
        destination = target / source.name
        if source.name.endswith(".ref.dll") or not destination.is_file() or digest(source) == digest(destination):
            continue
        backup.mkdir(parents=True, exist_ok=True)
        shutil.copy2(destination, backup / destination.name)
        temporary = destination.with_name(f".{destination.name}.{uuid.uuid4().hex}.tmp")
        try:
            shutil.copyfile(source, temporary)
            temporary.replace(destination)
        finally:
            temporary.unlink(missing_ok=True)
        updated.append(source.name)
        # Keep debug symbols paired with the implementation when Unity emits them.
        for suffix in (".pdb", ".dll.mdb"):
            symbols = source.with_suffix(suffix)
            if symbols.is_file():
                shutil.copy2(symbols, destination.with_suffix(suffix))
    return {"available": True, "source": str(cooked), "updated": updated,
            "backup": str(backup) if updated else None}


def run(args) -> int:
    player = selected_player(args)
    if not player.is_file() or not os.access(player, os.X_OK):
        raise ToolError(f"Player executable is missing: {player}. Run 'unity-play build' first.")
    if not args.headless and not os.environ.get("DISPLAY") and not os.environ.get("WAYLAND_DISPLAY"):
        raise ToolError("No graphical session. Use --headless, or run from a desktop session.")
    graphics = None
    if not args.headless:
        from resources import gpu_preflight, ResourceError
        try:
            graphics = gpu_preflight(allow_software=args.allow_software)
        except ResourceError as error:
            raise ToolError(str(error)) from error
    run_dir = args.project / "Temp/UnityPlay/runs" / uuid.uuid4().hex[:12]
    report = Path(args.report).expanduser().resolve() if args.report else run_dir / "runtime.json"
    if report.suffix != ".json" or report == player:
        raise ToolError("--report must point to a .json report file.")
    if args.interactive and args.screenshot:
        raise ToolError("--screenshot requires a finite frame budget.")
    if args.headless and args.screenshot:
        raise ToolError("--screenshot requires a graphical run.")
    command = [str(player), "-logFile", str(run_dir / "player.log")]
    if args.headless:
        command += ["-batchmode", "-nographics"]
    else:
        command += ["-force-glcore", "-screen-fullscreen", "0"]
        if args.width is not None:
            command += ["-screen-width", str(args.width)]
        if args.height is not None:
            command += ["-screen-height", str(args.height)]
    command += args.player_arg
    command += ["--unity-cli-report", str(report), "--unity-cli-frames", str(args.frames)]
    ready_path = Path(str(report) + ".ready.json")
    command += ["--unity-cli-ready", str(ready_path)]
    if args.interactive:
        command += ["--unity-cli-interactive"]
    if args.fps is not None:
        command += ["--unity-cli-fps", str(args.fps)]
    if args.no_vsync:
        command += ["--unity-cli-no-vsync"]
    if args.screenshot:
        command += ["--unity-cli-screenshot", str(Path(args.screenshot).expanduser().resolve())]
    if args.content:
        command += ["--unity-cli-content", str(Path(args.content).expanduser().resolve())]
    if getattr(args, "raw_manifest", None):
        command += ["--unity-cli-raw-manifest", str(Path(args.raw_manifest).expanduser().resolve())]
    if getattr(args, "raw_reload_path", None):
        command += ["--unity-cli-raw-reload", str(Path(args.raw_reload_path).expanduser().resolve())]
    if args.scene is not None:
        command += ["--unity-cli-scene", args.scene]
    if args.dry_run:
        emit({"command": command, "runtimeNeedsEditor": False})
        return 0
    run_dir.mkdir(parents=True, exist_ok=True)
    report.parent.mkdir(parents=True, exist_ok=True)
    report.unlink(missing_ok=True)
    ready_path.unlink(missing_ok=True)
    timeout = args.timeout if args.timeout is not None else None if args.interactive else 120
    result = run_process(command, args.project, run_dir / "launch.log", timeout,
                         ready_path=ready_path, measure=True, gpu=not args.headless,
                         stop_when=getattr(args, "source_watch", None))
    result.update({"player": str(player), "playerLog": str(run_dir / "player.log"), "runtimeNeedsEditor": False})
    if graphics is not None:
        result["graphicsPreflight"] = graphics
    if result.get("stoppedForSourceChange"):
        result["exitCode"] = 0
    elif not args.interactive or report.is_file():
        result["report"] = str(report)
        if not report.is_file():
            result.update(exitCode=1, error="Player produced no report. It may have exited early or lack the CLI bridge.")
        else:
            runtime = json.loads(report.read_text())
            if not isinstance(runtime, dict) or type(runtime.get("frames")) is not int or type(runtime.get("success")) is not bool:
                raise ToolError(f"Malformed CLI runtime report: {report}")
            result["runtime"] = runtime
            if not args.headless and (runtime.get("graphicsDeviceType") == "Null" or not runtime.get("graphicsDeviceName")):
                result.update(exitCode=1, error="Graphical Player did not initialize a GPU device.")
            if not args.headless and not args.allow_software and any(
                marker in str(runtime.get("graphicsDeviceName", "")).lower()
                for marker in ("llvmpipe", "softpipe", "software", "swrast", "swiftshader")):
                result.update(exitCode=1, error="Player selected a software GPU. Use the accelerated desktop DISPLAY.")
            if not runtime.get("success") or not args.interactive and runtime.get("frames", 0) < args.frames:
                result["exitCode"] = 1
    write_json(run_dir / "launch.json", result)
    emit(result)
    return 0 if result["exitCode"] == 0 else 1


def positive_int(value: str) -> int:
    result = int(value)
    if result < 1:
        raise argparse.ArgumentTypeError("must be positive")
    return result


def positive_seconds(value: str) -> float:
    result = float(value)
    if not 0 < result < float("inf"):
        raise argparse.ArgumentTypeError("must be finite and positive")
    return result


def frame_rate(value: str) -> int:
    result = int(value)
    if result != -1 and result < 1:
        raise argparse.ArgumentTypeError("must be -1 (uncapped) or positive")
    return result


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(description=__doc__)
    root.add_argument("--project", type=Path, default=TOOL_ROOT.parents[1], help="Unity project root")
    root.add_argument("--unity", help="Unity Editor binary, only used for build/module discovery")
    commands = root.add_subparsers(dest="command", required=True)
    check = commands.add_parser("doctor", help="inspect project, runtime, scenes, and running Editors")
    check.add_argument("--json", action="store_true")
    check.set_defaults(handler=doctor)
    commands.add_parser("modules", help="show native runtime exports and managed UnityEngine modules").set_defaults(handler=modules)
    commands.add_parser("install", help="copy the bridge into another project, preserving existing files").set_defaults(handler=install)
    prepare = commands.add_parser("build", help="import and build a cached Linux Mono Player using batchmode Editor")
    prepare.add_argument("--scene", action="append", help="scene asset path, repeat to include several scenes")
    prepare.add_argument("--smoke", action="store_true", help="build a temporary empty scene to test the engine")
    prepare.add_argument("--graphics-api", choices=["glcore", "project"], default="glcore", help="Player graphics APIs, default glcore to reduce first-build shader compilation")
    prepare.add_argument("--player", help="output executable")
    prepare.add_argument("--asset-bundle-manifest", help="Unity root .manifest used to preserve types/modules needed by external content")
    prepare.add_argument("--timeout", type=positive_seconds, default=1800)
    prepare.add_argument("--dry-run", action="store_true")
    prepare.set_defaults(handler=build)
    host = commands.add_parser("host-build", help="prepare a reusable Mono host in a separate tool project, once using Editor")
    host.add_argument("--player", help="universal host output executable")
    host.add_argument("--timeout", type=positive_seconds, default=1800)
    host.add_argument("--project-packages", action="store_true", help="include exact runtime libraries of the project packages in the reusable host")
    host.set_defaults(handler=host_build)
    bridge = commands.add_parser("host-refresh", help="compile updated tool runtime into a stopped host using Roslyn, without Editor")
    bridge.add_argument("--player", required=True)
    bridge.set_defaults(handler=host_refresh)
    raw_check = commands.add_parser("raw-validate", help="inspect raw scene dependencies and unsupported content without Editor")
    raw_check.add_argument("--scene", required=True, help="raw project-relative scene path")
    raw_check.add_argument("--report", type=Path, help="save diagnostics JSON outside Assets")
    raw_check.add_argument("--native-catalog", type=Path, help="exact catalog of Unity-imported assets")
    raw_check.set_defaults(handler=raw_validate)
    raw_build = commands.add_parser("raw-build", help="compile code and prepare raw scene for the reusable host, without Editor")
    raw_build.add_argument("--scene", required=True)
    raw_build.add_argument("--player", help="universal host executable")
    raw_build.add_argument("--raw-dir", type=Path, help="immutable raw content revisions directory")
    native_options(raw_build)
    raw_build.set_defaults(handler=raw_build_command)
    play = commands.add_parser("run", help="run the prepared Player without starting an Editor")
    play.add_argument("--player", help="existing Player executable")
    play.add_argument("--scene", help="built scene name or build index")
    play.add_argument("--headless", action="store_true")
    play.add_argument("--allow-software", action="store_true", help="explicitly allow a software renderer, otherwise graphical runs require GPU acceleration")
    play.add_argument("--width", type=positive_int, help="window width")
    play.add_argument("--height", type=positive_int, help="window height")
    play.add_argument("--fps", type=frame_rate, help="observed run target framerate, -1 or positive")
    play.add_argument("--no-vsync", action="store_true", help="disable vsync in the observed run")
    play.add_argument("--screenshot", help="write a final rendered frame to an absolute PNG path")
    play.add_argument("--content", help="load a scene from an exported content/current.json manifest")
    play.add_argument("--interactive", action="store_true", help="run until the user or game closes the Player")
    play.add_argument("--frames", type=positive_int, default=120)
    play.add_argument("--report", help="runtime JSON destination")
    play.add_argument("--player-arg", action="append", default=[], help="extra native Player flag, e.g. --player-arg=-force-glcore")
    play.add_argument("--timeout", type=positive_seconds, help="seconds before stopping the Player, default 120 for finite runs")
    play.add_argument("--dry-run", action="store_true")
    play.set_defaults(handler=run)
    raw_play = commands.add_parser("raw-run", help="compile a raw project and run its scene in the universal host without Editor")
    raw_play.add_argument("--scene", required=True, help="raw .unity source asset path")
    raw_play.add_argument("--player", help="universal host executable")
    raw_play.add_argument("--raw-dir", type=Path)
    raw_play.add_argument("--headless", action="store_true")
    raw_play.add_argument("--allow-software", action="store_true")
    raw_play.add_argument("--width", type=positive_int, default=960)
    raw_play.add_argument("--height", type=positive_int, default=540)
    raw_play.add_argument("--fps", type=frame_rate, default=60)
    raw_play.add_argument("--no-vsync", action="store_true")
    raw_play.add_argument("--screenshot")
    raw_play.add_argument("--interactive", action="store_true")
    raw_play.add_argument("--watch", action="store_true", help="watch source edits, reload compatible content in the Player and restart for code/native changes; implies interactive")
    raw_play.add_argument("--reload-request", dest="raw_reload_path", type=Path,
                          help="scene reload request JSON path; watch creates a private path by default")
    raw_play.add_argument("--frames", type=positive_int, default=120)
    raw_play.add_argument("--report")
    raw_play.add_argument("--player-arg", action="append", default=[])
    raw_play.add_argument("--timeout", type=positive_seconds)
    raw_play.add_argument("--dry-run", action="store_true")
    native_options(raw_play)
    raw_play.set_defaults(handler=raw_run, content=None)
    native = commands.add_parser("import-assets", help="import heavy assets in an isolated Unity project, keeping scenes in our loader")
    native.add_argument("--scene", action="append", required=True, help="source scene for dependency discovery only")
    native.add_argument("--native-cache", type=Path)
    native.add_argument("--timeout", type=positive_seconds, default=1800)
    native.set_defaults(handler=import_assets_command)
    recovery = commands.add_parser("import-recover", help="publish a completed interrupted native import without starting Editor")
    recovery.add_argument("--native-cache", type=Path)
    recovery.set_defaults(handler=import_recover_command)
    refresh = commands.add_parser("refresh-code", help="experimental offline Roslyn compilation into a stopped Mono Player")
    refresh.add_argument("--assembly", required=True, help="one assembly captured from the baseline build")
    refresh.add_argument("--player", help="prepared Player executable")
    refresh.add_argument("--dry-run", action="store_true")
    refresh.add_argument("--timeout", type=positive_seconds, default=120)
    refresh.set_defaults(handler=refresh_code)
    content = commands.add_parser("refresh-assets", help="incrementally cook scene content, preserving the cached Player")
    content.add_argument("--scene", action="append", required=True, help="scene asset path, repeat for multiple scenes")
    content.add_argument("--player", help="prepared Player executable")
    content.add_argument("--prepare", action="store_true", help="cook before a shell build, returning its type-preservation manifest; does not publish current.json")
    content.add_argument("--content-dir", type=Path, help="published immutable content directory")
    content.add_argument("--timeout", type=positive_seconds, default=1800)
    content.add_argument("--dry-run", action="store_true")
    content.set_defaults(handler=refresh_assets)
    return root


def native_options(command) -> None:
    group = command.add_mutually_exclusive_group()
    group.add_argument("--native-import", action="store_true", help="reuse native heavy-asset cache; refresh via isolated Unity importer when needed")
    group.add_argument("--native-catalog", type=Path, help="use an existing native asset catalog without starting Editor")
    command.add_argument("--native-cache", type=Path, help="native importer cache directory")


def native_arguments(args) -> dict:
    if not getattr(args, "native_import", False) and not getattr(args, "native_catalog", None):
        return {}
    return {"native_import": bool(getattr(args, "native_import", False)),
            "native_catalog": args.native_catalog.expanduser().resolve() if getattr(args, "native_catalog", None) else None,
            "native_cache": args.native_cache.expanduser().resolve() if getattr(args, "native_cache", None) else None}


def import_assets_command(args) -> int:
    from imported_assets import export_imported_assets
    emit(export_imported_assets(args.project, editor_path(args.project, args.unity), args.scene,
                               cache_dir=args.native_cache, timeout=args.timeout, seed_library=True))
    return 0


def import_recover_command(args) -> int:
    from imported_assets import recover_import
    cache = args.native_cache or args.project / "Temp/UnityPlay/native-import"
    emit(recover_import(cache))
    return 0


def universal_host(args) -> Path:
    profile = "CLIPlayHybridHost" if getattr(args, "native_import", False) or getattr(args, "native_catalog", None) else "CLIPlayHost"
    return Path(args.player).expanduser().resolve() if args.player else TOOL_ROOT.parents[1] / "Builds" / profile / "UnityPlayHost.x86_64"


def raw_directory(args) -> Path:
    import hashlib
    if args.raw_dir:
        return args.raw_dir.expanduser().resolve()
    key = hashlib.sha256(str(args.project).encode()).hexdigest()[:16]
    return TOOL_ROOT.parents[1] / "Temp/UnityPlay/raw" / key


def host_build(args) -> int:
    from host import build_host
    emit(build_host(args.project, editor_path(args.project, args.unity), universal_host(args), args.timeout, project_packages=args.project_packages))
    return 0


def host_refresh(args) -> int:
    from host import refresh_bridge
    emit(refresh_bridge(args.project, editor_path(args.project, args.unity), universal_host(args)))
    return 0


def raw_validate(args) -> int:
    from raw_scene import RawSceneError, validate_scene
    try:
        result = validate_scene(args.project, args.scene, native_catalog=args.native_catalog)
    except RawSceneError as error:
        raise ToolError(str(error)) from error
    if args.report:
        write_json(args.report.expanduser().resolve(), result)
    emit(result)
    return 0 if result["supported"] else 1


def raw_build_command(args) -> int:
    from raw import prepare_raw
    emit(prepare_raw(args.project, editor_path(args.project, args.unity), universal_host(args), args.scene, raw_directory(args), **native_arguments(args)))
    return 0


def raw_run(args) -> int:
    from raw import prepare_raw
    host = universal_host(args)
    if args.watch and args.screenshot:
        raise ToolError("--watch is interactive and cannot be combined with --screenshot.")
    if args.watch:
        args.interactive = True
    if args.dry_run:
        args.raw_manifest = str(raw_directory(args) / "<revision>/scene.json")
        args.player = str(host)
        args.scene = None
        return run(args)
    source_scene = args.scene
    editor = editor_path(args.project, args.unity)
    directory = raw_directory(args)
    session = None
    preparation_arguments = native_arguments(args)
    if args.watch:
        from raw_cache import RawPreparationSession
        session = RawPreparationSession()
        preparation_arguments["session_cache"] = session
        args.raw_reload_path = (getattr(args, "raw_reload_path", None)
                                or directory / "reload" / uuid.uuid4().hex / "request.json").expanduser().resolve()
    elif getattr(args, "raw_reload_path", None):
        args.raw_reload_path = args.raw_reload_path.expanduser().resolve()
    result = prepare_raw(args.project, editor, host, source_scene, directory, **preparation_arguments)
    while True:
        watcher = None
        if args.watch:
            from raw_watch import RawSourceWatch
            watcher = RawSourceWatch(args.project, editor, host, source_scene, directory, result,
                                     session_cache=session, reload_path=args.raw_reload_path)
            args.interactive = True
        args.raw_manifest = result["manifest"]
        args.player = result.get("player", str(host))
        args.scene = None
        args.source_watch = watcher.poll if watcher is not None else None
        if getattr(args, "raw_reload_path", None):
            args.raw_reload_path.parent.mkdir(parents=True, exist_ok=True)
            args.raw_reload_path.unlink(missing_ok=True)
            Path(str(args.raw_reload_path) + ".ack.json").unlink(missing_ok=True)
        with contextlib.redirect_stdout(__import__("io").StringIO()) as capture:
            status = run(args)
        launch = json.loads(capture.getvalue())
        launch["rawBuild"] = result
        if watcher is not None:
            launch["sceneReloads"] = watcher.reload_history
            launch["rawReloadRequestPath"] = str(watcher.reload_path)
        if "startupToSceneReadySeconds" in launch:
            launch["prepareToSceneReadySeconds"] = result["prepareSeconds"] + launch["startupToSceneReadySeconds"]
        emit(launch)
        if watcher is None or not launch.get("stoppedForSourceChange"):
            return status
        result = watcher.next_build


def refresh_code(args) -> int:
    from recompile import refresh
    emit(refresh(args.project, selected_player(args), args.assembly, dry_run=args.dry_run, timeout=args.timeout))
    return 0


def refresh_assets(args) -> int:
    from assets import export
    emit(export(args.project, editor_path(args.project, args.unity), selected_player(args), args.scene,
                dry_run=args.dry_run, timeout=args.timeout, prepare_only=args.prepare, content_dir=args.content_dir))
    return 0


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    args.project = args.project.expanduser().resolve()
    try:
        unity_version(args.project)
        return args.handler(args)
    except KeyboardInterrupt:
        print("Stopped.", file=sys.stderr)
        return 130
    except (ToolError, OSError, ValueError) as error:
        print(f"unity-play: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
