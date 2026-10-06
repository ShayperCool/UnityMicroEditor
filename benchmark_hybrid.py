#!/usr/bin/env python3
"""Measure the real Dungeon SampleScene on the hybrid host in disposable sources.

The first launch has a cold C# cache and an already verified native asset cache.
OS caches are left alone. Warm launches, a gameplay C# method edit, a source
scene transform edit and an optional native WAV reimport each have runtime proof.
"""
from __future__ import annotations

import argparse
import copy
import datetime
import hashlib
import json
import math
import os
from pathlib import Path
import re
import statistics
import struct
import subprocess
import time
import uuid

from imported_assets import (_cached, _dependency_signature, _inventory,
                             inspect_catalog, prepare_import, sha256, source_inventory)
from raw_scene import parse_unity_yaml
from unity_play import ToolError, editor_path, require_idle, stop_child, write_json

TOOL = Path(__file__).resolve().parent
REPO = TOOL.parents[1]
SCENE = "Assets/Scenes/SampleScene.unity"
PROBE_PATH = "Assets/Dungeon/Scripts/UnityPlayHybridBenchmarkProbe.cs"
PROBE_GUID = hashlib.md5(b"unity-play-hybrid-benchmark-probe").hexdigest()


def _reflink(source: Path, destination: Path) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    result = subprocess.run(["cp", "--reflink=always", "-a", "--", str(source), str(destination)],
                            capture_output=True, text=True, check=False)
    if result.returncode:
        raise ToolError(f"Cannot create an isolated reflink copy of {source}: {result.stderr.strip()}")


def _host_inventory(player: Path) -> dict[str, str]:
    files = [player, player.parent / "UnityPlayer.so", player.parent / "host.json"]
    files += sorted((player.parent / (player.stem + "_Data")).rglob("*"))
    if any(not path.is_file() for path in files[:3]):
        raise ToolError("Hybrid benchmark requires a built universal host and host.json.")
    return {str(path): sha256(path) for path in files if path.is_file()}


def _choose_audio(catalog: dict, project: Path, requested: str | None) -> dict:
    choices = sorted((item for item in catalog["assets"]
                      if item["typeName"] == "UnityEngine.AudioClip" and item["path"].startswith("Assets/Dungeon/")
                      and item["path"].lower().endswith(".wav")), key=lambda item: (item["path"], item["fileId"]))
    if requested:
        choices = [item for item in choices if item["path"] == requested]
    for item in choices:
        metadata = (project / (item["path"] + ".meta")).read_text()
        # Unity GetData requires DecompressOnLoad. Keep the authored importer
        # settings rather than changing the profile solely for the benchmark.
        if re.search(r"^\s+loadType:\s*0\s*$", metadata, re.MULTILINE):
            _wav_layout((project / item["path"]).read_bytes())
            return item
    raise ToolError("The published catalog needs an imported Dungeon PCM WAV with loadType 0; choose --audio.")


