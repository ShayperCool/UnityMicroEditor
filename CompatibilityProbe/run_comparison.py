#!/usr/bin/env python3
"""Observe Editor in an isolated copy and compare the independent raw loader."""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import re
import shutil
import struct
import sys
import time
import uuid
import zlib

TOOLS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(TOOLS))

CASES = ("baseline", "transparent-texture", "cyclic-roots", "inherited-lifecycle",
         "execution-order", "disable-peer", "deactivate-peer", "destroy-peer-immediate",
         "late-reactivate", "enable-only-peer", "dynamic-add-component", "reentrant-activation")
ORDERED_CASES = {"execution-order", "disable-peer", "deactivate-peer", "destroy-peer-immediate",
                 "late-reactivate", "enable-only-peer"}


def digest_tree(project: Path) -> dict:
    return {str(path.relative_to(project)): hashlib.sha256(path.read_bytes()).hexdigest()
            for folder in ("Assets", "Packages", "ProjectSettings")
            for path in sorted((project / folder).rglob("*")) if path.is_file()}


def differences(expected, actual, path="$", tolerance=1e-5):
    if path.endswith((".serializedJson", ".scalarJson")) and isinstance(expected, str) and isinstance(actual, str):
        try:
            return differences(json.loads(expected), json.loads(actual), path + "{json}", tolerance)
        except json.JSONDecodeError:
            pass
    if isinstance(expected, bool) or isinstance(actual, bool):
        return [] if type(expected) is type(actual) and expected == actual else [{"path": path, "editor": expected, "raw": actual}]
    if isinstance(expected, (int, float)) and isinstance(actual, (int, float)):
        if math.isfinite(expected) and math.isfinite(actual) and math.isclose(expected, actual, rel_tol=tolerance, abs_tol=tolerance):
            return []
    elif type(expected) is type(actual):
        if isinstance(expected, dict):
            result = []
            for key in sorted(set(expected) | set(actual)):
                if key not in expected or key not in actual:
                    result.append({"path": f"{path}.{key}", "editor": expected.get(key), "raw": actual.get(key), "missingKey": True})
                else:
                    result.extend(differences(expected[key], actual[key], f"{path}.{key}", tolerance))
            return result
        if isinstance(expected, list):
            if len(expected) != len(actual):
                return [{"path": path + ".length", "editor": len(expected), "raw": len(actual)}]
            result = []
            for index, (left, right) in enumerate(zip(expected, actual)):
                result.extend(differences(left, right, f"{path}[{index}]", tolerance))
            return result
        if expected == actual:
            return []
    return [{"path": path, "editor": expected, "raw": actual}]


def lifecycle_groups(events):
    groups = {}
    for event in events:
        item = dict(event)
        item.pop("frameCount", None)
        groups.setdefault(item["objectPath"], []).append(item)
    return groups


def execution_order_errors(events, case, observation):
    """Check different execution orders without constraining equal-order objects."""
    if case not in ORDERED_CASES:
        return []
    errors = []
    for phase in ("Awake", "OnEnable", "Start"):
        early = [index for index, event in enumerate(events)
                 if event["objectPath"] == "Compatibility active" and event["phase"] == phase]
        late = [index for index, event in enumerate(events)
                if event["objectPath"] == "Compatibility peer" and event["phase"] == phase]
        # Mutation cases may suppress a peer callback. Presence and state are
        # compared against the real Editor trace by lifecycle_groups below.
        if case == "execution-order" and (len(early) != 1 or len(late) != 1):
            errors.append({"observation": observation, "phase": phase,
                           "error": "Expected one early and one late callback", "earlyIndices": early, "lateIndices": late})
        elif early and late and max(early) >= min(late):
            errors.append({"observation": observation, "phase": phase,
                           "error": "Execution order -100 must precede +100", "earlyIndices": early, "lateIndices": late})
    return errors


def dynamic_creation_errors(scene, observation):
    creators = [component for owner in scene.get("objects", []) for component in owner.get("components", [])
                if component.get("typeName") == "UnityPlay.Compatibility.CompatibilityDynamicCreationProbe"]
    if len(creators) != 1:
        return [{"path": "$.dynamicCreation." + observation, "error": "Expected one dynamic creation observer"}]
    captured = json.loads(creators[0]["serializedJson"])
    return [{"path": "$.dynamicCreation." + observation + "." + field,
             "error": "AddComponent must finish this callback before returning", "actual": captured.get(field)}
            for field in ("childAwakeRanBeforeAddReturned", "childEnableRanBeforeAddReturned")
            if captured.get(field) is not True]


