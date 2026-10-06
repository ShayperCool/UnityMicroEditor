"""Unity native asset cache for the raw scene loader.

Only tool-owned import projects are opened in the Editor. Authored scenes and
text prefabs supply dependency information, never native bundle contents.
"""
from __future__ import annotations

import contextlib
import datetime
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import time
import uuid

from unity_play import BRIDGE, TOOL_ROOT, ToolError, require_idle, run_process, unity_version, write_json

BUILD_METHOD = "Dungeon.CLIPlay.ImportedAssets.Editor.NativeAssetExporter.RunFromCommandLine"
OWNED_ASSET_FOLDER = "__UnityPlayImports"
HASH = re.compile(r"^[0-9a-f]{64}$")
GUID = re.compile(r"^[0-9a-f]{32}$")
EXPORT_ID = re.compile(r"^[0-9a-f]{32}$")
DONT_SAVE_IN_BUILD = 16  # UnityEngine.HideFlags.DontSaveInBuild in the native profile.
FILE_ID = re.compile(r"^-?[0-9]+$")
GUID_REFERENCES = re.compile(r"\bguid:\s*([0-9a-fA-F]{32})\b")
NATIVE_EXTENSIONS = {
    ".fbx", ".obj", ".dae", ".blend", ".3ds", ".max", ".ma", ".mb",
    ".wav", ".ogg", ".mp3", ".aif", ".aiff", ".flac", ".mixer", ".anim", ".controller", ".overridecontroller",
    ".mat", ".shader", ".shadergraph", ".shadersubgraph", ".compute", ".inputactions",
    ".png", ".jpg", ".jpeg", ".tga", ".psd", ".exr", ".hdr", ".tif", ".tiff", ".dds", ".bmp",
    ".rendertexture", ".cubemap", ".physicmaterial", ".physicsmaterial2d", ".mask",
    ".uxml", ".uss", ".ttf", ".otf", ".fontsettings", ".spriteatlas", ".spriteatlasv2",
}
TEXT_EXTENSIONS = {".unity", ".prefab", ".asset", ".mat", ".anim", ".controller", ".overridecontroller",
                   ".meta", ".inputactions", ".shadergraph", ".shadersubgraph", ".uxml", ".uss"}


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def _logical(value: str, *, scene: bool = False) -> str:
    if (not isinstance(value, str) or not value or "\\" in value or Path(value).is_absolute()
            or ".." in Path(value).parts or not value.startswith(("Assets/", "Packages/"))):
        raise ToolError(f"Native import path must be relative Assets/ or Packages/: {value}")
    if scene and (not value.startswith("Assets/") or Path(value).suffix.lower() != ".unity"):
        raise ToolError(f"Dependency scenes must be Assets/*.unity: {value}")
    if not scene and Path(value).suffix.lower() in {".unity", ".prefab"}:
        raise ToolError(f"Scenes and text prefabs belong to the raw scene loader: {value}")
    return Path(value).as_posix()


def _inventory(folder: Path) -> dict[str, str]:
    result = {}
    if not folder.is_dir():
        return result
    for current, directories, files in os.walk(folder):
        base = Path(current)
        for name in directories + files:
            if (base / name).is_symlink():
                raise ToolError(f"Native import cannot copy a writable source symlink: {base / name}")
        for name in files:
            path = base / name
            result[path.relative_to(folder).as_posix()] = sha256(path)
    return result


def source_inventory(project: Path) -> dict[str, str]:
    return {f"{folder}/{relative}": digest for folder in ("Assets", "Packages", "ProjectSettings")
            for relative, digest in _inventory(project / folder).items()}


def _sync_tree(source: Path, destination: Path, source_files: dict[str, str], *, reserved: str | None = None) -> None:
    """Reflink changed files and retain this isolated project's Library untouched."""
    destination.mkdir(parents=True, exist_ok=True)
    for relative, digest in source_files.items():
        if reserved and Path(relative).parts[0] == reserved:
            raise ToolError(f"Source Assets reserves a tool-owned importer directory: {relative}")
        origin, target = source / relative, destination / relative
        if target.is_symlink():
            raise ToolError(f"Isolated import destination contains a symlink: {target}")
        if target.is_file() and sha256(target) == digest:
            continue
        target.parent.mkdir(parents=True, exist_ok=True)
        copied = subprocess.run(["cp", "--reflink=auto", "--preserve=timestamps", "--", str(origin), str(target)],
                                capture_output=True, text=True, check=False)
        if copied.returncode:
            raise ToolError(f"Cannot copy native importer source {origin}: {copied.stderr.strip()}")
        if sha256(target) != digest:
            raise ToolError(f"Source changed while preparing native importer: {origin}")
    for path in sorted(destination.rglob("*"), reverse=True):
        relative = path.relative_to(destination).as_posix()
        if reserved and (Path(relative).parts[0] == reserved or relative == reserved + ".meta"):
            continue
        if path.is_symlink():
            raise ToolError(f"Isolated import destination contains a symlink: {path}")
        if path.is_file() and relative not in source_files:
            path.unlink()
        elif path.is_dir():
            with contextlib.suppress(OSError):
                path.rmdir()


