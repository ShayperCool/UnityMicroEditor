#!/usr/bin/env python3
"""Measure the bundled raw fixture using isolated source and immutable host files."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import statistics
import struct
import subprocess
import time
import uuid
import zlib


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--output", type=Path, default=Path("Logs/UnityPlay/raw-benchmark"))
    parser.add_argument("--player", type=Path)
    parser.add_argument("--unity", type=Path)
    args = parser.parse_args()
    if args.runs < 1:
        parser.error("--runs must be positive")
    tool = Path(__file__).resolve().parent
    repo = tool.parents[1]
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    project = repo / "Temp/UnityPlay/raw-benchmark" / uuid.uuid4().hex
    shutil.copytree(tool / "Samples/RawProject", project,
                    ignore=shutil.ignore_patterns("Temp", "Library", "Builds", "Logs"))
    player = (args.player or repo / "Builds/CLIPlayHost/UnityPlayHost.x86_64").resolve()
    host_files = [player, player.parent / "UnityPlayer.so", player.parent / "host.json"]
    host_files.extend(sorted((player.parent / f"{player.stem}_Data").rglob("*")))
    host_hashes = {str(path): hashlib.sha256(path.read_bytes()).hexdigest()
                   for path in host_files if path.is_file()}
    launches = []

    def launch(label, expected):
        command = [str(tool / "unity-play"), "--project", str(project)]
        if args.unity:
            command += ["--unity", str(args.unity)]
        command += ["raw-run", "--scene", "Assets/Scenes/RawSample.unity", "--player", str(player),
                    "--frames", "120", "--fps", "60", "--no-vsync", "--width", "960", "--height", "540",
                    "--report", str(output / f"{label}-runtime.json")]
        started = time.monotonic()
        result = subprocess.run(command, capture_output=True, text=True, timeout=90)
        (output / f"{label}-stderr.log").write_text(result.stderr)
        (output / f"{label}-launch.json").write_text(result.stdout)
        if result.returncode:
            raise RuntimeError(f"{label} failed: {result.stderr}; see {output}")
        record = json.loads(result.stdout)
        proof_match = re.search(r"\[unity-raw-proof\] (\{[^\n]+\})", Path(record["log"]).with_name("player.log").read_text())
        if proof_match is None:
            raise RuntimeError(f"{label}: no proof from the loaded script")
        proof = json.loads(proof_match.group(1))
        for key, value in expected.items():
            actual = proof[key]
            if isinstance(value, dict):
                for component, number in value.items():
                    if abs(actual[component] - number) > 1e-5:
                        raise RuntimeError(f"{label}: {key}.{component} = {actual[component]}, expected {number}")
            elif actual != value:
                raise RuntimeError(f"{label}: {key} = {actual}, expected {value}")
        steady = record["resources"]["steady"]
        launches.append({"label": label, "commandWallSeconds": time.monotonic() - started,
                         "prepareSeconds": record["rawBuild"]["prepareSeconds"],
                         "compileSeconds": record["rawBuild"]["compiler"]["elapsedSeconds"],
                         "codeCacheHit": record["rawBuild"]["compiler"]["cacheHit"],
                         "playerReadySeconds": record["startupToSceneReadySeconds"],
                         "prepareToReadySeconds": record["prepareToSceneReadySeconds"],
                         "steady": steady, "proof": proof, "runtime": record.get("runtime")})
        print(f"{label}: {launches[-1]['prepareToReadySeconds']:.3f}s to ready", flush=True)

    launch("cold", {"codeRevision": 1, "dataRevision": 7})
    for index in range(args.runs):
        launch(f"warm-{index+1}", {"codeRevision": 1})
    code = project / "Assets/Scripts/RawIterationProbe.cs"
    code.write_text(code.read_text().replace("CodeRevision = 1", "CodeRevision = 2"))
    launch("code-change", {"codeRevision": 2})
    edits = {
        "Assets/Scenes/RawSample.unity": [("  revision: 1\n", "  revision: 2\n")],
        "Assets/Data/Tuning.asset": [("  revision: 7\n", "  revision: 8\n")],
        "Assets/Prefabs/Cube.prefab": [("    y: 1\n", "    y: 1.25\n")],
        "Assets/Materials/Checker.mat": [("      r: 0.2\n", "      r: 0.75\n")],
    }
    for relative, replacements in edits.items():
        path = project / relative
        content = path.read_text()
        for old, new in replacements:
            if old not in content:
                raise RuntimeError(f"Fixture edit no longer matches: {relative}")
            content = content.replace(old, new)
        path.write_text(content)
    # A one-pixel codec fixture, not an imported/cooked Unity texture.
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
    (project / "Assets/Textures/Probe.png").write_bytes(
        b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", 1, 1, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(b"\x00\xff\x00\x00\xff")) + chunk(b"IEND", b""))
    launch("asset-change", {"codeRevision": 2, "revision": 2, "dataRevision": 8,
           "materialColor": {"r": .75}, "prefabScale": {"x": 1.5, "y": 1.25},
           "texturePixel": {"r": 1, "g": 0, "b": 0}})
    if any(hashlib.sha256(Path(path).read_bytes()).hexdigest() != digest for path, digest in host_hashes.items()):
        raise RuntimeError("Host files changed during raw iteration")
    if (project / "Library").exists() or (project / "Builds").exists():
        raise RuntimeError("Raw source unexpectedly acquired Library or Builds")
    warm = launches[1:1+args.runs]
    summary = {"project": str(project), "host": str(player), "hostUnchanged": True,
               "inputLibraryExists": False, "inputBuildsExists": False, "launches": launches,
               "warmMedianSeconds": statistics.median(row["prepareToReadySeconds"] for row in warm)}
    (output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(f"Summary: {output / 'summary.json'}")


if __name__ == "__main__":
    main()
