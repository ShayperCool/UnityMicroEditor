#!/usr/bin/env python3
"""Prepare a disposable source project for raw prefab/native-reference checks."""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys
import uuid

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "Tools/UnityMicroEditor"))
from raw import prepare_raw


def guid(name: str) -> str:
    return hashlib.md5(("unity-play-hybrid-probe:" + name).encode()).hexdigest()


def write(path: Path, content: str, name: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content)
    path.with_name(path.name + ".meta").write_text("fileFormatVersion: 2\nguid: " + guid(name) + "\n")


def object_record(owner: int, components: list[int], name: str) -> str:
    component_lines = "".join("  - component: {fileID: " + str(item) + "}\n" for item in components)
    return f"--- !u!1 &{owner}\nGameObject:\n  serializedVersion: 6\n  m_Component:\n{component_lines}  m_Name: {name}\n  m_TagString: Untagged\n  m_Layer: 0\n  m_IsActive: 1\n"


def transform_record(owner: int, identifier: int, parent: int = 0, children: list[int] | None = None) -> str:
    lines = "  m_Children: []\n" if not children else "  m_Children:\n" + "".join("  - {fileID: " + str(item) + "}\n" for item in children)
    return f"--- !u!4 &{identifier}\nTransform:\n  m_GameObject: {{fileID: {owner}}}\n  m_LocalPosition: {{x: 0, y: 0, z: 0}}\n  m_LocalRotation: {{x: 0, y: 0, z: 0, w: 1}}\n  m_LocalScale: {{x: 1, y: 1, z: 1}}\n{lines}  m_Father: {{fileID: {parent}}}\n"


def reference(asset: dict | None) -> str:
    return "{fileID: 0}" if asset is None else "{fileID: " + asset["fileId"] + ", guid: " + asset["guid"] + ", type: 3}"