def _settings_guids(project: Path) -> list[str]:
    values = set()
    for path in sorted((project / "ProjectSettings").glob("*.asset")):
        if path.name == "EditorBuildSettings.asset":
            continue
        values.update(value.lower() for value in GUID_REFERENCES.findall(path.read_text(errors="replace")))
    return sorted(values - {"0" * 32})


def _override_probes(project: Path, inventory: dict[str, str]) -> list[dict]:
    from raw_scene import RawSceneError, parse_unity_yaml
    probes = []
    for logical in sorted(inventory):
        if not logical.startswith("Assets/") or Path(logical).suffix.lower() != ".prefab":
            continue
        path = project / logical
        text = path.read_text(errors="replace")
        if ".Array.size" not in text:
            continue
        try:
            documents = parse_unity_yaml(path, text)
        except RawSceneError:
            continue
        for document in documents:
            if document["classId"] != 1001:
                continue
            modifications = document["data"].get("m_Modification", {}).get("m_Modifications", [])
            sizes = {}
            for item in modifications:
                target = item.get("target", {})
                property_path = item.get("propertyPath", "")
                if not property_path.endswith(".Array.size"):
                    continue
                try:
                    size = int(item.get("value"))
                except (TypeError, ValueError):
                    continue
                if size >= 0:
                    sizes[(target.get("guid", ""), str(target.get("fileID", "")), property_path[:-len(".Array.size")])] = size
            for item in modifications:
                target = item.get("target", {})
                property_path = item.get("propertyPath", "")
                for match in re.finditer(r"\.Array\.data\[(\d+)\]", property_path):
                    array_path = property_path[:match.start()]
                    size = sizes.get((target.get("guid", ""), str(target.get("fileID", "")), array_path))
                    if size is None or int(match.group(1)) < size:
                        continue
                    probes.append({"path": logical, "instanceFileId": document["fileId"], "targetGuid": target.get("guid", ""),
                                   "targetFileId": str(target.get("fileID", "")), "propertyPath": property_path,
                                   "arrayPath": array_path, "expectedArraySize": size})
                    break
    return probes


def _scene_lighting_signature(project: Path, scenes: list[str]) -> dict[str, str]:
    result = {}
    for logical in scenes:
        # A transform edit leaves the native lighting proof valid. A lightmap
        # index, baked texture or lighting settings reference changes that proof.
        fields = [line.strip() for line in (project / logical).read_text(errors="replace").splitlines()
                  if re.match(r"\s*m_(?:Lightmap\w*|LightingDataAsset|LightingSettings):", line)]
        result[logical] = hashlib.sha256("\n".join(fields).encode()).hexdigest()
    return result