def _wav_layout(data: bytes) -> tuple[int, int, int, int]:
    """Return PCM data offset/bytes/channels/sample width without rewriting RIFF."""
    if len(data) < 12 or data[:4] != b"RIFF" or data[8:12] != b"WAVE":
        raise ToolError("Heavy iteration fixture must be a RIFF/WAVE PCM file.")
    position, format_info, payload = 12, None, None
    while position + 8 <= len(data):
        kind, size = data[position:position + 4], struct.unpack_from("<I", data, position + 4)[0]
        start = position + 8
        if start + size > len(data):
            raise ToolError("Heavy iteration WAV has a truncated RIFF chunk.")
        if kind == b"fmt ":
            if size < 16:
                raise ToolError("Heavy iteration WAV has a truncated format chunk.")
            encoding, channels, _, _, alignment, bits = struct.unpack_from("<HHIIHH", data, start)
            if encoding != 1 or channels < 1 or bits not in (8, 16, 24, 32) or alignment != channels * (bits // 8):
                raise ToolError("Heavy iteration requires integer PCM WAV with consistent channel alignment.")
            format_info = channels, bits // 8
        elif kind == b"data":
            payload = start, size
        position = start + size + (size & 1)
    if format_info is None or payload is None or payload[1] < format_info[0] * format_info[1]:
        raise ToolError("Heavy iteration WAV is missing PCM sample data.")
    if payload[1] % (format_info[0] * format_info[1]):
        raise ToolError("Heavy iteration WAV ends inside a PCM frame.")
    return *payload, *format_info


def edit_pcm(path: Path) -> dict:
    """Change the copied file's first samples, keeping encoding and duration."""
    data = bytearray(path.read_bytes())
    offset, payload_bytes, channels, width = _wav_layout(data)
    before = hashlib.sha256(data).hexdigest()
    values = min(payload_bytes // width, 4096 * channels)
    for index in range(values):
        # Alternating short blocks survive normal audio resampling/compression.
        amplitude = .72 if (index // channels // 64) % 2 == 0 else -.61
        if width == 1:
            encoded = bytes([round(128 + amplitude * 127)])
        else:
            encoded = round(amplitude * ((1 << (width * 8 - 1)) - 1)).to_bytes(width, "little", signed=True)
        start = offset + index * width
        data[start:start + width] = encoded
    path.write_bytes(data)
    after = sha256(path)
    if before == after:
        raise ToolError("PCM iteration did not change the copied heavy asset.")
    return {"path": str(path), "beforeSha256": before, "afterSha256": after,
            "changedSampleValues": values, "channels": channels, "sampleWidthBytes": width,
            "byteLengthUnchanged": True, "importerSettingsChanged": False}


def _inject_probe(project: Path, audio: dict) -> dict:
    path = project / SCENE
    text = path.read_text()
    documents = parse_unity_yaml(path)
    by_id = {document["fileId"]: document for document in documents}
    shooting_guid = re.search(r"^guid:\s*([0-9a-f]{32})\s*$",
                               (project / "Assets/Dungeon/Scripts/DungeonShootingTarget.cs.meta").read_text(), re.MULTILINE)
    if shooting_guid is None:
        raise ToolError("DungeonShootingTarget script GUID is missing.")
    target = next((document for document in documents if document["classId"] == 114
                   and document["data"].get("m_Script", {}).get("guid") == shooting_guid.group(1)), None)
    floor = next((document for document in documents if document["classId"] == 1
                  and document["data"].get("m_Name") == "Floor"), None)
    if target is None or floor is None:
        raise ToolError("Full Dungeon SampleScene is missing its authored Floor or shooting target.")
    floor_transform = next((by_id[str(item["component"]["fileID"])] for item in floor["data"]["m_Component"]
                            if by_id[str(item["component"]["fileID"])]["classId"] == 4), None)
    if floor_transform is None:
        raise ToolError("SampleScene Floor has no explicit Transform.")
    roots = next((document for document in documents if document["classId"] == 1660057539), None)
    if roots is None:
        raise ToolError("SampleScene lacks the authored SceneRoots record.")
    identifiers = [str(900000000000000 + index) for index in range(3)]
    if set(identifiers) & by_id.keys():
        raise ToolError("SampleScene already contains reserved benchmark object identifiers.")
    owner, transform, component = identifiers
    payload = (
        f"--- !u!1 &{owner}\nGameObject:\n  serializedVersion: 6\n  m_Component:\n"
        f"  - component: {{fileID: {transform}}}\n  - component: {{fileID: {component}}}\n"
        "  m_Layer: 0\n  m_Name: UnityPlay hybrid iteration proof\n  m_TagString: Untagged\n  m_IsActive: 1\n"
        f"--- !u!4 &{transform}\nTransform:\n  m_GameObject: {{fileID: {owner}}}\n"
        "  m_LocalPosition: {x: 0, y: 0, z: 0}\n  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}\n"
        "  m_LocalScale: {x: 1, y: 1, z: 1}\n  m_Children: []\n  m_Father: {fileID: 0}\n"
        f"--- !u!114 &{component}\nMonoBehaviour:\n  m_GameObject: {{fileID: {owner}}}\n  m_Enabled: 1\n"
        f"  m_Script: {{fileID: 11500000, guid: {PROBE_GUID}, type: 3}}\n"
        f"  sceneTarget: {{fileID: {floor_transform['fileId']}}}\n  dungeonTarget: {{fileID: {target['fileId']}}}\n"
        f"  nativeAudio: {{fileID: {audio['fileId']}, guid: {audio['guid']}, type: 3}}\n")
    marker = f"--- !u!1660057539 &{roots['fileId']}\n"
    if text.count(marker) != 1:
        raise ToolError("SampleScene SceneRoots is not unique.")
    text = text.replace(marker, payload + marker)
    root_position = text.index(marker)
    graph, root_text = text[:root_position], text[root_position:]
    if "  m_Roots:\n" not in root_text:
        raise ToolError("SampleScene SceneRoots has no root list.")
    text = graph + root_text.replace("  m_Roots:\n", "  m_Roots:\n  - {fileID: " + transform + "}\n", 1)
    path.write_text(text)
    probe_path = project / PROBE_PATH
    if probe_path.exists() or probe_path.with_suffix(".cs.meta").exists():
        raise ToolError("Disposable source already contains a benchmark probe.")
    probe_path.write_text((TOOL / "HybridBenchmark/Runtime/UnityPlayHybridBenchmarkProbe.cs").read_text())
    probe_path.with_suffix(".cs.meta").write_text("fileFormatVersion: 2\nguid: " + PROBE_GUID + "\n")
    return {"sceneTransformFileId": floor_transform["fileId"], "dungeonComponentFileId": target["fileId"],
            "scenePosition": floor_transform["data"]["m_LocalPosition"], "audio": audio,
            "codePath": PROBE_PATH, "codeAssembly": "Dungeon.Runtime"}


def _owned_package_roots(original: dict, project: Path, cache: Path, editor: Path) -> list[dict]:
    """Verify the fixture's final package locations before compiling any code."""
    baseline = Path(original["importProject"]).resolve()
    fixture = (cache / "ImportProject").resolve()
    source_project = Path(original["project"]).resolve()
    sidecar_path = fixture / ".unity-play-package-snapshots.json"
    snapshots = json.loads(sidecar_path.read_text()) if sidecar_path.is_file() else {}
    if snapshots:
        if snapshots.get("schemaVersion") != 1 or any(
                snapshots.get("originalPackages", {}).get(name) != sha256(project / "Packages" / name)
                for name in ("manifest.json", "packages-lock.json")):
            raise ToolError("Fixture package snapshots do not prove the original manifest and lock hashes.")
    by_name = {}
    for record in snapshots.get("snapshots", []):
        name = record.get("name")
        if not isinstance(name, str) or name in by_name:
            raise ToolError("Fixture package snapshot names are invalid or duplicated.")
        by_name[name] = record
    locked = json.loads((project / "Packages/packages-lock.json").read_text()).get("dependencies", {})
    result = []
    seen = set()
    for root in original.get("sourceRoots", []):
        logical = root.get("path", "")
        parts = Path(logical).parts
        if len(parts) != 2 or parts[0] != "Packages" or parts[1] in {".", ".."} or logical in seen:
            raise ToolError("Native package source root is invalid or duplicated.")
        seen.add(logical)
        origin = Path(root.get("source", ""))
        if not origin.is_absolute() or origin.is_symlink() or not origin.is_dir():
            raise ToolError("Native package source root is not an absolute regular directory.")
        origin = origin.resolve()
        name = parts[1]
        if name in by_name:
            record = by_name[name]
            target = Path(record.get("source", ""))
            requirement = locked.get(name, {})
            package_id = name + "@" + requirement.get("version", "")
            if (not target.is_absolute() or not target.resolve().is_relative_to(cache / "PackageSources")
                    or record.get("packageId") != package_id
                    or record.get("packageSource") != requirement.get("source")
                    or requirement.get("source") not in {"registry", "git"}
                    or requirement.get("source") == "git" and (not requirement.get("hash") or record.get("gitHash") != requirement["hash"])):
                raise ToolError("Fixture package snapshot lacks exact owned path/version/Git provenance: " + name)
            expected = record.get("sha256")
        elif origin.is_relative_to(baseline):
            # Built-in modules and bundled engine packages resolve through the
            # native ImportProject Library. That entire idle Library was cloned.
            target = fixture / origin.relative_to(baseline)
            expected = None
        elif origin.is_relative_to(source_project / "Packages"):
            target = project / origin.relative_to(source_project)
            expected = None
        elif origin.is_relative_to(editor.parent / "Data"):
            # Some Unity versions resolve built-in packages directly in the
            # immutable installation rather than into Library/PackageCache.
            target, expected = origin, None
        else:
            raise ToolError("Native package root has no proven fixture-owned or engine installation source: " + logical)
        if target.is_symlink() or not target.is_dir():
            raise ToolError("Fixture package source is missing or symlinked: " + str(target))
        source_files = _inventory(origin)
        target_files = _inventory(target)
        if source_files != target_files:
            raise ToolError("Fixture package source differs from the native catalog bytes: " + logical)
        fingerprint = hashlib.sha256(json.dumps(target_files, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
        if expected is not None and fingerprint != expected:
            raise ToolError("Fixture package snapshot checksum differs from its provenance: " + logical)
        package = json.loads((target / "package.json").read_text())
        if package.get("name") != name:
            raise ToolError("Fixture package.json name differs from its logical root: " + logical)
        if _inventory(origin) != source_files:
            raise ToolError("Native package source changed during fixture ownership verification: " + logical)
        result.append({**root, "source": str(target.resolve())})
    return result


def _remap_dependencies(original: dict, project: Path, roots: list[dict]) -> list[dict]:
    old_roots = {item["path"]: Path(item["source"]) for item in original.get("sourceRoots", [])}
    new_roots = {item["path"]: Path(item["source"]) for item in roots}
    result = []
    for source in original.get("sourceDependencies", []):
        logical = source["path"]
        if Path(logical).is_absolute() or ".." in Path(logical).parts:
            raise ToolError("Native dependency logical path escapes its source root.")
        if logical.startswith(("Assets/", "ProjectSettings/")):
            target = project / logical
        elif logical.startswith("Packages/"):
            parts = Path(logical).parts
            root_name = "/".join(parts[:2])
            if len(parts) < 3 or root_name not in new_roots or root_name not in old_roots:
                raise ToolError("Native package dependency has no exact logical source root: " + logical)
            relative = Path(*parts[2:])
            if Path(source["physicalPath"]).resolve() != (old_roots[root_name] / relative).resolve():
                raise ToolError("Native dependency physical path differs from its declared package source: " + logical)
            target = new_roots[root_name] / relative
        else:
            raise ToolError("Native dependency has no owned source path: " + logical)
        if (not target.is_file() or sha256(target) != source["sha256"]
                or source.get("bytes") is not None and target.stat().st_size != source["bytes"]):
            raise ToolError("Remapped native dependency bytes differ from the published catalog: " + logical)
        result.append({**source, "physicalPath": str(target.resolve())})
    return result


def _fork_catalog(original: dict, project: Path, cache: Path, source_signature: dict,
                  *, source_roots: list[dict] | None = None) -> Path:
    """Bind verified unchanged native content to an exact disposable source copy."""
    roots = source_roots if source_roots is not None else original.get("sourceRoots", [])
    signature = _dependency_signature(project, [SCENE], [], source_inventory(project), roots,
                                      {item["path"] for item in original["assets"]})
    if signature != source_signature:
        changed = sorted(key for key in signature if signature.get(key) != source_signature.get(key))
        raise ToolError("Injected runtime proof changed native content inputs: " + ", ".join(changed))
    catalog = copy.deepcopy(original)
    catalog.pop("catalogPath", None)
    catalog["project"] = str(project)
    catalog["importProject"] = str(cache / "ImportProject")
    catalog["createdUtc"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
    catalog["benchmarkSeed"] = {"originalCatalog": original["catalogPath"],
                                 "sourceCopyVerified": True, "nativeSignatureUnchanged": True}
    catalog["sourceRoots"] = roots
    catalog["sourceDependencies"] = _remap_dependencies(original, project, roots)
    catalog["discoverySignature"] = signature
    # The cache ID includes its source binding. Original pack data/hash/CRC and
    # importer observations remain unchanged and are verified again below.
    content_id = {key: value for key, value in catalog.items() if key not in {"packs", "revision", "createdUtc"}}
    content_id["packs"] = [{key: value for key, value in pack.items() if key != "bundlePath"} for pack in catalog["packs"]]
    revision = hashlib.sha256(json.dumps(content_id, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
    destination = cache / "revisions" / revision
    destination.mkdir(parents=True, exist_ok=False)
    for index, pack in enumerate(catalog["packs"]):
        origin = Path(pack["bundlePath"])
        target = destination / f"{index:03d}-{origin.name}"
        _reflink(origin, target)
        pack["bundlePath"] = str(target)
    catalog["revision"] = revision
    path = destination / "catalog.json"
    write_json(path, catalog)
    inspect_catalog(path)
    write_json(cache / "current.json", {"schemaVersion": 1, "revision": revision,
                                        "catalog": str(path), "catalogSha256": sha256(path)})
    if _cached(project, [SCENE], [], cache) is None:
        raise ToolError("Verified copied native catalog did not yield a real importer cache hit.")
    return path


def prepare(source: Path, catalog_path: Path, player: Path, editor: Path, output: Path,
            *, audio_path: str | None = None, seed_library: bool = True) -> dict:
    started = time.monotonic()
    if output.exists():
        raise ToolError(f"Choose a fresh benchmark output directory: {output}")
    source, output = source.resolve(), output.resolve()
    original = inspect_catalog(catalog_path)
    if original.get("project") != str(source):
        raise ToolError("Benchmark source project and published native catalog differ.")
    before = source_inventory(source)
    signature = _dependency_signature(source, [SCENE], [], before, original.get("sourceRoots", []),
                                      {item["path"] for item in original["assets"]})
    if signature != original.get("discoverySignature"):
        raise ToolError("Published native catalog is stale for the current tool/source inputs. Import assets first.")
    host_before = _host_inventory(player)
    project = output / "source-project"
    project.mkdir(parents=True)
    for name in ("Assets", "Packages", "ProjectSettings"):
        _reflink(source / name, project / name)
    if source_inventory(project) != before or source_inventory(source) != before:
        raise ToolError("Authored files changed while making the exact disposable source copy.")
    selected = _choose_audio(original, project, audio_path)
    proof = _inject_probe(project, selected)
    cache = output / "native-cache"
    seed = None
    roots = original.get("sourceRoots", [])
    if seed_library:
        baseline_import = Path(original["importProject"])
        owner = baseline_import / ".unity-play-native-import-owner.json"
        ownership = json.loads(owner.read_text()) if owner.is_file() else {}
        if ownership.get("schemaVersion") != 1 or ownership.get("project") != str(source):
            raise ToolError("Heavy benchmark can seed only a tool-owned importer for this source project.")
        require_idle(baseline_import)
        if not (baseline_import / "Library").is_dir() or (baseline_import / "Library").is_symlink():
            raise ToolError("Heavy benchmark requires the tool-owned native importer Library for a warm reimport.")
        prepared = prepare_import(project, editor, [SCENE], cache_dir=cache, seed_library=False)
        fixture_import = Path(prepared["importProject"])
        _reflink(baseline_import / "Library", fixture_import / "Library")
        # This provenance connects the source registry/Git requests to the
        # baseline's immutable file: package snapshots. A second prepare_import
        # can then create the fixture's own snapshots before the heavy export.
        package_snapshots = baseline_import / ".unity-play-package-snapshots.json"
        if package_snapshots.is_file():
            _reflink(package_snapshots, fixture_import / package_snapshots.name)
        # Resolve all snapshots to this fixture before the first C# compile.
        # Otherwise the heavy reimport changes the physical package paths and
        # produces a compiler cache miss unrelated to the edited WAV.
        prepare_import(project, editor, [SCENE], cache_dir=cache, seed_library=False)
        roots = _owned_package_roots(original, project, cache, editor)
        seed = {"sourceProject": str(baseline_import), "sourceLibrary": str(baseline_import / "Library"),
                "copyMode": "xfs-reflink", "sourceWasIdle": True, "sourceInputLibraryUsed": False}
        write_json(fixture_import / ".unity-play-library-seed.json", seed)
    fork = _fork_catalog(original, project, cache, signature, source_roots=roots)
    result = {"schemaVersion": 1, "sourceProject": str(source), "project": str(project),
              "originalCatalog": str(catalog_path.resolve()), "seedCatalog": str(fork),
              "nativeCache": str(cache), "player": str(player), "editor": str(editor),
              "output": str(output), "probe": proof, "setupSeconds": time.monotonic() - started,
              "librarySeed": seed, "packageRootsOwnedBeforeCompile": seed_library,
              "packageSourceRoots": roots, "sourceInventory": before, "hostInventory": host_before}
    write_json(output / "prepared.json", result)
    return result


def edit_scene(project: Path, transform_id: str) -> dict:
    path = project / SCENE
    text = path.read_text()
    expression = re.compile(r"(^--- !u!4 &" + re.escape(transform_id) + r"\nTransform:\n)(.*?)(?=^--- |\Z)", re.MULTILINE | re.DOTALL)
    match = expression.search(text)
    if match is None:
        raise ToolError("Benchmark transform no longer exists in the source scene.")
    document = parse_unity_yaml(path, text=match.group(0))[0]
    position = dict(document["data"]["m_LocalPosition"])
    position["x"] = float(position["x"]) + .125
    replacement = "  m_LocalPosition: {x: " + str(position["x"]) + ", y: " + str(position["y"]) + ", z: " + str(position["z"]) + "}"
    body, count = re.subn(r"^  m_LocalPosition:.*$", replacement, match.group(2), count=1, flags=re.MULTILINE)
    if count != 1:
        raise ToolError("Benchmark transform has no unique local position.")
    path.write_text(text[:match.start()] + match.group(1) + body + text[match.end():])
    return position


def _proof_matches(proof: dict, revision: int, position: dict) -> None:
    if proof.get("schemaVersion") != 1 or proof.get("success") is not True:
        raise ToolError("Full-scene benchmark produced no successful runtime proof.")
    if type(proof.get("codeRevision")) is not int or proof["codeRevision"] != revision or proof.get("sceneName") != "SampleScene":
        raise ToolError("Full-scene benchmark did not execute the expected gameplay method/scene.")
    for axis, value in position.items():
        actual = proof.get("sceneTargetLocalPosition", {}).get(axis)
        if type(actual) not in (int, float) or not math.isfinite(actual) or abs(actual - float(value)) > 1e-5:
            raise ToolError("Full-scene benchmark did not load the expected raw source scene position.")
    rms = proof.get("audioRms")
    if (proof.get("sceneReferencesRestored") is not True or proof.get("dungeonTargetAwakeObserved") is not True
            or proof.get("dungeonTargetType") != "Dungeon.DungeonShootingTarget" or not proof.get("audioFingerprint")
            or proof.get("audioReadValues", 0) < 1 or type(rms) not in (int, float) or not math.isfinite(rms) or rms <= 1e-8):
        raise ToolError("Full-scene benchmark game component or native audio was not initialized.")


def _timings(record: dict, proof: dict, command_started_utc_ms: float, command_wall: float) -> dict:
    """Keep whole CLI latency separate from the tool's internal phase sum."""
    ready = record.get("runtime", {}).get("readyUtcUnixMilliseconds")
    observed = proof.get("observedUtcUnixMilliseconds")
    player_started = record.get("resources", {}).get("startedUtcUnixMilliseconds")
    prepare = record.get("rawBuild", {}).get("prepareSeconds")
    values = (ready, observed, player_started, prepare, command_started_utc_ms, command_wall)
    if any(type(value) not in (int, float) or not math.isfinite(value) for value in values):
        raise ToolError("Benchmark timing data is missing or nonfinite.")
    # Millisecond rounding is tolerated, a clock jump or stale proof is not.
    tolerance = .05
    if (prepare < 0 or command_wall <= 0 or ready < command_started_utc_ms or ready < player_started - tolerance * 1000
            or observed < ready - tolerance * 1000 or player_started < command_started_utc_ms - tolerance * 1000
            or observed > command_started_utc_ms + (command_wall + tolerance) * 1000):
        raise ToolError("Benchmark timestamps are inconsistent; do not report these latency values.")
    return {"commandToReadySeconds": (ready - command_started_utc_ms) / 1000,
            "commandToProbeSeconds": (observed - command_started_utc_ms) / 1000,
            "prepareToProbeSeconds": prepare + (observed - player_started) / 1000}


def _resource_measurement(resources: dict) -> dict:
    if resources.get("scope") != "single-process":
        raise ToolError("Benchmark requires single-instance resource accounting.")
    steady = resources.get("steady", {})
    if steady.get("available") is not True or steady.get("sampleCount", 0) < 2:
        raise ToolError("Benchmark has too few steady scene resource samples.")
    start, end = steady.get("startElapsedSeconds"), steady.get("endElapsedSeconds")
    if (any(type(value) not in (int, float) or not math.isfinite(value) for value in (start, end))
            or end - start < 1):
        raise ToolError("Benchmark steady scene resource window is shorter than one second.")
    for key in ("meanRssMiB", "meanPssMiB", "meanPrivateMiB", "meanGpuMiB"):
        value = steady.get(key)
        if value is not None and (type(value) not in (int, float) or not math.isfinite(value) or value < 0):
            raise ToolError("Benchmark resource value is invalid: " + key)
    if type(steady.get("meanRssMiB")) not in (int, float) or steady["meanRssMiB"] <= 0:
        raise ToolError("Benchmark has no measured RSS for the launched scene instance.")
    gpu_records = [item["gpu"] for item in resources.get("samples", [])
                   if item.get("status") == "available" and "gpu" in item
                   and start <= item.get("elapsedSeconds", -1) < end]
    gpu_error = next((item.get("reason") for item in gpu_records if item.get("status") == "unavailable"), None)
    gpu_pid_listed = any(device.get("pidListed") is True for item in gpu_records for device in item.get("devices", []))
    gpu_measured = steady.get("meanGpuMiB") is not None and gpu_pid_listed
    if not gpu_measured and gpu_error is None:
        gpu_error = "NVML did not list the Player process in the steady window" if not gpu_pid_listed else "NVML per-process allocation is unavailable"
    return {"scope": "single-process", "steadyWindowSeconds": end - start,
            "steadySampleCount": steady["sampleCount"],
            "gpuMemoryMeasured": gpu_measured,
            "gpuMemoryUnavailableReason": gpu_error if not gpu_measured else None,
            "cpuPercentMeaning": resources.get("cpuPercentMeaning"),
            "memoryUnits": "MiB"}


def _run_cli(command: list[str], stdout, stderr, environment: dict, timeout: float) -> int:
    process = subprocess.Popen(command, stdout=stdout, stderr=stderr, env=environment, start_new_session=True)
    try:
        return process.wait(timeout=timeout)
    except BaseException:
        # Stop descendants while the CLI parent is still alive. Unity and its
        # workers use their own sessions and must not outlive a timed-out run.
        stop_child(process)
        raise


def benchmark(prepared: dict, *, runs: int = 3, skip_heavy: bool = False, timeout: float = 2100) -> dict:
    output, project = Path(prepared["output"]), Path(prepared["project"])
    player, editor = Path(prepared["player"]), Path(prepared["editor"])
    launches = []
    summary = {"schemaVersion": 1, "success": False, "project": str(project), "host": str(player),
               "scene": SCENE, "sourceLibraryUsed": False, "sourceScenesCooked": False,
               "iterationMode": "raw-run prepare and Player restart",
               "watchDetectionMeasured": False, "inProcessHotPatchMeasured": False,
               "latencyDefinitions": {"commandToReadySeconds": "whole CLI start through the first rendered frame ready report",
                                      "prepareToReadySeconds": "internal raw preparation plus Player startup; excludes CLI and GPU preflight overhead",
                                      "commandToProbeSeconds": "whole CLI start through executed C# and decoded AudioClip proof",
                                      "commandWallSeconds": "whole CLI including 120 frames and shutdown"},
               "firstLaunchCacheState": {"code": "cold", "nativeAssets": "verified warm cache", "os": "untouched"},
               "setupSeconds": prepared["setupSeconds"], "librarySeed": prepared["librarySeed"], "launches": launches}
    summary["packageRootsOwnedBeforeCompile"] = prepared.get("packageRootsOwnedBeforeCompile", False)
    baseline_audio = None
    revision = 1
    position = dict(prepared["probe"]["scenePosition"])

    def verify_inputs() -> None:
        if _host_inventory(player) != prepared["hostInventory"]:
            raise ToolError("Universal host files changed during the benchmark.")
        if source_inventory(Path(prepared["sourceProject"])) != prepared["sourceInventory"]:
            raise ToolError("Original authored source files changed during the benchmark.")
        if (project / "Library").exists() or (project / "Builds").exists():
            raise ToolError("Raw source unexpectedly acquired a project Library or Builds.")

    def launch(label: str, *, expected_native_hit: bool, expected_code_hit: bool) -> dict:
        proof_path = output / (label + "-proof.json")
        command = [str(TOOL / "unity-play"), "--project", str(project), "--unity", str(editor),
                   "raw-run", "--scene", SCENE, "--player", str(player), "--raw-dir", str(output / "raw"),
                   "--native-import", "--native-cache", prepared["nativeCache"], "--frames", "120", "--fps", "60",
                   "--no-vsync", "--width", "960", "--height", "540", "--report", str(output / (label + "-runtime.json"))]
        environment = {**os.environ, "UNITY_PLAY_HYBRID_BENCHMARK_PROOF": str(proof_path)}
        proof_path.unlink(missing_ok=True)
        started = time.monotonic()
        started_utc_ms = time.time_ns() / 1_000_000
        with (output / (label + "-launch.json")).open("w") as stdout, (output / (label + "-stderr.log")).open("w") as stderr:
            exit_code = _run_cli(command, stdout, stderr, environment, timeout)
        wall = time.monotonic() - started
        if exit_code:
            raise ToolError(f"Hybrid benchmark {label} failed, exit {exit_code}. See {output / (label + '-stderr.log')}")
        record = json.loads((output / (label + "-launch.json")).read_text())
        runtime = record.get("runtime", {})
        if (runtime.get("success") is not True or runtime.get("errors", 1) != 0 or runtime.get("exceptions", 1) != 0
                or runtime.get("frames", 0) < 120 or runtime.get("graphicsDeviceType") != "OpenGLCore"
                or runtime.get("targetFrameRate") != 60 or runtime.get("vSyncCount") != 0
                or runtime.get("noVsyncRequested") is not True):
            raise ToolError(f"Hybrid benchmark {label} has a failed runtime/GPU report.")
        if not proof_path.is_file():
            raise ToolError(f"Hybrid benchmark {label} produced no executed-code/native-audio proof.")
        proof = json.loads(proof_path.read_text())
        _proof_matches(proof, revision, position)
        if proof.get("audioName") != Path(prepared["probe"]["audio"]["path"]).stem:
            raise ToolError(f"Hybrid benchmark {label} restored a different AudioClip than the copied heavy source.")
        native, compiler = record["rawBuild"]["nativeImport"], record["rawBuild"]["compiler"]
        if (record["rawBuild"].get("requiresProjectLibrary") is not False
                or record["rawBuild"].get("requiresCookedScene") is not False
                or compiler.get("projectLibraryUsed") is not False):
            raise ToolError(f"Hybrid benchmark {label} used the input Library or a cooked source scene.")
        if native.get("cacheHit") is not expected_native_hit or compiler.get("cacheHit") is not expected_code_hit:
            raise ToolError(f"Hybrid benchmark {label} did not exercise the requested native/compiler cache state.")
        if record["rawBuild"].get("editorStarted") is not (not expected_native_hit):
            raise ToolError(f"Hybrid benchmark {label} has inconsistent Editor launch accounting.")
        measurement = _resource_measurement(record.get("resources", {}))
        timings = _timings(record, proof, started_utc_ms, wall)
        steady = dict(record["resources"]["steady"])
        if not measurement["gpuMemoryMeasured"]:
            for key in ("peakGpuMiB", "meanGpuMiB", "lastGpuMiB"):
                steady[key] = None
        row = {"label": label, "commandWallSeconds": wall,
               "prepareSeconds": record["rawBuild"]["prepareSeconds"],
               "compileSeconds": compiler["elapsedSeconds"], "codeCacheHit": compiler["cacheHit"],
               "compiledAssemblyCount": compiler.get("compiledAssemblyCount", 0 if compiler["cacheHit"] else len(compiler["assemblies"])),
               "reusedAssemblyCount": compiler.get("reusedAssemblyCount", len(compiler["assemblies"]) if compiler["cacheHit"] else 0),
               "compiledAssemblies": [item["name"] for item in compiler["assemblies"] if item.get("cacheHit") is False],
               "nativeCacheHit": native["cacheHit"], "nativeImportSeconds": native.get("wallSeconds", 0),
               "editorStarted": record["rawBuild"]["editorStarted"],
               "playerReadySeconds": record["startupToSceneReadySeconds"],
               "prepareToReadySeconds": record["prepareToSceneReadySeconds"],
               **timings,
               "gpu": {"name": runtime["graphicsDeviceName"], "type": runtime["graphicsDeviceType"]},
               "steady": steady, "resourceMeasurement": measurement, "proof": proof,
               "rawManifest": record["rawBuild"]["manifest"], "isolatedPlayer": record["rawBuild"].get("player"),
               "runtimeReport": str(output / (label + "-runtime.json")),
               "playerLog": record["playerLog"], "nativeCatalog": native.get("catalog")}
        launches.append(row)
        write_json(output / "summary.json", summary)
        print(f"{label}: {row['commandToReadySeconds']:.3f}s CLI to ready; "
              f"code cache {row['codeCacheHit']}, native cache {row['nativeCacheHit']}", flush=True)
        verify_inputs()
        return row

    try:
        verify_inputs()
        if (output / "raw" / "compiler-cache").exists():
            raise ToolError("First launch already has a C# compiler cache. Prepare a fresh benchmark fixture.")
        if not skip_heavy and not prepared.get("librarySeed"):
            raise ToolError("Heavy measurement requires a prepared tool-owned Library seed; prepare without --skip-heavy first.")
        cold = launch("cold-code", expected_native_hit=True, expected_code_hit=False)
        baseline_audio = cold["proof"]["audioFingerprint"]
        for index in range(runs):
            row = launch(f"warm-{index + 1}", expected_native_hit=True, expected_code_hit=True)
            if row["proof"]["audioFingerprint"] != baseline_audio:
                raise ToolError("Unchanged native audio produced a different warm-run fingerprint.")
        code = project / PROBE_PATH
        before = "private static int CodeRevision() { return 1; } // UNITY_PLAY_BENCHMARK_CODE"
        after = "private static int CodeRevision() { return 2; } // UNITY_PLAY_BENCHMARK_CODE"
        if code.read_text().count(before) != 1:
            raise ToolError("Gameplay method edit no longer matches the copied probe source.")
        code.write_text(code.read_text().replace(before, after))
        revision = 2
        row = launch("code-change", expected_native_hit=True, expected_code_hit=False)
        if row["proof"]["audioFingerprint"] != baseline_audio:
            raise ToolError("A C# method edit unexpectedly changed native audio content.")
        position = edit_scene(project, prepared["probe"]["sceneTransformFileId"])
        row = launch("scene-change", expected_native_hit=True, expected_code_hit=True)
        if row["proof"]["audioFingerprint"] != baseline_audio:
            raise ToolError("A raw scene transform edit unexpectedly changed native audio content.")
        if not skip_heavy:
            summary["heavyEdit"] = edit_pcm(project / prepared["probe"]["audio"]["path"])
            row = launch("audio-change", expected_native_hit=False, expected_code_hit=True)
            if row["proof"]["audioFingerprint"] == baseline_audio:
                raise ToolError("Unity reimport did not update the AudioClip's actual decoded PCM samples.")
            if row["proof"]["audioSamples"] != cold["proof"]["audioSamples"]:
                raise ToolError("PCM edit changed the clip's expected duration/sample count.")
            summary["heavyEdit"]["nativeDecodedAudioChanged"] = True
        verify_inputs()
        warm = launches[1:1 + runs]
        summary.update(success=True, hostUnchanged=True, sourceProjectUnchanged=True,
                       inputLibraryExists=False, inputBuildsExists=False,
                       warmMedianSeconds=statistics.median(row["prepareToReadySeconds"] for row in warm),
                       warmMedianPlayerReadySeconds=statistics.median(row["playerReadySeconds"] for row in warm),
                       warmMedianCommandToReadySeconds=statistics.median(row["commandToReadySeconds"] for row in warm),
                       heavyMeasured=not skip_heavy)
        return summary
    except Exception as error:
        summary["failure"] = str(error)
        raise
    finally:
        write_json(output / "summary.json", summary)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--project", type=Path, default=REPO)
    parser.add_argument("--native-catalog", type=Path)
    parser.add_argument("--player", type=Path, default=REPO / "Builds/CLIPlayHybridHost/UnityPlayHost.x86_64")
    parser.add_argument("--unity", type=Path)
    parser.add_argument("--output", type=Path, default=REPO / "Temp/UnityPlay/hybrid-benchmarks" / uuid.uuid4().hex[:12])
    parser.add_argument("--runs", type=int, default=3, help="number of unchanged warm launches after the cold C# launch")
    parser.add_argument("--audio", help="existing imported Assets/Dungeon/*.wav with decompressed sample data")
    parser.add_argument("--skip-heavy", action="store_true", help="measure code/scene iteration without launching an importer")
    parser.add_argument("--prepare-only", action="store_true", help="create verified disposable inputs without launching Editor or Player")
    parser.add_argument("--prepared", type=Path, help="run an existing prepared.json; output/project/player options are ignored")
    parser.add_argument("--timeout", type=float, default=2100, help="per-launch timeout, including optional Unity reimport")
    args = parser.parse_args()
    if args.runs < 1 or not 0 < args.timeout < float("inf"):
        parser.error("--runs and --timeout must be positive")
    try:
        if args.prepared:
            prepared = json.loads(args.prepared.resolve().read_text())
        else:
            source = args.project.resolve()
            catalog_path = args.native_catalog
            if catalog_path is None:
                catalog_path = Path(json.loads((source / "Temp/UnityPlay/native-import/current.json").read_text())["catalog"])
            prepared = prepare(source, catalog_path.resolve(), args.player.resolve(), editor_path(source, args.unity),
                               args.output.resolve(), audio_path=args.audio, seed_library=not args.skip_heavy)
        if args.prepare_only:
            print(json.dumps({"success": True, "prepared": str(Path(prepared["output"]) / "prepared.json"),
                              "project": prepared["project"], "setupSeconds": prepared["setupSeconds"],
                              "editorStarted": False, "playerStarted": False}, indent=2))
            return 0
        summary = benchmark(prepared, runs=args.runs, skip_heavy=args.skip_heavy, timeout=args.timeout)
        print(json.dumps({"success": summary["success"], "summary": str(Path(prepared["output"]) / "summary.json"),
                          "warmMedianSeconds": summary["warmMedianSeconds"], "heavyMeasured": summary["heavyMeasured"]}, indent=2))
        return 0
    except (ToolError, OSError, ValueError, KeyError, subprocess.TimeoutExpired) as error:
        print(str(error), file=__import__("sys").stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