def reentrant_activation_errors(events, observation):
    expected = [("Compatibility active", "Awake"), ("Compatibility peer", "Awake"),
                ("Compatibility peer", "OnEnable"), ("Compatibility active", "Awake.AfterActivatePeerRoot")]
    indices = [[index for index, event in enumerate(events)
                if event["objectPath"] == owner and event["phase"] == phase]
               for owner, phase in expected]
    if any(len(found) != 1 for found in indices):
        return [{"path": "$.reentrantActivation." + observation,
                 "error": "Expected one controller Awake and after-activation event, and one nested peer Awake/OnEnable",
                 "indices": indices}]
    positions = [found[0] for found in indices]
    if positions != sorted(positions):
        return [{"path": "$.reentrantActivation." + observation,
                 "error": "Peer Awake/OnEnable must execute inside controller SetActive before it returns", "indices": positions}]
    return []


def append_probe(scene: Path, identifier: int, name: str, script_guid: str, fields="", active=1, enabled=1):
    text = scene.read_text()
    roots = re.search(r"(?m)^SceneRoots:\n(?:[ \t]+[^\n]*\n)*", text)
    if roots:
        text = text[:roots.end()] + f"  - {{fileID: {identifier+1}}}\n" + text[roots.end():]
    text += f"""--- !u!1 &{identifier}
GameObject:
  m_ObjectHideFlags: 0
  serializedVersion: 6
  m_Component:
  - component: {{fileID: {identifier+1}}}
  - component: {{fileID: {identifier+2}}}
  m_Layer: 0
  m_Name: {name}
  m_TagString: Untagged
  m_IsActive: {active}
--- !u!4 &{identifier+1}
Transform:
  serializedVersion: 2
  m_GameObject: {{fileID: {identifier}}}
  m_LocalRotation: {{x: 0, y: 0, z: 0, w: 1}}
  m_LocalPosition: {{x: 0, y: 0, z: 0}}
  m_LocalScale: {{x: 1, y: 1, z: 1}}
  m_Children: []
  m_Father: {{fileID: 0}}
--- !u!114 &{identifier+2}
MonoBehaviour:
  m_GameObject: {{fileID: {identifier}}}
  m_Enabled: {enabled}
  m_Script: {{fileID: 11500000, guid: {script_guid}, type: 3}}
{fields}"""
    scene.write_text(text)


def fixture(output: Path, case: str):
    if case not in CASES:
        raise ValueError(f"Unknown comparison case: {case}")
    source = TOOLS / "Samples/RawProject"
    project = output / "raw-project"
    if project.exists():
        raise ValueError(f"Comparison output already contains a project: {project}")
    shutil.copytree(source, project, ignore=shutil.ignore_patterns("Temp", "Library", "Builds", "Logs"))
    if case == "transparent-texture":
        def chunk(kind, payload):
            return struct.pack(">I", len(payload)) + kind + payload + struct.pack(">I", zlib.crc32(kind + payload) & 0xffffffff)
        pixels = bytes([0, 255, 255, 255, 255, 255, 0, 0, 128,
                        0, 0, 0, 255, 0, 0, 255, 0, 255])
        png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", 2, 2, 8, 6, 0, 0, 0))
        png += chunk(b"IDAT", zlib.compress(pixels)) + chunk(b"IEND", b"")
        (project / "Assets/Textures/Probe.png").write_bytes(png)
    scripts = project / "Assets/UnityPlayCompatibility"
    scripts.mkdir()
    guids = {}
    for source_file in sorted((Path(__file__).parent / "Runtime").glob("*.cs")):
        destination = scripts / source_file.name
        shutil.copyfile(source_file, destination)
        guid = hashlib.md5(("unity-play-compatibility-" + source_file.name).encode()).hexdigest()
        guids[source_file.stem] = guid
        destination.with_name(destination.name + ".meta").write_text(
            f"fileFormatVersion: 2\nguid: {guid}\nMonoImporter:\n  serializedVersion: 2\n  defaultReferences: []\n  executionOrder: 0\n")
    scene = project / "Assets/Scenes/RawSample.unity"
    scene.write_text(scene.read_text().replace("degreesPerSecond: 40", "degreesPerSecond: 0"))
    fields = """  anchor: {fileID: 201}
  asset: {fileID: 11400000, guid: a01ac09a24c8493b862564ce94c80303, type: 2}
  references:
  - {fileID: 202}
  - {fileID: 501}
  revision: 17
  flag: 1
  scalar: 2.5
"""
    has_peer = case in ("cyclic-roots", "reentrant-activation") or case in ORDERED_CASES
    active_type = "CompatibilityEarlyLifecycleProbe" if case in ORDERED_CASES or case == "reentrant-activation" else "CompatibilityLifecycleProbe"
    if case == "inherited-lifecycle":
        active_type = "CompatibilityInheritedLifecycleProbe"
    elif case == "dynamic-add-component":
        active_type = "CompatibilityDynamicCreationProbe"
    active_fields = fields + ("  peerRoot: {fileID: 850}\n  peerBehaviour: {fileID: 852}\n" if has_peer else "  peerRoot: {fileID: 200}\n")
    operation = {"disable-peer": 1, "deactivate-peer": 2, "destroy-peer-immediate": 4,
                 "late-reactivate": 2, "reentrant-activation": 5}.get(case, 0)
    active_fields += f"  awakeOperation: {operation}\n"
    if case == "late-reactivate":
        active_fields += "  reactivatePeerOnStart: 1\n"
    if case == "inherited-lifecycle":
        active_fields += "  derivedRevision: 29\n"
    elif case == "dynamic-add-component":
        active_fields = "  created: {fileID: 0}\n  childAwakeRanBeforeAddReturned: 0\n  childEnableRanBeforeAddReturned: 0\n"
    append_probe(scene, 800, "Compatibility active", guids[active_type], active_fields)
    if has_peer:
        peer_type = "CompatibilityLateLifecycleProbe" if case in ORDERED_CASES or case == "reentrant-activation" else "CompatibilityLifecycleProbe"
        peer_fields = fields + "  peerRoot: {fileID: 800}\n  peerBehaviour: {fileID: 802}\n  awakeOperation: 0\n"
        if case == "enable-only-peer":
            peer_type = "CompatibilityEnableOnlyProbe"
            peer_fields = "  wasEnabled: 0\n  wasStarted: 0\n"
        append_probe(scene, 850, "Compatibility peer", guids[peer_type],
                     peer_fields, active=0 if case == "reentrant-activation" else 1)
    append_probe(scene, 1000, "Compatibility inactive", guids["CompatibilityLifecycleProbe"], fields, active=0)
    append_probe(scene, 1100, "Compatibility disabled", guids["CompatibilityLifecycleProbe"], fields, enabled=0)
    append_probe(scene, 900, "Compatibility snapshot", guids["SnapshotDriver"])
    return project


