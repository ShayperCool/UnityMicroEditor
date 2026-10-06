#!/usr/bin/env python3
"""Repeatable CLI Player measurements; raw evidence stays under Logs/UnityPlay."""
from __future__ import annotations

import argparse
import datetime
import json
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import time

ROOT = Path(__file__).resolve().parents[2]
CLI = ROOT / "Tools/UnityMicroEditor/unity-play"
SHIP = "Assets/Dungeon/CLIPlay/Benchmarks/ShipGenBenchmark.unity"


def distribution(values):
    values = [float(value) for value in values if value is not None]
    if not values:
        return None
    return {"n": len(values), "median": round(statistics.median(values), 4),
            "min": round(min(values), 4), "max": round(max(values), 4)}


def execute(arguments, destination, environment=None):
    started = time.monotonic()
    process = subprocess.run([str(CLI), *arguments], cwd=ROOT, text=True, capture_output=True,
                             env=None if environment is None else {**os.environ, **environment})
    (destination / "command.json").write_text(json.dumps(arguments, indent=2) + "\n")
    (destination / "stdout.json").write_text(process.stdout)
    (destination / "stderr.log").write_text(process.stderr)
    if process.returncode:
        raise RuntimeError(f"Command failed ({process.returncode}); evidence: {destination}")
    result = json.loads(process.stdout)
    result["cliWallSeconds"] = time.monotonic() - started
    if result.get("playerLog"):
        shutil.copyfile(result["playerLog"], destination / "player.log")
    (destination / "measurement.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


def run_arguments(args, destination):
    arguments = ["run", "--scene", args.scene, "--frames", str(args.frames),
                 "--fps", str(args.fps), "--no-vsync", "--report", str(destination / "runtime.json")]
    if args.headless:
        arguments.append("--headless")
    else:
        arguments += ["--width", str(args.width), "--height", str(args.height)]
    if args.content:
        arguments += ["--content", str(Path(args.content).resolve())]
    return arguments


def summarize(runs):
    result = {key: distribution([run.get(key) for run in runs])
              for key in ("startupToSceneReadySeconds", "wallSeconds", "cliWallSeconds", "peakRssMiB")}
    for metric in ("meanRssMiB", "meanPssMiB", "meanPrivateMiB", "meanCpuPercent", "meanGpuMiB", "peakGpuMiB"):
        result["steady_" + metric] = distribution([
            run.get("resources", {}).get("steady", {}).get(metric) for run in runs])
    result["observedFramesPerSecond"] = distribution([run.get("runtime", {}).get("observedFramesPerSecond") for run in runs])
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=["startup", "code", "assets"])
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--warmup", type=int, default=1)
    parser.add_argument("--frames", type=int, default=600)
    parser.add_argument("--fps", type=int, default=60)
    parser.add_argument("--scene", default=SHIP)
    parser.add_argument("--content")
    parser.add_argument("--headless", action="store_true")
    parser.add_argument("--width", type=int, default=960)
    parser.add_argument("--height", type=int, default=540)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.runs < 1 or args.warmup < 0 or args.frames < 1:
        parser.error("runs/frames must be positive and warmup nonnegative")
    directory = (args.output or ROOT / "Logs/UnityPlay/benchmarks" /
                 datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")).resolve()
    directory.mkdir(parents=True, exist_ok=False)
    report = {"dateUtc": datetime.datetime.now(datetime.timezone.utc).isoformat(), "mode": args.mode,
              "scene": args.scene, "frames": args.frames, "targetFps": args.fps, "resolution": [args.width, args.height],
              "headless": args.headless, "warmupRunsExcluded": args.warmup, "runs": []}

    if args.mode == "assets":
        return benchmark_assets(args, directory, report)

    source = ROOT / "Assets/Dungeon/ShipGen/Scripts/Gameplay/ShipBootstrap.cs"
    original = source.read_bytes() if args.mode == "code" else None
    anchor = "private void Start()\n        {"
    if original is not None and original.decode().count(anchor) != 1:
        raise RuntimeError("Gameplay benchmark anchor is absent/ambiguous; refusing to edit source.")
    try:
        for index in range(args.warmup + args.runs):
            destination = directory / f"run-{index:02d}"
            destination.mkdir()
            changed_at = None
            changed_utc_ms = None
            compiled = None
            marker = f"cli-code-benchmark-{index}"
            if original is not None:
                changed_at = time.monotonic()
                changed_utc_ms = time.time_ns() / 1_000_000
                modified = original.decode().replace(anchor, anchor + f'\n            Debug.Log("{marker}");')
                source.write_text(modified)
                compile_dir = destination / "compile"
                compile_dir.mkdir()
                compiled = execute(["refresh-code", "--assembly", "Dungeon.ShipGen"], compile_dir)
            run = execute(run_arguments(args, destination), destination)
            if original is not None:
                if marker not in (destination / "player.log").read_text():
                    raise RuntimeError("Refreshed gameplay code was not observed in the Player.")
                run["codeRefreshSeconds"] = compiled["cliWallSeconds"]
                run["editToSceneReadySeconds"] = (run["ready"]["readyUtcUnixMilliseconds"] - changed_utc_ms) / 1000
                # Actual edit-to-process-exit is also recorded, without confusing it with readiness.
                run["editToExitSeconds"] = time.monotonic() - changed_at
            if index >= args.warmup:
                report["runs"].append(run)
            print(f"run {index}: ready={run.get('startupToSceneReadySeconds')}s total={run['wallSeconds']}s", flush=True)
    finally:
        if original is not None:
            source.write_bytes(original)
            restore = directory / "restore"
            restore.mkdir(exist_ok=True)
            execute(["refresh-code", "--assembly", "Dungeon.ShipGen"], restore)
    report["summary"] = summarize(report["runs"])
    if args.mode == "code":
        for key in ("codeRefreshSeconds", "editToSceneReadySeconds", "editToExitSeconds"):
            report["summary"][key] = distribution([run[key] for run in report["runs"]])
    (directory / "summary.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report["summary"], indent=2))
    print(f"Evidence: {directory}")


def benchmark_assets(args, directory, report):
    import hashlib
    folder = ROOT / "Assets/Dungeon/CLIPlay/Benchmarks"
    original = {path: path.read_bytes() for path in folder.rglob("*") if path.is_file()}
    player_folder = ROOT / "Builds/CLIPlay"
    def hashes():
        player_paths = [player_folder / "UnityPlay.x86_64", player_folder / "UnityPlayer.so"]
        player_paths.extend(path for path in (player_folder / "UnityPlay_Data").rglob("*") if path.is_file())
        return {str(path.relative_to(player_folder)): hashlib.sha256(path.read_bytes()).hexdigest() for path in player_paths}
    baseline = hashes()
    (directory / "player-hashes-before.json").write_text(json.dumps(baseline, indent=2) + "\n")
    scene = "Assets/Dungeon/CLIPlay/Benchmarks/AssetIterationBenchmark.unity"
    content_dir = player_folder / "benchmark-content"
    export_arguments = ["refresh-assets", "--scene", scene, "--content-dir", str(content_dir)]
    args.scene = scene
    args.content = str(content_dir / "current.json")
    report["scene"] = scene
    try:
        for index in range(args.warmup + args.runs):
            revision = index + 2
            destination = directory / f"run-{index:02d}"
            destination.mkdir()
            export_dir = destination / "export"
            export_dir.mkdir()
            invoked_utc_ms = time.time_ns() / 1_000_000
            exported = execute(export_arguments, export_dir, {"UNITY_CLI_CONTENT_BENCHMARK_REVISION": str(revision)})
            editor_log = Path(exported["editorLog"]).read_text()
            shutil.copyfile(exported["editorLog"], export_dir / "editor.log")
            import re
            match = re.search(r'asset-edit-ready (\{[^\n]+\})', editor_log)
            if match is None:
                raise RuntimeError("Editor did not confirm the benchmark asset edit.")
            edited = json.loads(match.group(1))
            arguments = run_arguments(args, destination)
            if not args.headless:
                arguments += ["--screenshot", str(destination / "frame.png")]
            run = execute(arguments, destination)
            proof_line = re.search(r'asset-probe (\{[^\n]+\})', (destination / "player.log").read_text())
            if proof_line is None:
                raise RuntimeError("Player did not report the actual loaded test assets.")
            proof = json.loads(proof_line.group(1))
            expected_color = [0.9, 0.15, 0.1] if revision % 2 == 0 else [0.2, 0.9, 0.3]
            if proof["revision"] != revision or any(abs(proof["materialColor"][key] - expected) > 0.005
                for key, expected in zip(("r", "g", "b"), expected_color)):
                raise RuntimeError("Player loaded an outdated ScriptableObject/material.")
            pixel = [1, 0.75, 0.5] if revision % 2 == 0 else [0.5, 0.25, 1]
            background = [0.08, 0.04, 0.03] if revision % 2 == 0 else [0.03, 0.08, 0.04]
            if any(abs(proof["texturePixel"][key] - expected) > 0.01 for key, expected in zip(("r", "g", "b"), pixel)) or abs(proof["prefabScale"]["x"] - (1 + revision * 0.05)) > 0.005:
                raise RuntimeError("Player loaded an outdated texture/prefab.")
            if any(abs(proof["sceneBackground"][key] - expected) > 0.005 for key, expected in zip(("r", "g", "b"), background)):
                raise RuntimeError("Player loaded an outdated scene camera setting.")
            if hashes() != baseline:
                raise RuntimeError("Content-only export changed the Player binaries or built-in assets.")
            run.update(assetProof=proof, assetExportSeconds=exported["cliWallSeconds"],
                       editorExportSeconds=exported["editorExportSeconds"],
                       commandToSceneReadySeconds=(run["ready"]["readyUtcUnixMilliseconds"] - invoked_utc_ms) / 1000,
                       editToSceneReadySeconds=(run["ready"]["readyUtcUnixMilliseconds"] - edited["editUtcUnixMilliseconds"]) / 1000,
                       playerBytesUnchanged=True)
            if index >= args.warmup:
                report["runs"].append(run)
            print(f"asset revision {revision}: export={run['assetExportSeconds']:.3f}s edit-to-ready={run['editToSceneReadySeconds']:.3f}s", flush=True)
    finally:
        # Restore the exact benchmark assets saved before this experiment, then cook them.
        for path, value in original.items():
            path.write_bytes(value)
        restore = directory / "restore"
        restore.mkdir(exist_ok=True)
        execute(export_arguments, restore)
    report["summary"] = summarize(report["runs"])
    for key in ("assetExportSeconds", "editorExportSeconds", "commandToSceneReadySeconds", "editToSceneReadySeconds"):
        report["summary"][key] = distribution([run[key] for run in report["runs"]])
    (directory / "summary.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report["summary"], indent=2))
    print(f"Evidence: {directory}")


if __name__ == "__main__":
    main()