def prepare(destination: Path, player: Path, editor: Path, source_project: Path | None, catalog_path: Path | None) -> dict:
    if destination.exists():
        raise ValueError("Probe output already exists; choose a fresh disposable directory.")
    destination.mkdir(parents=True)
    project = destination / "raw-project"
    source = source_project or ROOT / "Tools/UnityMicroEditor/Samples/RawProject"
    project.mkdir()
    for name in ("Assets", "Packages", "ProjectSettings"):
        path = source / name
        if path.exists():
            subprocess.run(["cp", "--reflink=always", "-a", str(path), str(project / name)], check=True)
    catalog = None
    mesh = {"fileId": "10202", "guid": "0000000000000000e000000000000000"}
    audio = controller = None
    if catalog_path:
        if source_project is None:
            raise ValueError("A native probe requires --source-project so all pack script types and deferred prefab targets remain available.")
        catalog = json.loads(catalog_path.read_text())
        if catalog.get("success") is not True or catalog.get("containsScenes") is not False:
            raise ValueError("Native probe needs a validated content-only catalog.")
        choices = {
            "mesh": ("UnityEngine.Mesh", "Assets/Dungeon/Art/KayKitSword/sword_B.fbx"),
            "audio": ("UnityEngine.AudioClip", "Assets/CAS Demo/Sounds/FPS/Pistol/S_Pistol_Fire_01.WAV"),
            "controller": ("UnityEngine.RuntimeAnimatorController", "Assets/Dungeon/Animation/AC_FirstPerson.controller"),
        }
        selected = {}
        for name, (type_name, path) in choices.items():
            found = [item for item in catalog["assets"] if item["typeName"] == type_name and item["path"] == path]
            if not found:
                raise ValueError("Native catalog is missing probe asset " + path + " (" + type_name + ")")
            selected[name] = sorted(found, key=lambda item: item["fileId"])[0]
        mesh, audio, controller = selected["mesh"], selected["audio"], selected["controller"]
        # This test binds the same immutable pack to an exact reflink copy of
        # authored inputs. It does not modify the published importer catalog.
        catalog["project"] = str(project)
        (destination / "probe-catalog.json").write_text(json.dumps(catalog, indent=2) + "\n")
    scripts = project / "Assets/UnityPlayHybridProbe"
    for path in (Path(__file__).parent / "Runtime").glob("*.cs"):
        write(scripts / path.name, path.read_text(), path.stem)
    native = 1 if catalog else 0
    proto = "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n"
    proto += object_record(100, [101, 104, 105, 103], "Hybrid prefab root")
    proto += transform_record(100, 101, children=[201])
    proto += object_record(200, [201, 106, 203], "Hybrid prefab peer")
    proto += transform_record(200, 201, 101)
    for identifier, owner, peer in [(103, 100, 203), (203, 200, 103)]:
        proto += f"--- !u!114 &{identifier}\nMonoBehaviour:\n  m_GameObject: {{fileID: {owner}}}\n  m_Enabled: 1\n  m_Script: {{fileID: 11500000, guid: {guid('HybridCloneNode')}, type: 3}}\n  peer: {{fileID: {peer}}}\n  mesh: {{fileID: 104}}\n  audio: {{fileID: 105}}\n  animator: {{fileID: 106}}\n  revision: 17\n  nativeExpected: {native}\n"
    proto += "--- !u!33 &104\nMeshFilter:\n  m_GameObject: {fileID: 100}\n  m_Mesh: " + reference(mesh) + "\n"
    proto += "--- !u!82 &105\nAudioSource:\n  m_GameObject: {fileID: 100}\n  m_Enabled: 1\n  m_PlayOnAwake: 0\n  m_audioClip: " + reference(audio) + "\n"
    proto += "--- !u!95 &106\nAnimator:\n  m_GameObject: {fileID: 200}\n  m_Enabled: 0\n  m_Controller: " + reference(controller) + "\n"
    write(scripts / "HybridPrefab.prefab", proto, "HybridPrefab")
    scene = "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n"
    scene += object_record(1000, [1001, 1002], "Hybrid probe driver") + transform_record(1000, 1001)
    scene += f"--- !u!114 &1002\nMonoBehaviour:\n  m_GameObject: {{fileID: 1000}}\n  m_Enabled: 1\n  m_Script: {{fileID: 11500000, guid: {guid('HybridProbeDriver')}, type: 3}}\n  prefab: {{fileID: 103, guid: {guid('HybridPrefab')}, type: 3}}\n  nativeExpected: {native}\n"
    scene += "--- !u!1660057539 &9223372036854775807\nSceneRoots:\n  m_Roots:\n  - {fileID: 1001}\n"
    write(scripts / "HybridProbe.unity", scene, "HybridProbeScene")
    result = prepare_raw(project, editor, player, "Assets/UnityPlayHybridProbe/HybridProbe.unity", destination / "raw", native_catalog=catalog)
    result["proof"] = str(destination / "proof.json")
    result["environment"] = {"UNITY_PLAY_HYBRID_PROOF": result["proof"]}
    result["runCommand"] = [str(ROOT / "Tools/UnityMicroEditor/unity-play"), "--project", str(project), "raw-run", "--scene", "Assets/UnityPlayHybridProbe/HybridProbe.unity", "--player", str(player), "--raw-dir", str(destination / "raw"), "--frames", "30", "--fps", "60", "--width", "960", "--height", "540", "--report", str(destination / "runtime.json")]
    if catalog:
        result["runCommand"] += ["--native-catalog", str(destination / "probe-catalog.json")]
        result["environment"]["UNITY_PLAY_CURVE_OBSERVATIONS"] = str(destination / "probe-catalog.json")
    (destination / "prepared.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "Temp/UnityPlay/hybrid-probes" / uuid.uuid4().hex[:12])
    parser.add_argument("--player", type=Path, default=ROOT / "Builds/CLIPlayHybridHost/UnityPlayHost.x86_64")
    parser.add_argument("--unity", type=Path, default=Path("/home/shaypercool/Unity/Hub/Editor/6000.6.3f1/Editor/Unity"))
    parser.add_argument("--source-project", type=Path)
    parser.add_argument("--native-catalog", type=Path)
    args = parser.parse_args()
    result = prepare(args.output.resolve(), args.player.resolve(), args.unity.resolve(), args.source_project.resolve() if args.source_project else None, args.native_catalog.resolve() if args.native_catalog else None)
    print(json.dumps({"success": True, "prepared": str(args.output.resolve() / "prepared.json"), "manifest": result["manifest"], "proof": result["proof"], "runCommand": result["runCommand"], "environment": result["environment"]}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