def _prefab_identity_signature(path: Path) -> str:
    """Hash identity/structure, allowing scalar/transform edits to stay raw."""
    from raw_scene import parse_unity_yaml
    identities = []
    for document in parse_unity_yaml(path):
        data = document["data"]
        entry = {"fileId": document["fileId"], "classId": document["classId"], "stripped": document["stripped"]}
        for key in ("m_CorrespondingSourceObject", "m_PrefabInstance", "m_SourcePrefab", "m_GameObject", "m_Component", "m_Father", "m_Children", "m_Script"):
            if key in data:
                entry[key] = data[key]
        modifications = data.get("m_Modification", {})
        for key in ("m_RemovedComponents", "m_RemovedGameObjects", "m_AddedGameObjects", "m_AddedComponents"):
            if key in modifications:
                entry[key] = modifications[key]
        identities.append(entry)
    return hashlib.sha256(json.dumps(identities, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def _helper_hash() -> str:
    files = [(path.relative_to(TOOL_ROOT).as_posix(), sha256(path))
             for path in sorted((TOOL_ROOT / "ImportedAssets/Editor").rglob("*")) if path.is_file()]
    files.append(("imported_assets.py", sha256(Path(__file__))))
    return hashlib.sha256(json.dumps(files, separators=(",", ":")).encode()).hexdigest()


def _dependency_signature(project: Path, scenes: list[str], asset_paths: list[str], inventory: dict[str, str],
                          source_roots: list[dict] | None = None, native_asset_paths: set[str] | None = None) -> dict:
    """Follow authored GUIDs, excluding scene layout and game method bodies from the cook key.

    The Editor's exact dependencies are verified separately after the first
    export. This discovery signature catches changed/new raw graph references
    without cooking a scene, or launching Unity for scene transform edits.
    """
    roots = [("Assets", project / "Assets")]
    roots += [(item["path"], Path(item["source"])) for item in source_roots or []]
    by_guid = {}
    for logical_root, physical_root in roots:
        for meta in sorted(physical_root.rglob("*.meta")):
            matches = re.findall(r"^guid:\s*([0-9a-fA-F]{32})\s*$", meta.read_text(errors="replace"), re.MULTILINE)
            if len(matches) != 1:
                continue
            path = meta.with_suffix("")
            if not path.is_file():
                continue
            logical = logical_root + "/" + path.relative_to(physical_root).as_posix()
            by_guid[matches[0].lower()] = (logical, path)
    pending = [(scene, project / scene) for scene in scenes]
    pending += [(logical, project / logical) for logical in asset_paths if logical.startswith("Assets/")]
    pending += [by_guid[guid] for guid in _settings_guids(project) if guid in by_guid]
    visited, native_inputs, unknown, prefab_identities = set(), {}, set(), {}
    while pending:
        logical, path = pending.pop()
        if logical in visited or not path.is_file():
            continue
        visited.add(logical)
        suffix = path.suffix.lower()
        if suffix == ".prefab":
            prefab_identities[logical] = _prefab_identity_signature(path)
        if suffix in NATIVE_EXTENSIONS or suffix == ".asset" and logical in (native_asset_paths or set()):
            # URP and other native ScriptableObject types are known after export.
            # Changing ordinary raw data assets must not trigger a native cook.
            native_inputs[logical] = sha256(path)
            if path.with_name(path.name + ".meta").is_file():
                native_inputs[logical + ".meta"] = sha256(path.with_name(path.name + ".meta"))
        references = set()
        if suffix in TEXT_EXTENSIONS and path.stat().st_size <= 32 * 1024 * 1024:
            references.update(value.lower() for value in GUID_REFERENCES.findall(path.read_text(errors="replace")))
        meta = path.with_name(path.name + ".meta")
        if meta.is_file():
            references.update(value.lower() for value in GUID_REFERENCES.findall(meta.read_text(errors="replace")))
        for guid in references:
            if guid.startswith("0000000000000000"):
                continue
            if guid in by_guid:
                pending.append(by_guid[guid])
            else:
                unknown.add(guid)
    configuration = {path: digest for path, digest in inventory.items()
                     if path.startswith("Packages/") or path.startswith("ProjectSettings/") and not path.endswith("EditorBuildSettings.asset")
                     or Path(path).suffix.lower() in {".asmdef", ".asmref"}
                     or (path.endswith(".cs") and ("/Editor/" in path or _is_importer_script(project / path)))}
    return {"nativeInputs": dict(sorted(native_inputs.items())), "unknownGuids": sorted(unknown),
            "configuration": configuration, "explicitAssets": sorted(asset_paths), "engineVersion": unity_version(project),
            "helperHash": _helper_hash(), "graphicsApi": "glcore", "sceneLighting": _scene_lighting_signature(project, scenes),
            "overrideProbes": _override_probes(project, inventory), "prefabIdentities": dict(sorted(prefab_identities.items()))}


def _is_importer_script(path: Path) -> bool:
    contents = path.read_text(errors="replace")
    return "AssetPostprocessor" in contents or "ScriptedImporter" in contents


def _cached_package_resolutions(project: Path, import_project: Path) -> list[tuple[dict, dict]]:
    resolutions = []
    snapshots_path = import_project / ".unity-play-package-snapshots.json"
    if snapshots_path.is_file():
        snapshots = json.loads(snapshots_path.read_text())
        original = snapshots.get("originalPackages", {})
        matches = snapshots.get("schemaVersion") == 1 and all(
            (project / "Packages" / name).is_file() and original.get(name) == sha256(project / "Packages" / name)
            for name in ("manifest.json", "packages-lock.json"))
        if matches:
            entries, locked = {}, {}
            for record in snapshots.get("snapshots", []):
                name, package_id, source = record.get("name"), record.get("packageId"), Path(record.get("source", ""))
                if not isinstance(name, str) or not isinstance(package_id, str) or not source.is_absolute() or not source.is_dir():
                    continue
                inventory = _inventory(source)
                digest = hashlib.sha256(json.dumps(inventory, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
                if digest != record.get("sha256"):
                    raise ToolError(f"Owned native package snapshot checksum differs: {source}")
                package_source = record.get("packageSource")
                entries[package_id] = {"name": name, "source": package_source, "resolvedPath": str(source)}
                locked[name] = {"source": package_source, "version": package_id[len(name) + 1:], "hash": record.get("gitHash")}
            resolutions.append((entries, locked))
    for owner in (import_project, project):
        path = owner / "Library/PackageManager/projectResolution.json"
        lock_path = owner / "Packages/packages-lock.json"
        if path.is_file() and lock_path.is_file():
            value = json.loads(path.read_text())
            locked = json.loads(lock_path.read_text()).get("dependencies", {})
            resolutions.append((value.get("outputs", {}), locked))
    return resolutions


def _embed_cached_packages(project: Path, import_project: Path, cache: Path,
                           resolutions: list[tuple[dict, dict]]) -> dict:
    """Use proven locked package snapshots without changing the source manifest.

    This is optional. A missing exact registry version or Git commit leaves the
    mirrored manifest intact, allowing the normal UPM resolution path.
    """
    manifest_path = project / "Packages/manifest.json"
    lock_path = project / "Packages/packages-lock.json"
    if not manifest_path.is_file() or not lock_path.is_file():
        return {"enabled": False, "reason": "No complete source package lock"}
    manifest = json.loads(manifest_path.read_text())
    locked = json.loads(lock_path.read_text()).get("dependencies", {})
    needed = {name: value for name, value in locked.items() if value.get("source") not in {"builtin", "embedded"}}
    if not needed:
        return {"enabled": False, "reason": "No registry or Git packages require a snapshot"}
    selected = {}
    missing = []
    for name, requirement in needed.items():
        package_id = name + "@" + requirement.get("version", "")
        for resolution, resolved_lock in resolutions:
            record = resolution.get(package_id)
            if not isinstance(record, dict) or record.get("name") != name or record.get("source") != requirement.get("source"):
                continue
            root = Path(record.get("resolvedPath", ""))
            if not root.is_absolute() or root.is_symlink() or not (root / "package.json").is_file():
                continue
            package = json.loads((root / "package.json").read_text())
            if package.get("name") != name:
                continue
            if requirement.get("source") == "registry" and package.get("version") != requirement.get("version"):
                continue
            if requirement.get("source") == "git":
                previous = resolved_lock.get(name, {})
                if (not requirement.get("hash") or previous.get("hash") != requirement["hash"]
                        or previous.get("version") != requirement.get("version") or previous.get("source") != "git"):
                    continue
            # Local packages keep their authored resolution. Their external
            # directory is not part of this source snapshot and is not guessed.
            if requirement.get("source") not in {"registry", "git"}:
                continue
            selected[name] = (package_id, requirement, root)
            break
        if name not in selected:
            missing.append(name)
    if missing:
        return {"enabled": False, "reason": "Exact resolved package sources are unavailable", "missing": sorted(missing)}
    destination = cache / "PackageSources"
    dependencies = dict(manifest.get("dependencies", {}))
    provenance = destination / ".unity-play-package-resolution.json"
    records = json.loads(provenance.read_text()).get("packages", {}) if provenance.is_file() else {}
    snapshots = []
    for name, (package_id, requirement, source) in sorted(selected.items()):
        inventory = _inventory(source)
        fingerprint = hashlib.sha256(json.dumps(inventory, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
        target = destination / (name + "@" + fingerprint)
        if target.exists():
            if _inventory(target) != inventory:
                raise ToolError(f"Owned native package snapshot differs from its immutable content: {target}")
        else:
            temporary = destination / (".pending-" + uuid.uuid4().hex)
            try:
                _sync_tree(source, temporary, inventory)
                if _inventory(source) != inventory:
                    raise ToolError(f"Package source changed while preparing native importer: {source}")
                os.replace(temporary, target)
            finally:
                if temporary.exists():
                    shutil.rmtree(temporary)
        dependencies[name] = "file:" + str(target)
        records[name] = {"packageId": package_id, "root": target.name, "gitHash": requirement.get("hash")}
        snapshots.append({"name": name, "packageId": package_id, "source": str(target), "sha256": fingerprint,
                          "originalSource": str(source), "packageSource": requirement.get("source"), "gitHash": requirement.get("hash")})
    write_json(provenance, {"schemaVersion": 1, "packages": records})
    # Only the tool-owned mirror receives these file: overrides. The original
    # manifest and lock remain part of sourceInventory for publication checks.
    write_json(import_project / "Packages/manifest.json", {**manifest, "dependencies": dependencies})
    original = {name: sha256(project / "Packages" / name) for name in ("manifest.json", "packages-lock.json")}
    write_json(import_project / ".unity-play-package-snapshots.json", {"schemaVersion": 1, "originalPackages": original,
                                                                    "snapshots": snapshots})
    return {"enabled": True, "originalPackages": original, "snapshots": snapshots}


def prepare_import(project: Path, editor: Path, scenes: list[str], *, cache_dir: Path | None = None,
                   asset_paths: list[str] | None = None, seed_library: bool = False) -> dict:
    """Synchronize a separate import project and return its exact Editor invocation."""
    project, editor = project.resolve(), editor.resolve()
    scenes = sorted(set(_logical(value, scene=True) for value in scenes))
    explicit = sorted(set(_logical(value) for value in asset_paths or []))
    if not scenes and not explicit:
        raise ToolError("Specify dependency scenes or native assets to import")
    if not editor.is_file():
        raise ToolError(f"Unity Editor is missing: {editor}")
    for logical in scenes:
        if not (project / logical).is_file():
            raise ToolError(f"Native import dependency scene is missing: {logical}")
    version = unity_version(project)
    cache = (cache_dir or project / "Temp/UnityPlay/native-import").resolve()
    import_project = cache / "ImportProject"
    if import_project == project or import_project.is_relative_to(project / "Assets"):
        raise ToolError("Native importer must use a separate tool-owned Unity project")
    require_idle(import_project)
    inventory = source_inventory(project)
    if any(path.startswith("Assets/" + OWNED_ASSET_FOLDER + "/") or path == "Assets/" + OWNED_ASSET_FOLDER + ".meta" for path in inventory):
        raise ToolError("Source project already contains the reserved native importer asset directory")
    import_project.mkdir(parents=True, exist_ok=True)
    owner = import_project / ".unity-play-native-import-owner.json"
    seeded = False
    if owner.exists():
        value = json.loads(owner.read_text())
        if value.get("schemaVersion") != 1 or value.get("project") != str(project):
            raise ToolError("Isolated native importer belongs to another source project")
    elif any(import_project.iterdir()):
        raise ToolError("Refusing to synchronize an existing project not owned by the native importer")
    write_json(owner, {"schemaVersion": 1, "project": str(project)})
    # Read the old resolved lock before replacing this mirror's authored
    # Packages. A stale Git checkout must never inherit a newly requested hash.
    package_resolutions = _cached_package_resolutions(project, import_project)
    if seed_library and (project / "Library").is_dir() and not (import_project / "Library").exists():
        # A source Editor could be writing the ArtifactDB. Cloning it is allowed
        # only while the source project is idle, using this filesystem's reflink.
        require_idle(project)
        if (project / "Library").is_symlink():
            raise ToolError("Cannot seed native importer from a symlinked source Library")
        copied = subprocess.run(["cp", "-a", "--reflink=always", "--", str(project / "Library"), str(import_project / "Library")],
                                capture_output=True, text=True, check=False)
        if copied.returncode:
            raise ToolError(f"Cannot reflink source Library into native import cache: {copied.stderr.strip()}")
        seeded = True
        write_json(import_project / ".unity-play-library-seed.json", {"schemaVersion": 1, "sourceProject": str(project),
                   "sourceLibrary": str(project / "Library"), "engineVersion": version, "copyMode": "xfs-reflink"})
    for folder in ("Assets", "Packages", "ProjectSettings"):
        _sync_tree(project / folder, import_project / folder,
                   {path[len(folder) + 1:]: digest for path, digest in inventory.items() if path.startswith(folder + "/")},
                   reserved=OWNED_ASSET_FOLDER if folder == "Assets" else None)
    package_snapshots = _embed_cached_packages(project, import_project, cache, package_resolutions)
    helper = import_project / "Assets" / OWNED_ASSET_FOLDER / "Editor"
    helper.mkdir(parents=True, exist_ok=True)
    for source in sorted((TOOL_ROOT / "ImportedAssets/Editor").glob("*")):
        if source.is_file():
            shutil.copy2(source, helper / source.name)
    if source_inventory(project) != inventory:
        raise ToolError("Authored source changed during native importer synchronization")
    output = import_project / "Builds/UnityPlayNative/cache"
    export_id = uuid.uuid4().hex
    attempt = cache / "Attempts" / export_id
    request = attempt / "request.json"
    native_catalog = attempt / "export-catalog.json"
    write_json(request, {"schemaVersion": 1, "exportId": export_id, "scenes": scenes, "assetPaths": explicit,
                         "settingsGuids": _settings_guids(project), "outputDirectory": str(output),
                         "catalogPath": str(native_catalog), "graphicsApi": "glcore",
                         "overrideProbes": _override_probes(project, inventory),
                         "sceneLighting": _scene_lighting_signature(project, scenes)})
    log_root = cache / "Logs" / export_id[:12]
    command = [str(editor), "-batchmode", "-nographics", "-projectPath", str(import_project),
               "-buildTarget", "Linux64", "-executeMethod", BUILD_METHOD, "-logFile", str(log_root / "editor.log")]
    prepared = {"schemaVersion": 1, "exportId": export_id, "project": str(project), "importProject": str(import_project), "cacheDirectory": str(cache),
            "engineVersion": version, "request": str(request), "exportCatalog": str(native_catalog),
            "requestSha256": sha256(request), "preparedManifest": str(attempt / "prepared.json"),
            "command": command, "environment": {"UNITY_PLAY_NATIVE_IMPORT_REQUEST": str(request)},
            "editorLog": str(log_root / "editor.log"), "launchLog": str(log_root / "launch.log"),
            "sourceInventory": inventory, "scenes": scenes, "assetPaths": explicit, "runtimeNeedsEditor": False,
            "containsScenes": False, "sceneLoader": "raw", "librarySeeded": seeded,
            "helperHash": _helper_hash(), "packageSnapshots": package_snapshots}
    # Save the original snapshot before Editor can mutate its own mirror. An
    # interrupted CLI can later prove which exact request produced the result.
    write_json(Path(prepared["preparedManifest"]), prepared)
    write_json(cache / "pending.json", {"schemaVersion": 1, "exportId": export_id,
                                      "preparedManifest": prepared["preparedManifest"],
                                      "preparedSha256": sha256(Path(prepared["preparedManifest"]))})
    return prepared


def _file(path: Path, expected: str, size: int | None = None) -> None:
    if not isinstance(expected, str) or not HASH.fullmatch(expected) or not path.is_file():
        raise ToolError(f"Native asset file/hash is missing: {path}")
    if size is not None and (not isinstance(size, int) or isinstance(size, bool) or size < 0 or path.stat().st_size != size):
        raise ToolError(f"Native asset file size differs: {path}")
    if sha256(path) != expected:
        raise ToolError(f"Native asset SHA256 mismatch: {path}")


def inspect_catalog(path: Path, *, check_sources: bool = True) -> dict:
    path = path.resolve()
    try:
        value = json.loads(path.read_text())
    except (OSError, ValueError) as error:
        raise ToolError(f"Native imported-asset catalog is unreadable: {path}") from error
    if (value.get("schemaVersion") != 1 or value.get("success") is not True
            or value.get("target") != "StandaloneLinux64" or value.get("containsScenes") is not False
            or value.get("sceneLoader") != "raw" or value.get("scriptingBackend") != "Mono2x"
            or value.get("graphicsApi") != "glcore"):
        raise ToolError(f"Native imported-asset catalog has an invalid runtime/schema contract: {value.get('failure', '')}")
    packs = value.get("packs")
    if not isinstance(packs, list) or not packs:
        raise ToolError("Native imported-asset catalog contains no asset packs")
    pack_paths = set()
    for pack in packs:
        bundle = Path(pack.get("bundlePath", ""))
        if not bundle.is_absolute():
            bundle = (path.parent / bundle).resolve()
        if str(bundle) in pack_paths:
            raise ToolError("Native imported-asset pack path is duplicated")
        pack_paths.add(str(bundle))
        _file(bundle, pack.get("sha256"), pack.get("bytes"))
        crc = pack.get("crc")
        if not isinstance(crc, int) or isinstance(crc, bool) or not 0 <= crc <= 0xFFFFFFFF:
            raise ToolError("Native imported-asset pack CRC is invalid")
        registry = pack.get("registryAsset")
        if not isinstance(registry, str) or not registry.lower().startswith("assets/__unityplayimports/") or not registry.lower().endswith(".asset"):
            raise ToolError("Native pack registry is not a tool-owned carrier asset")
    records = value.get("assets")
    if not isinstance(records, list) or not records:
        raise ToolError("Native imported-asset catalog has no native object records")
    keys = set()
    identities = {}
    for record in records:
        guid, local_id = record.get("guid"), record.get("fileId")
        if (not isinstance(guid, str) or not GUID.fullmatch(guid) or not isinstance(local_id, str)
                or not FILE_ID.fullmatch(local_id) or local_id == "0" or record.get("kind") != "imported"
                or not isinstance(record.get("typeName"), str) or not record["typeName"]):
            raise ToolError("Native asset object identity/type is invalid")
        key = guid + ":" + local_id
        if key in keys:
            raise ToolError(f"Native asset object identity is duplicated: {key}")
        keys.add(key)
        identities[key] = record
        _logical(record.get("path", ""))
    excluded_keys = set()
    cooked_omissions = 0
    for excluded in value.get("excludedRuntimeAssets", []):
        key = _runtime_observation(excluded)
        flags = excluded["hideFlags"]
        if key in keys or key in excluded_keys:
            raise ToolError("Native build-excluded asset lacks exact hide-flags proof or duplicates a runtime identity")
        reason = excluded.get("reason")
        if reason == "HideFlags.DontSaveInBuild":
            if not flags & DONT_SAVE_IN_BUILD:
                raise ToolError("Native build-excluded asset lacks exact hide-flags proof")
        elif reason == "nativeCookedOmission":
            _cooked_omission_proof(excluded.get("cookedProof"), value.get("engineVersion"))
            cooked_omissions += 1
        else:
            raise ToolError("Native build-excluded asset lacks exact hide-flags proof or cooked-null proof")
        excluded_keys.add(key)
    observed_clips = set()
    for clip in value.get("modelAnimationClips", []):
        key = _runtime_observation(clip)
        if (key in observed_clips or clip["typeName"] != "UnityEngine.AnimationClip"
                or key not in keys | excluded_keys or clip["dontSaveInBuild"] and key not in excluded_keys):
            raise ToolError("Native model AnimationClip observation has no matching runtime/excluded identity")
        observed_clips.add(key)
    if cooked_omissions:
        passes = value.get("nativeBuildPasses", [])
        if (value.get("cookedRegistryVerified") is not True or len(passes) != 2
                or passes[0].get("cookedNullEntries") != cooked_omissions
                or passes[1].get("cookedNullEntries") != 0
                or passes[1].get("registryEntries") != len(records)):
            raise ToolError("Native cooked omissions require a verified rebuilt registry without null entries")
    global_keys, global_addresses = set(), set()
    for settings in value.get("globalSettingsAssets", []):
        key = settings.get("guid", "") + ":" + settings.get("fileId", "")
        asset_name = settings.get("assetName")
        if (key not in identities or key in global_keys or settings.get("path") != identities[key]["path"]
                or not isinstance(asset_name, str) or asset_name != settings["path"].lower()
                or not asset_name.endswith(".asset") or asset_name in global_addresses):
            raise ToolError("Native pipeline global-settings identity/address is invalid or duplicated")
        if not any(asset_name in pack.get("addressableAssets", []) for pack in packs):
            raise ToolError("Native pipeline global settings are not an explicit addressable pack asset")
        global_keys.add(key)
        global_addresses.add(asset_name)
    for model in value.get("modelPrefabs", []):
        if not GUID.fullmatch(model.get("guid", "")) or not isinstance(model.get("objects"), list) or not model["objects"]:
            raise ToolError("Native model prefab graph is invalid")
        identifiers = set()
        for obj in model["objects"]:
            local_id = obj.get("fileId")
            if not isinstance(local_id, str) or not FILE_ID.fullmatch(local_id) or local_id == "0" or local_id in identifiers:
                raise ToolError("Native model prefab object identifier is invalid/duplicated")
            identifiers.add(local_id)
            if model["guid"] + ":" + local_id not in keys or not isinstance(obj.get("data"), dict):
                raise ToolError("Native model prefab object has no matching registry entry/data")
        if model.get("rootFileId") not in identifiers:
            raise ToolError("Native model prefab root is missing")
    binding_keys = set()
    for binding in value.get("deferredBindings", []):
        owner = binding.get("ownerGuid", "") + ":" + binding.get("ownerFileId", "")
        target_guid, target_id, property_path = binding.get("targetGuid"), binding.get("targetFileId"), binding.get("propertyPath")
        if (owner not in keys or not isinstance(target_guid, str) or not GUID.fullmatch(target_guid)
                or not isinstance(target_id, str) or not FILE_ID.fullmatch(target_id) or target_id == "0"
                or not isinstance(property_path, str) or not property_path):
            raise ToolError("Deferred raw prefab binding identity/property is invalid")
        if (owner, property_path) in binding_keys:
            raise ToolError("Deferred raw prefab binding property is duplicated")
        binding_keys.add((owner, property_path))
    if check_sources:
        for source in value.get("sourceDependencies", []):
            _file(Path(source.get("physicalPath", "")), source.get("sha256"), source.get("bytes"))
    return {**value, "catalogPath": str(path)}


def _runtime_observation(value: dict) -> str:
    guid, local_id = value.get("guid"), value.get("fileId")
    flags = value.get("hideFlags")
    if (not isinstance(guid, str) or not GUID.fullmatch(guid) or not isinstance(local_id, str)
            or not FILE_ID.fullmatch(local_id) or local_id == "0" or not isinstance(flags, int)
            or isinstance(flags, bool) or flags < 0 or not isinstance(value.get("typeName"), str)
            or not value["typeName"] or not isinstance(value.get("name"), str)
            or value.get("dontSaveInBuild") is not bool(flags & DONT_SAVE_IN_BUILD)):
        raise ToolError("Native runtime asset hide-flags observation is invalid")
    _logical(value.get("path", ""))
    return guid + ":" + local_id


def _cooked_omission_proof(value: dict | None, engine_version: str) -> None:
    if not isinstance(value, dict):
        raise ToolError("Native cooked omission is missing its public registry readback proof")
    crc = value.get("bundleCrc")
    if (value.get("method") != "AssetBundle.LoadFromFile/LoadAsset<ImportedAssetRegistry>"
            or value.get("target") != "StandaloneLinux64" or value.get("engineVersion") != engine_version
            or not isinstance(value.get("bundleSha256"), str) or not HASH.fullmatch(value["bundleSha256"])
            or not isinstance(crc, int) or isinstance(crc, bool) or not 0 <= crc <= 0xFFFFFFFF
            or value.get("registryAsset") != "assets/__unityplayimports/registry.asset"
            or value.get("nativeNullObserved") is not True or value.get("sourceTypeAvailable") is not True
            or value.get("retainedTypesVerified") is not True):
        raise ToolError("Native cooked omission has an invalid public registry readback proof")


def publish_import(prepared: dict) -> dict:
    """Validate an externally run Editor export and publish an immutable catalog."""
    project, cache = Path(prepared["project"]), Path(prepared["cacheDirectory"])
    if source_inventory(project) != prepared["sourceInventory"]:
        raise ToolError("Authored sources changed during native export; previous native revision is unchanged")
    if prepared.get("helperHash") != _helper_hash():
        raise ToolError("Native importer implementation changed during export; previous native revision is unchanged")
    export_id = prepared.get("exportId", "")
    if not isinstance(export_id, str) or not EXPORT_ID.fullmatch(export_id):
        raise ToolError("Native export preparation has no verifiable completion identity")
    _file(Path(prepared["request"]), prepared.get("requestSha256"))
    raw = inspect_catalog(Path(prepared["exportCatalog"]))
    if raw.get("exportId") != export_id or raw.get("requestSha256") != prepared["requestSha256"]:
        raise ToolError("Native export completion does not match the prepared request")
    if raw.get("engineVersion") != prepared["engineVersion"]:
        raise ToolError("Native import engine version differs from requested project")
    # Replace temporary mirror paths with source assets for exact future cache checks.
    dependencies = []
    for source in raw.get("sourceDependencies", []):
        logical = source["path"]
        if logical.startswith("Assets/"):
            source = {**source, "physicalPath": str(project / logical)}
        dependencies.append(source)
    for source in dependencies:
        _file(Path(source["physicalPath"]), source["sha256"], source.get("bytes"))
    signature = _dependency_signature(project, prepared["scenes"], prepared["assetPaths"], prepared["sourceInventory"],
                                      raw.get("sourceRoots", []), {record["path"] for record in raw["assets"]})
    revision_data = {key: raw.get(key) for key in ("engineVersion", "target", "graphicsApi", "assets", "modelPrefabs", "deferredBindings",
                                                 "nativeSceneMetadata", "ignoredPrefabOverrides", "prefabObservations", "curveWrapModes", "preloadedAssets", "globalSettingsAssets", "excludedRuntimeAssets", "modelAnimationClips", "nativeBuildPasses", "cookedRegistryVerified", "packs", "requiredAssemblies")}
    revision_data["sourceDependencies"] = dependencies
    revision_data["discoverySignature"] = signature
    revision = hashlib.sha256(json.dumps(revision_data, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
    revisions = cache / "revisions"
    destination = revisions / revision
    temporary = revisions / (".pending-" + uuid.uuid4().hex[:12])
    try:
        if not destination.exists():
            temporary.mkdir(parents=True)
            packs = []
            for index, pack in enumerate(raw["packs"]):
                source = Path(pack["bundlePath"])
                filename = f"{index:03d}-{source.name}"
                shutil.copyfile(source, temporary / filename)
                _file(temporary / filename, pack["sha256"], pack["bytes"])
                packs.append({**pack, "bundlePath": str(destination / filename)})
            final = {**raw, "packs": packs, "sourceDependencies": dependencies, "revision": revision,
                     "discoverySignature": signature, "createdUtc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                     "importProject": prepared["importProject"], "project": str(project)}
            final.pop("catalogPath", None)
            write_json(temporary / "catalog.json", final)
            os.replace(temporary, destination)
        catalog = inspect_catalog(destination / "catalog.json")
        write_json(cache / "current.json", {"schemaVersion": 1, "revision": revision,
                                            "catalog": str(destination / "catalog.json"),
                                            "catalogSha256": sha256(destination / "catalog.json")})
        return {**catalog, "cacheHit": False, "importProject": prepared["importProject"], "editorLog": prepared["editorLog"]}
    finally:
        if temporary.exists():
            shutil.rmtree(temporary)


def recover_import(cache_dir: Path) -> dict:
    """Publish a completed interrupted attempt using its persisted source snapshot.

    Recovery never synchronizes the import project, alters a request, or launches
    Editor. An older result without persisted preparation cannot be recovered.
    """
    cache = cache_dir.resolve()
    try:
        lock_stream = (cache / "export.lock").open("a+")
    except OSError as error:
        raise ToolError(f"Native import recovery cache is unavailable: {cache}") from error
    with lock_stream as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise ToolError("Another native asset export is already running") from error
        try:
            pointer = json.loads((cache / "pending.json").read_text())
            export_id = pointer.get("exportId", "")
            if pointer.get("schemaVersion") != 1 or not isinstance(export_id, str) or not EXPORT_ID.fullmatch(export_id):
                raise ToolError("Native import recovery has an invalid preparation pointer")
            attempt = cache / "Attempts" / export_id
            manifest = Path(pointer.get("preparedManifest", "")).resolve()
            if manifest != attempt / "prepared.json":
                raise ToolError("Native import recovery preparation escapes its attempt")
            _file(manifest, pointer.get("preparedSha256"))
            prepared = json.loads(manifest.read_text())
            if (prepared.get("schemaVersion") != 1 or prepared.get("exportId") != export_id
                    or prepared.get("cacheDirectory") != str(cache)
                    or prepared.get("preparedManifest") != str(manifest)
                    or prepared.get("request") != str(attempt / "request.json")
                    or prepared.get("exportCatalog") != str(attempt / "export-catalog.json")
                    or prepared.get("importProject") != str(cache / "ImportProject")):
                raise ToolError("Native import recovery preparation has an invalid attempt contract")
            require_idle(Path(prepared["importProject"]))
            result = publish_import(prepared)
            return {**result, "recovered": True, "runtimeNeedsEditor": False}
        except (OSError, ValueError, KeyError, TypeError) as error:
            raise ToolError(f"Native import recovery preparation is incomplete: {cache}") from error


def _cached(project: Path, scenes: list[str], asset_paths: list[str], cache: Path) -> dict | None:
    pointer = cache / "current.json"
    if not pointer.is_file():
        return None
    try:
        value = json.loads(pointer.read_text())
        if value.get("schemaVersion") != 1 or not HASH.fullmatch(value.get("revision", "")):
            return None
        catalog_path = Path(value["catalog"])
        if not catalog_path.resolve().is_relative_to(cache / "revisions" / value["revision"]):
            raise ToolError("Native import current pointer escapes its immutable revision")
        _file(catalog_path, value["catalogSha256"])
        catalog = inspect_catalog(catalog_path)
        if catalog.get("project") != str(project) or catalog.get("revision") != value["revision"]:
            return None
        signature = _dependency_signature(project, scenes, asset_paths, source_inventory(project), catalog.get("sourceRoots", []),
                                          {record["path"] for record in catalog["assets"]})
        if signature != catalog.get("discoverySignature"):
            return None
        return {**catalog, "cacheHit": True, "runtimeNeedsEditor": False, "editorExportSeconds": 0}
    except (ToolError, OSError, ValueError, KeyError):
        return None


def export_imported_assets(project: Path, editor: Path, scenes: list[str], *, cache_dir: Path | None = None,
                           asset_paths: list[str] | None = None, timeout: float = 1800,
                           dry_run: bool = False, seed_library: bool = False) -> dict:
    """Reuse valid native content, otherwise run Unity only in an isolated cache project."""
    project, editor = project.resolve(), editor.resolve()
    scenes = sorted(set(_logical(value, scene=True) for value in scenes))
    explicit = sorted(set(_logical(value) for value in asset_paths or []))
    cache = (cache_dir or project / "Temp/UnityPlay/native-import").resolve()
    cache.mkdir(parents=True, exist_ok=True)
    with (cache / "export.lock").open("a+") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise ToolError("Another native asset export is already running") from error
        hit = _cached(project, scenes, explicit, cache)
        if hit is not None:
            return hit
        prepared = prepare_import(project, editor, scenes, cache_dir=cache, asset_paths=explicit, seed_library=seed_library)
        if dry_run:
            return {**prepared, "dryRun": True}
        Path(prepared["editorLog"]).parent.mkdir(parents=True, exist_ok=True)
        Path(prepared["exportCatalog"]).unlink(missing_ok=True)
        started = time.monotonic()
        launched = run_process(prepared["command"], Path(prepared["importProject"]), Path(prepared["launchLog"]), timeout,
                               {**os.environ, **prepared["environment"]})
        if launched["exitCode"] or not Path(prepared["exportCatalog"]).is_file():
            raise ToolError(f"Native asset import failed; previous content is unchanged. Editor log: {prepared['editorLog']}")
        published = publish_import(prepared)
        return {**published, "wallSeconds": time.monotonic() - started, "editorExportSeconds": published.get("elapsedSeconds")}
