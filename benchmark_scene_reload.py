#!/usr/bin/env python3
"""Measure raw scene edits in one persistent GPU Player using disposable sources.

Transform, added GameObject and authored ScriptableObject edits must all keep the
same Player PID and the exact loaded native AudioClip/Mesh/Material/Texture2D.
The metric includes watcher detection and preparation, through first-frame ack
and executed scene proof. Heavy native reimports and C# patching are not claimed.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import re
import signal
import statistics
import subprocess
import time
import uuid

import benchmark_hybrid as hybrid
from imported_assets import _dependency_signature, inspect_catalog, sha256, source_inventory
from raw_scene import parse_unity_yaml
from unity_play import ToolError, editor_path, stop_child, write_json

TOOL = Path(__file__).resolve().parent
REPO = TOOL.parents[1]
PROBE_COMPONENT = "900000000000002"
DATA_PATH = "Assets/Dungeon/UnityPlaySceneReloadData.asset"
DATA_SCRIPT = "Assets/Dungeon/Scripts/UnityPlaySceneReloadData.cs"
DATA_GUID = hashlib.md5(b"unity-play-scene-reload-data").hexdigest()
DATA_SCRIPT_GUID = hashlib.md5(b"unity-play-scene-reload-data-script").hexdigest()
NATIVE_IDS = ("nativeAudioEntityId", "nativeMeshEntityId", "nativeMaterialEntityId", "nativeTextureEntityId")


def atomic_text(path: Path, text: str) -> None:
    temporary = path.with_name(path.name + ".benchmark.tmp")
    temporary.write_text(text)
    temporary.replace(path)


def _document(text: str, class_id: int, file_id: str) -> re.Match:
    expression = re.compile(r"^--- !u!" + str(class_id) + " &" + re.escape(file_id)
                            + r"\n[^\n]+:\n.*?(?=^--- |\Z)", re.MULTILINE | re.DOTALL)
    matches = list(expression.finditer(text))
    if len(matches) != 1:
        raise ToolError(f"Benchmark requires one class {class_id} object {file_id}, found {len(matches)}.")
    return matches[0]


def _replace_field(body: str, field: str, value: str) -> str:
    updated, count = re.subn(r"^  " + re.escape(field) + r":.*$", "  " + field + ": " + value,
                             body, count=1, flags=re.MULTILINE)
    if count != 1:
        raise ToolError("Benchmark source field is absent: " + field)
    return updated


def _set_probe_fields(text: str, revision: int, added: list[dict]) -> str:
    match = _document(text, 114, PROBE_COMPONENT)
    body = _replace_field(match.group(0), "iterationRevision", str(revision))
    expression = re.compile(r"^  addedObjects:[^\n]*(?:\n  - [^\n]*)*", re.MULTILINE)
    replacement = "  addedObjects:" + ("\n" + "\n".join("  - {fileID: " + item["gameObjectId"] + "}" for item in added)
                                                if added else " []")
    body, count = expression.subn(lambda _: replacement, body, count=1)
    if count != 1:
        raise ToolError("Benchmark probe has no unique serialized object list.")
    return text[:match.start()] + body + text[match.end():]


def transform_edit(project: Path, transform_id: str, revision: int, added: list[dict]) -> dict:
    path = project / hybrid.SCENE
    text = path.read_text()
    match = _document(text, 4, transform_id)
    record = parse_unity_yaml(path, text=match.group(0))[0]
    position = dict(record["data"]["m_LocalPosition"])
    position["x"] = float(position["x"]) + .125
    value = "{x: " + str(position["x"]) + ", y: " + str(position["y"]) + ", z: " + str(position["z"]) + "}"
    text = text[:match.start()] + _replace_field(match.group(0), "m_LocalPosition", value) + text[match.end():]
    atomic_text(path, _set_probe_fields(text, revision, added))
    return position


def add_object_edit(project: Path, revision: int, added: list[dict]) -> dict:
    path = project / hybrid.SCENE
    text = path.read_text()
    index = len(added) + 1
    owner, transform = str(900000000100000 + index * 2), str(900000000100001 + index * 2)
    documents = parse_unity_yaml(path)
    if {owner, transform}.intersection(item["fileId"] for item in documents):
        raise ToolError("Added-object benchmark ID collides with an authored object.")
    roots = [item for item in documents if item["classId"] == 1660057539]
    if len(roots) != 1:
        raise ToolError("Added-object benchmark requires one authored SceneRoots record.")
    item = {"gameObjectId": owner, "transformId": transform, "name": "UnityPlay added object " + str(index)}
    payload = (f"--- !u!1 &{owner}\nGameObject:\n  serializedVersion: 6\n"
               f"  m_Component:\n  - component: {{fileID: {transform}}}\n"
               f"  m_Layer: 0\n  m_Name: {item['name']}\n  m_TagString: Untagged\n  m_IsActive: 1\n"
               f"--- !u!4 &{transform}\nTransform:\n  m_GameObject: {{fileID: {owner}}}\n"
               "  m_LocalPosition: {x: 1, y: 1, z: 1}\n  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}\n"
               "  m_LocalScale: {x: 1, y: 1, z: 1}\n  m_Children: []\n  m_Father: {fileID: 0}\n")
    marker = f"--- !u!1660057539 &{roots[0]['fileId']}\n"
    text = text.replace(marker, payload + marker, 1)
    match = _document(text, 1660057539, roots[0]["fileId"])
    root_body, count = re.subn(r"^  m_Roots:[ \t]*$", "  m_Roots:\n  - {fileID: " + transform + "}",
                               match.group(0), count=1, flags=re.MULTILINE)
    if count != 1:
        raise ToolError("Added-object benchmark root list is absent.")
    text = text[:match.start()] + root_body + text[match.end():]
    atomic_text(path, _set_probe_fields(text, revision, added + [item]))
    return item


def data_edit(project: Path, revision: int) -> None:
    path = project / DATA_PATH
    atomic_text(path, _replace_field(path.read_text(), "revision", str(revision)))


def prepare(source: Path, catalog: Path, player: Path, editor: Path, output: Path) -> dict:
    started = time.monotonic()
    prepared = hybrid.prepare(source, catalog, player, editor, output, seed_library=False)
    project = Path(prepared["project"])
    native = inspect_catalog(Path(prepared["seedCatalog"]))
    selected = {}
    for field, type_name in (("nativeMesh", "UnityEngine.Mesh"), ("nativeMaterial", "UnityEngine.Material"),
                             ("nativeTexture", "UnityEngine.Texture2D")):
        entries = sorted((item for item in native["assets"] if item.get("typeName") == type_name),
                         key=lambda item: (item["path"], item["fileId"]))
        if not entries:
            raise ToolError("Published hybrid catalog lacks a retained native fixture: " + type_name)
        selected[field] = entries[0]
    # These authored proof assets use the existing Dungeon.Runtime assembly.
    # They neither add a native importer dependency nor change its exact pack.
    atomic_text(project / hybrid.PROBE_PATH,
                (TOOL / "SceneReloadBenchmark/Runtime/UnityPlayHybridBenchmarkProbe.cs").read_text())
    atomic_text(project / DATA_SCRIPT, (TOOL / "SceneReloadBenchmark/Runtime/UnityPlaySceneReloadData.cs").read_text())
    atomic_text(project / (DATA_SCRIPT + ".meta"), "fileFormatVersion: 2\nguid: " + DATA_SCRIPT_GUID + "\n")
    atomic_text(project / DATA_PATH,
                "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!114 &11400000\nMonoBehaviour:\n"
                "  m_GameObject: {fileID: 0}\n  m_Enabled: 1\n  m_Name: UnityPlay reload tuning\n"
                "  m_Script: {fileID: 11500000, guid: " + DATA_SCRIPT_GUID + ", type: 3}\n  revision: 1\n")
    atomic_text(project / (DATA_PATH + ".meta"), "fileFormatVersion: 2\nguid: " + DATA_GUID
                + "\nNativeFormatImporter:\n  mainObjectFileID: 11400000\n")
    path = project / hybrid.SCENE
    text = path.read_text()
    match = _document(text, 114, PROBE_COMPONENT)
    fields = "  iterationRevision: 1\n  addedObjects: []\n  data: {fileID: 11400000, guid: " + DATA_GUID + ", type: 2}\n"
    for field, item in selected.items():
        fields += "  " + field + ": {fileID: " + item["fileId"] + ", guid: " + item["guid"] + ", type: 3}\n"
    text = text[:match.end()] + fields + text[match.end():]
    atomic_text(path, text)
    signature = _dependency_signature(project, [hybrid.SCENE], [], source_inventory(project), native.get("sourceRoots", []),
                                      {item["path"] for item in native["assets"]})
    if signature != native["discoverySignature"]:
        raise ToolError("Raw benchmark fixture changed native importer inputs; do not claim native retention.")
    prepared.update(benchmark="persistent-scene-reload", iterationRevision=1, dataRevision=1,
                    addedObjects=[], setupSeconds=time.monotonic() - started,
                    nativeRetainedFixtures=selected)
    write_json(output / "prepared.json", prepared)
    return prepared


def _wait_json(path: Path, predicate, process: subprocess.Popen, timeout: float) -> dict:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise ToolError("Persistent CLI exited while waiting for " + str(path))
        try:
            value = json.loads(path.read_text())
            if isinstance(value, dict) and predicate(value):
                return value
        except (OSError, ValueError):
            pass
        time.sleep(.02)
    raise ToolError("Persistent scene reload timed out waiting for " + str(path))


def _finite(value, name: str) -> float:
    if type(value) not in (int, float) or not math.isfinite(value):
        raise ToolError("Scene reload benchmark has an invalid numeric value: " + name)
    return float(value)


def validate_proof(proof: dict, baseline: dict | None, *, pid: int, revision: int,
                   data_revision: int, position: dict, added: list[dict]) -> None:
    if (proof.get("schemaVersion") != 1 or proof.get("success") is not True or proof.get("pid") != pid
            or proof.get("sceneName") != "SampleScene" or proof.get("iterationRevision") != revision
            or proof.get("dataRevision") != data_revision or proof.get("awakeReferencesRestored") is not True
            or proof.get("sceneReferencesRestored") is not True or proof.get("dungeonTargetAwakeObserved") is not True):
        raise ToolError("Scene reload did not execute the expected authored values and Awake references.")
    if proof.get("addedObjectNames") != [item["name"] for item in added]:
        raise ToolError("Scene reload did not create the exact authored GameObject references.")
    for axis, value in position.items():
        actual = _finite(proof.get("sceneTargetLocalPosition", {}).get(axis), "loaded position " + axis)
        if abs(actual - float(value)) > 1e-5:
            raise ToolError("Scene reload loaded a stale or incorrect source Transform.")
    if proof.get("graphicsDeviceType") != "OpenGLCore" or "RTX 4060" not in proof.get("graphicsDeviceName", ""):
        raise ToolError("Scene reload benchmark requires the accelerated RTX 4060 OpenGLCore device.")
    for name in NATIVE_IDS:
        if not isinstance(proof.get(name), str) or not proof[name].strip():
            raise ToolError("Native fixture is missing: " + name)
        if baseline is not None and proof[name] != baseline[name]:
            raise ToolError("Scene reload replaced an already loaded native asset: " + name)


def validate_ack(ack: dict, request: dict, *, pid: int, edit_utc_ms: float,
                 native_identity: str | None = None, native_object_count: int | None = None) -> None:
    if (ack.get("schemaVersion") != 1 or ack.get("success") is not True or ack.get("requestId") != request.get("requestId")
            or ack.get("manifest") != request.get("manifest") or ack.get("pid") != pid
            or ack.get("errors", 0) != 0 or ack.get("exceptions", 0) != 0 or ack.get("fatalError")):
        raise ToolError("Persistent Player rejected the scene update or reported runtime errors.")
    manifest = Path(request["manifest"])
    if not manifest.is_absolute() or not manifest.is_file() or sha256(manifest) != request.get("manifestSha256"):
        raise ToolError("Scene update acknowledgement does not prove an immutable manifest SHA.")
    revision = json.loads(manifest.read_text()).get("revision")
    if not revision or ack.get("revision") != revision or request.get("revision") != revision:
        raise ToolError("Scene update acknowledgement has a stale content revision.")
    if native_identity is not None and (ack.get("nativeLoadCount") != 1
            or ack.get("nativeIdentityDigest") != native_identity
            or ack.get("nativeObjectCount") != native_object_count):
        raise ToolError("Scene update reloaded or replaced the retained native asset registry.")
    completed = _finite(ack.get("completedUtcUnixMilliseconds"), "completed first-frame timestamp")
    requested = _finite(request.get("requestedUtcUnixMilliseconds"), "reload request timestamp")
    elapsed = _finite(ack.get("elapsedSeconds"), "runtime reload seconds")
    if completed < edit_utc_ms or requested < edit_utc_ms or completed < requested or elapsed < 0:
        raise ToolError("Scene update timing is inconsistent; refusing to report latency.")


def benchmark(prepared: dict, *, runs: int = 3, timeout: float = 180) -> dict:
    output, project = Path(prepared["output"]), Path(prepared["project"])
    request_path = output / "reload-request.json"
    ack_path = Path(str(request_path) + ".ack.json")
    report = output / "runtime.json"
    proof_path = output / "proof.json"
    for path in (request_path, ack_path, report, Path(str(report) + ".ready.json"), proof_path):
        path.unlink(missing_ok=True)
    environment = {**os.environ, "UNITY_PLAY_SCENE_RELOAD_PROOF": str(proof_path)}
    command = [str(TOOL / "unity-play"), "--project", str(project), "--unity", prepared["editor"], "raw-run",
               "--scene", hybrid.SCENE, "--player", prepared["player"], "--native-catalog", prepared["seedCatalog"],
               "--raw-dir", str(output / "raw"), "--watch", "--reload-request", str(request_path), "--fps", "60",
               "--no-vsync", "--width", "960", "--height", "540", "--report", str(report)]
    summary = {"schemaVersion": 1, "success": False, "scene": hybrid.SCENE,
               "iterationMode": "persistent Player, raw scene replacement, native assets retained",
               "watchDetectionMeasured": True, "codeHotPatchMeasured": False, "heavyReimportMeasured": False,
               "setupSeconds": prepared["setupSeconds"], "launchCommand": command, "launches": [], "updates": []}
    revision, data_revision, added = 1, 1, []
    position = dict(prepared["probe"]["scenePosition"])
    started_utc_ms, started_monotonic = time.time_ns() / 1_000_000, time.monotonic()
    with (output / "launch.jsonl").open("w") as stdout, (output / "stderr.log").open("w") as stderr:
        process = subprocess.Popen(command, stdout=stdout, stderr=stderr, env=environment, start_new_session=True)
        try:
            ready = _wait_json(Path(str(report) + ".ready.json"), lambda value: bool(value.get("readyUtcUnixMilliseconds")),
                               process, timeout)
            baseline = _wait_json(proof_path, lambda value: value.get("iterationRevision") == 1
                                  and value.get("dataRevision") == 1, process, timeout)
            pid = baseline.get("pid")
            if type(pid) is not int or pid <= 0 or ready.get("pid") != pid:
                raise ToolError("Persistent ready and executed proof do not identify the same Player PID.")
            if ready.get("rawReloadRequestPath") != str(request_path):
                raise ToolError("Player did not advertise the requested persistent scene update path.")
            native_identity = ready.get("nativeIdentityDigest")
            native_object_count = ready.get("nativeObjectCount")
            if (ready.get("nativeLoadCount") != 1 or not isinstance(native_identity, str)
                    or not re.fullmatch(r"[0-9a-f]{64}", native_identity)
                    or type(native_object_count) is not int or native_object_count <= 0):
                raise ToolError("Persistent ready report does not prove one loaded native asset registry.")
            validate_proof(baseline, None, pid=pid, revision=revision, data_revision=data_revision, position=position, added=added)
            summary.update(pid=pid, gpu={"name": baseline["graphicsDeviceName"], "type": baseline["graphicsDeviceType"]},
                           initialLaunchToReadySeconds=(_finite(ready["readyUtcUnixMilliseconds"], "ready") - started_utc_ms) / 1000,
                           initialLaunchToProofSeconds=time.monotonic() - started_monotonic,
                           initialProof=baseline, nativeRetainedEntityIds={name: baseline[name] for name in NATIVE_IDS})
            summary.update(nativeLoadCount=1, nativeObjectCount=native_object_count, nativeIdentityDigest=native_identity)
            write_json(output / "initial-ready.json", ready)
            write_json(output / "initial-proof.json", baseline)
            write_json(output / "summary.json", summary)
            print(f"initial-ready: {summary['initialLaunchToReadySeconds']:.3f}s CLI to first frame; "
                  f"PID {pid}, one native load, {native_object_count} retained objects", flush=True)

            def update(kind: str, index: int) -> None:
                nonlocal revision, data_revision, position, added
                before_ack = json.loads(ack_path.read_text()) if ack_path.is_file() else {}
                edit_utc_ms, edit_monotonic = time.time_ns() / 1_000_000, time.monotonic()
                if kind == "transform":
                    revision += 1
                    position = transform_edit(project, prepared["probe"]["sceneTransformFileId"], revision, added)
                elif kind == "add-gameobject":
                    revision += 1
                    added = added + [add_object_edit(project, revision, added)]
                elif kind == "raw-scriptableobject":
                    data_revision += 1
                    data_edit(project, data_revision)
                else:
                    raise ToolError("Unknown scene reload benchmark update: " + kind)
                ack = _wait_json(ack_path, lambda value: value.get("requestId") != before_ack.get("requestId")
                                 and bool(value.get("requestId")), process, timeout)
                request = json.loads(request_path.read_text())
                validate_ack(ack, request, pid=pid, edit_utc_ms=edit_utc_ms,
                             native_identity=native_identity, native_object_count=native_object_count)
                if ack.get("reloadCount") != len(summary["updates"]) + 1:
                    raise ToolError("Player did not acknowledge exactly one scene reload per source edit.")
                proof = _wait_json(proof_path, lambda value: value.get("iterationRevision") == revision
                                   and value.get("dataRevision") == data_revision
                                   and value.get("manifestRevision") == ack.get("revision"), process, timeout)
                validate_proof(proof, baseline, pid=pid, revision=revision, data_revision=data_revision, position=position, added=added)
                observed = _finite(proof.get("observedUtcUnixMilliseconds"), "executed rendered proof timestamp")
                completed = _finite(ack["completedUtcUnixMilliseconds"], "first frame ack timestamp")
                if observed < edit_utc_ms:
                    raise ToolError("Rendered scene proof predates its source edit.")
                row = {"label": kind + "-" + str(index), "kind": kind, "pid": pid,
                       "editToFirstFrameAckSeconds": (completed - edit_utc_ms) / 1000,
                       "editToRenderedProofSeconds": (max(observed, completed) - edit_utc_ms) / 1000,
                       "observedWallSeconds": time.monotonic() - edit_monotonic,
                       "detectionAndPrepareSeconds": (request["requestedUtcUnixMilliseconds"] - edit_utc_ms) / 1000,
                       "runtimeReloadSeconds": ack["elapsedSeconds"], "request": request, "ack": ack, "proof": proof,
                       "samePlayerPid": True, "sameNativeInstanceIds": True, "editorStarted": False}
                summary["updates"].append(row)
                write_json(output / (row["label"] + ".json"), row)
                write_json(output / "summary.json", summary)
                print(f"{row['label']}: {row['editToRenderedProofSeconds']:.3f}s edit to rendered proof; PID {pid}", flush=True)
                # Confirm one settled revision before the next edit.
                time.sleep(.1)

            for kind in ("transform", "add-gameobject", "raw-scriptableobject"):
                for index in range(1, runs + 1):
                    update(kind, index)
            summary["mediansSeconds"] = {kind: statistics.median(item["editToRenderedProofSeconds"] for item in summary["updates"]
                                                                  if item["kind"] == kind)
                                         for kind in ("transform", "add-gameobject", "raw-scriptableobject")}
            # Ask the CLI to finish its report and clean up its own descendants.
            process.send_signal(signal.SIGINT)
            process.wait(timeout=20)
            if report.is_file():
                runtime = json.loads(report.read_text())
                summary["runtime"] = runtime
                if runtime.get("errors", 0) or runtime.get("exceptions", 0) or runtime.get("fatalError"):
                    raise ToolError("Persistent scene benchmark final report contains runtime errors.")
            if source_inventory(Path(prepared["sourceProject"])) != prepared["sourceInventory"]:
                raise ToolError("Benchmark changed the original authored source project.")
            if hybrid._host_inventory(Path(prepared["player"])) != prepared["hostInventory"]:
                raise ToolError("Benchmark changed the reusable Player host.")
            if (project / "Library").exists() or (project / "Builds").exists():
                raise ToolError("Disposable input acquired a source Library or Builds.")
            summary.update(success=True, sourceProjectUnchanged=True, hostUnchanged=True,
                           sourceLibraryExists=False, sourceBuildsExists=False)
        except BaseException as error:
            summary["failure"] = str(error)
            raise
        finally:
            if process.poll() is None:
                stop_child(process)
            write_json(output / "summary.json", summary)
    return summary


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--project", type=Path, default=REPO)
    parser.add_argument("--native-catalog", type=Path)
    parser.add_argument("--player", type=Path, default=REPO / "Builds/CLIPlayHybridHost/UnityPlayHost.x86_64")
    parser.add_argument("--unity", type=Path)
    parser.add_argument("--output", type=Path, default=REPO / "Temp/UnityPlay/scene-reload-benchmarks" / uuid.uuid4().hex[:12])
    parser.add_argument("--prepared", type=Path)
    parser.add_argument("--prepare-only", action="store_true")
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--timeout", type=float, default=180)
    args = parser.parse_args()
    if args.runs < 1 or not 0 < args.timeout < math.inf:
        parser.error("--runs and --timeout must be positive")
    try:
        if args.prepared:
            prepared = json.loads(args.prepared.resolve().read_text())
            if prepared.get("benchmark") != "persistent-scene-reload":
                raise ToolError("Choose a prepared persistent-scene-reload benchmark fixture.")
        else:
            source = args.project.resolve()
            catalog = args.native_catalog or Path(json.loads((source / "Temp/UnityPlay/native-import/current.json").read_text())["catalog"])
            prepared = prepare(source, catalog.resolve(), args.player.resolve(), editor_path(source, args.unity), args.output.resolve())
        if args.prepare_only:
            print(json.dumps({"success": True, "prepared": str(Path(prepared["output"]) / "prepared.json"),
                              "editorStarted": False, "playerStarted": False}, indent=2))
            return 0
        result = benchmark(prepared, runs=args.runs, timeout=args.timeout)
        print(json.dumps({"success": result["success"], "summary": str(Path(prepared["output"]) / "summary.json"),
                          "mediansSeconds": result["mediansSeconds"]}, indent=2))
        return 0
    except (ToolError, OSError, ValueError, KeyError, subprocess.TimeoutExpired) as error:
        print(str(error), file=__import__("sys").stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