def compare(unity: Path, player: Path, output: Path, case="baseline", timeout=180):
    from unity_play import ToolError, require_idle, run_process, write_json
    output = output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    project = fixture(output, case)
    before = digest_tree(project)
    reference = output / "reference-project"
    shutil.copytree(project, reference)
    editor_sources = reference / "Assets/UnityPlayCompatibility/Editor"
    editor_sources.mkdir()
    shutil.copyfile(Path(__file__).parent / "Editor/ReferenceCapture.cs", editor_sources / "ReferenceCapture.cs")
    # Unity 6 module dependencies are versioned builtins, not registry downloads.
    modules = ["animation", "audio", "imageconversion", "jsonserialize", "physics", "physics2d", "ui", "imgui"]
    write_json(reference / "Packages/manifest.json", {"dependencies": {f"com.unity.modules.{name}": "1.0.0" for name in modules}})
    require_idle(reference)
    editor_report = output / "editor-observation.json"
    environment = os.environ.copy()
    environment.update(UNITY_PLAY_REFERENCE_REPORT=str(editor_report), UNITY_PLAY_REFERENCE_SCENE="Assets/Scenes/RawSample.unity")
    environment.pop("UNITY_PLAY_RAW_SNAPSHOT", None)
    command = [str(unity), "-batchmode", "-nographics", "-projectPath", str(reference),
               "-executeMethod", "UnityPlay.Compatibility.ReferenceCapture.RunFromCommandLine",
               "-logFile", str(output / "editor.log")]
    reference_launch = run_process(command, reference, output / "editor-launch.log", timeout, env=environment)
    if reference_launch["exitCode"] or not editor_report.is_file():
        failure = json.loads(editor_report.read_text()).get("failure") if editor_report.is_file() else "Missing observation report"
        write_json(output / "comparison.json", {"schemaVersion": 1, "success": False,
                   "case": case, "stage": "editor-reference", "failure": failure,
                   "editorObservation": str(editor_report), "editorLog": str(output / "editor.log"), "editorRun": reference_launch})
        raise ToolError(f"Editor reference capture failed: {failure}; see {output / 'editor.log'}")
    observed = json.loads(editor_report.read_text())
    if not observed.get("success"):
        raise ToolError(f"Editor observation failed: {observed.get('failure')}")
    raw_snapshot = output / "raw-observation.json"
    environment = os.environ.copy()
    environment["UNITY_PLAY_RAW_SNAPSHOT"] = str(raw_snapshot)
    command = [str(TOOLS / "unity-play"), "--project", str(project), "--unity", str(unity),
               "raw-run", "--player", str(player), "--scene", "Assets/Scenes/RawSample.unity",
               "--frames", "30", "--fps", "60", "--no-vsync", "--width", "960", "--height", "540",
               "--raw-dir", str(output / "revisions"), "--report", str(output / "raw-runtime.json")]
    raw_launch = run_process(command, project, output / "raw-launch.json", timeout, env=environment)
    raw_runtime = json.loads((output / "raw-runtime.json").read_text()) if (output / "raw-runtime.json").is_file() else None
    raw_observed = json.loads(raw_snapshot.read_text()) if raw_snapshot.is_file() else None
    scene_diff, event_diff, order_diff = [], [], []
    if raw_observed and "scene" in raw_observed and "lifecycle" in raw_observed:
        scene_diff = differences(observed["playScene"], raw_observed["scene"])
        if case == "dynamic-add-component":
            scene_diff += dynamic_creation_errors(observed["playScene"], "editor")
            scene_diff += dynamic_creation_errors(raw_observed["scene"], "raw")
        event_diff = differences(lifecycle_groups(observed["lifecycle"]), lifecycle_groups(raw_observed["lifecycle"]))
        order_diff = execution_order_errors(observed["lifecycle"], case, "editor")
        order_diff += execution_order_errors(raw_observed["lifecycle"], case, "raw")
        if case == "reentrant-activation":
            order_diff += reentrant_activation_errors(observed["lifecycle"], "editor")
            order_diff += reentrant_activation_errors(raw_observed["lifecycle"], "raw")
    elif raw_observed:
        scene_diff = [{"path": "$", "error": "Raw observation omitted required scene/lifecycle fields"}]
    else:
        scene_diff = [{"path": "$", "error": "Raw runner produced no scene observation"}]
    unchanged = before == digest_tree(project)
    no_library = not (project / "Library").exists()
    reference_valid = observed.get("capturedEditorIsPlaying") is True and observed.get("effectiveDomainReload") is True and observed.get("effectiveSceneReload") is True and observed.get("capturedEnterPlayModeOptionsEnabled") is False
    result = {"schemaVersion": 1, "success": raw_launch["exitCode"] == 0 and raw_observed is not None
              and not scene_diff and not event_diff and not order_diff and unchanged and no_library and reference_valid,
              "case": case, "referenceEngineVersion": observed.get("engineVersion"),
              "profile": "cold-start-both-reloads-enabled", "referenceProject": str(reference),
              "rawProject": str(project), "editorObservation": str(editor_report), "rawObservation": str(raw_snapshot),
              "sourceUnchanged": unchanged, "rawProjectLibraryExists": not no_library,
              "sourceSha256": before,
              "referencePlayModeVerified": reference_valid,
              "editorRun": reference_launch, "rawRun": raw_launch,
              "rawRuntime": raw_runtime,
              "sceneDifferences": scene_diff, "lifecycleDifferences": event_diff, "executionOrderDifferences": order_diff,
              "comparison": {"numericTolerance": 1e-5, "lifecycleOrder": "per-object sequence",
                             "globalOrderConstraints": "peer Awake/OnEnable execute inside controller Awake SetActive call" if case == "reentrant-activation" else "-100 precedes +100 for matching Awake/OnEnable/Start callbacks" if case in ORDERED_CASES else "none; equal-order cross-object order is unspecified",
                             "defines": {"editor": "UNITY_EDITOR plus engine/platform defines", "raw": "player defines; no UNITY_EDITOR"},
                             "ignoredEventFields": ["frameCount"]}, "wallSeconds": time.monotonic() - started}
    raw_failure = (raw_runtime or {}).get("fatalError", "") or ""
    result["outcome"] = "equivalent" if result["success"] else "different" if raw_observed else "failed"
    if "cyclic serialized references" in raw_failure:
        result["outcome"] = "unsupported"
        result["unsupportedReason"] = raw_failure
    write_json(output / "comparison.json", result)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--unity", type=Path, required=True)
    parser.add_argument("--player", type=Path, default=TOOLS.parents[1] / "Builds/CLIPlayHost/UnityPlayHost.x86_64")
    parser.add_argument("--output", type=Path, default=TOOLS.parents[1] / "Temp/UnityPlay/compatibility" / uuid.uuid4().hex)
    parser.add_argument("--case", choices=CASES, default="baseline")
    args = parser.parse_args()
    from unity_play import ToolError
    try:
        result = compare(args.unity.resolve(), args.player.resolve(), args.output, args.case)
    except (ToolError, ValueError) as error:
        print(json.dumps({"success": False, "failure": str(error), "output": str(args.output.resolve())}, indent=2))
        return 1
    print(json.dumps(result, indent=2))
    return 0 if result["success"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
