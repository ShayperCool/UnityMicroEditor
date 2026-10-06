"""Incremental scene AssetBundles and atomic immutable-content publication."""
from __future__ import annotations

import datetime
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import uuid

from unity_play import BRIDGE, ToolError, require_idle, run_process, unity_version, write_json
from recompile import accept_content, content_dependency, digest, project_files

BUILD_METHOD = "Dungeon.CLIPlay.Editor.AssetBuild.BuildFromCommandLine"
SAFE_NAME = re.compile(r"^[A-Za-z0-9_.-]+$")
HASH = re.compile(r"^[a-f0-9]{64}$")


def current_manifest(player: Path) -> Path:
    return player.parent / "content/current.json"


def contained(folder: Path, value: str) -> Path:
    if not isinstance(value, str) or not value or Path(value).is_absolute():
        raise ToolError(f"Content path must be relative: {value}")
    path = (folder / value).resolve()
    if not path.is_relative_to(folder.resolve()):
        raise ToolError(f"Content path escapes its directory: {value}")
    return path


def check_file(path: Path, expected: str, size: int | None = None) -> None:
    if not isinstance(expected, str) or not HASH.fullmatch(expected) or not path.is_file():
        raise ToolError(f"Content file/hash is missing: {path}")
    if size is not None and (not isinstance(size, int) or size < 0 or path.stat().st_size != size):
        raise ToolError(f"Content size mismatch: {path}")
    if digest(path) != expected:
        raise ToolError(f"Content SHA256 mismatch: {path}")


def inspect_content(path: Path) -> dict:
    """Validate an immutable revision or its atomic current.json pointer."""
    path = path.resolve()
    value = json.loads(path.read_text())
    pointer = value if value.get("manifest") else None
    if pointer:
        if pointer.get("schemaVersion") != 1 or not HASH.fullmatch(pointer.get("revision", "")):
            raise ToolError("Unsupported content pointer schema/revision")
        path = contained(path.parent, pointer["manifest"])
        check_file(path, pointer.get("manifestSha256"))
        value = json.loads(path.read_text())
    if value.get("schemaVersion") != 1 or not value.get("success") or not value.get("ready"):
        raise ToolError("Content revision is not ready")
    if not HASH.fullmatch(value.get("revision", "")) or pointer and pointer["revision"] != value["revision"]:
        raise ToolError("Invalid content revision or pointer/revision mismatch")
    validate_bundles(value, path.parent)
    return {**value, "manifestPath": str(path)}


def validate_bundles(value: dict, folder: Path) -> None:
    bundles = value.get("bundles")
    if value.get("target") != "StandaloneLinux64" or not isinstance(bundles, list) or not bundles:
        raise ToolError("Content must contain Linux Standalone bundles")
    names = set()
    for bundle in bundles:
        name = bundle.get("name")
        if not isinstance(name, str) or not SAFE_NAME.fullmatch(name) or name in names:
            raise ToolError("Content bundle names must be distinct safe filenames")
        names.add(name)
        check_file(contained(folder, bundle.get("file")), bundle.get("sha256"), bundle.get("bytes"))
        crc = bundle.get("crc")
        if not isinstance(crc, int) or not 0 <= crc <= 0xFFFFFFFF:
            raise ToolError(f"Invalid bundle CRC: {name}")
    if value.get("sceneBundle") not in names:
        raise ToolError("Content scene bundle is missing")
    graph = {}
    for bundle in bundles:
        dependencies = bundle.get("dependencies", [])
        if not isinstance(dependencies, list) or any(name not in names for name in dependencies):
            raise ToolError(f"Unknown bundle dependency: {bundle['name']}")
        graph[bundle["name"]] = dependencies
    visited, stack = set(), set()
    def visit(name):
        if name in stack:
            raise ToolError("Content bundle dependencies contain a cycle")
        if name in visited:
            return
        stack.add(name)
        for dependency in graph[name]:
            visit(dependency)
        stack.remove(name)
        visited.add(name)
    for name in sorted(names):
        visit(name)


def compilation_assets(values: dict[str, str]) -> dict[str, str]:
    """Settings that can change the code/types baked into the existing Player."""
    return {path: sha256 for path, sha256 in values.items()
            if path.startswith("ProjectSettings/")
            or path in {"Packages/manifest.json", "Packages/packages-lock.json"}
            or Path(path).suffix.lower() in {".asmdef", ".asmref"}
            or path.lower().endswith((".asmdef.meta", ".asmref.meta"))}


def source_contract(project: Path, player: Path, *, during_export: bool = False) -> dict:
    manifest = player.parent / "code-refresh.json"
    if not player.is_file() or not manifest.is_file():
        raise ToolError("Prepare a baseline Player first, or use --prepare to cook content for a new shell")
    baseline_bytes = manifest.read_bytes()
    value = json.loads(baseline_bytes)
    if not isinstance(value, dict) or value.get("schema") != 1 or value.get("project") != str(project) or value.get("player") != str(player):
        raise ToolError("Player compiler baseline belongs to a different project/Player")
    if not isinstance(value.get("assets"), dict) or not isinstance(value.get("assemblies"), dict):
        raise ToolError("Player compiler baseline has no asset/assembly contract; rebuild its shell")
    sources, assets = project_files(project)
    if sources != value.get("sources"):
        if during_export:
            raise ToolError("C# source changed during content export; prior content is unchanged")
        raise ToolError("C# source changed since the Player baseline. Refresh compatible code edits or rebuild the shell; use --prepare only before shell builds.")
    settings = compilation_assets(assets)
    if settings != compilation_assets(value["assets"]):
        context = "during content export; prior content is unchanged" if during_export else "since the Player baseline; rebuild its shell"
        raise ToolError("Player compilation settings/assembly definitions/packages changed " + context)
    dlls = {}
    managed = player.parent / f"{player.stem}_Data/Managed"
    for name, record in value["assemblies"].items():
        target = managed / f"{name}.dll"
        if (not isinstance(name, str) or not SAFE_NAME.fullmatch(name)
                or not isinstance(record, dict) or record.get("dll") != str(target)):
            raise ToolError("Player compiler baseline points outside its Managed assembly contract")
        try:
            check_file(target, record.get("dllSha256"))
        except ToolError as error:
            raise ToolError(f"Player DLL no longer matches its compiler baseline: {error}") from error
        dlls[name] = record["dllSha256"]
    return {"sources": sources, "compilationAssets": settings, "playerDlls": dlls,
            "baselineSha256": hashlib.sha256(baseline_bytes).hexdigest()}


def export(project: Path, editor: Path, player: Path, scenes: list[str], *, dry_run: bool = False,
           timeout: float = 1800, prepare_only: bool = False, content_dir: Path | None = None) -> dict:
    """Cook bundles incrementally; publish only validated complete revisions.

    prepare_only cooks the first bundles before a shell build, allowing callers
    to pass typePreservationManifest to BuildPlayerOptions. It never changes
    current.json and does not assert compatibility with an existing Player.
    """
    project, editor, player = project.resolve(), editor.resolve(), player.resolve()
    require_idle(project)
    if not editor.is_file():
        raise ToolError(f"Unity Editor is missing: {editor}")
    if not scenes or len(scenes) != len(set(scenes)):
        raise ToolError("Supply at least one distinct scene asset path")
    normalized = []
    for scene in scenes:
        if Path(scene).is_absolute():
            raise ToolError("Content scenes must be project-relative Assets/*.unity paths")
        path = (project / scene).resolve()
        if not path.is_relative_to(project / "Assets") or not path.is_file() or path.suffix != ".unity":
            raise ToolError(f"Content scene must be an existing Assets/*.unity file: {scene}")
        normalized.append(path.relative_to(project).as_posix())
    if len(normalized) != len(set(normalized)):
        raise ToolError("Duplicate normalized scene paths")
    if not (BRIDGE / "Editor/AssetBuild.cs").is_file():
        raise ToolError("The scene content exporter bridge is not installed")
    contract = None if prepare_only else source_contract(project, player)
    graphics = "glcore"
    build_manifest = player.parent / "build.json"
    if build_manifest.is_file():
        build = json.loads(build_manifest.read_text())
        if not prepare_only and build.get("engineVersion") != unity_version(project):
            raise ToolError("Player Unity version differs from the project; rebuild its shell")
        graphics = build.get("graphicsApi", "glcore")
    if graphics not in {"glcore", "project"}:
        raise ToolError("Unsupported Player graphics API contract")
    key = hashlib.sha256(json.dumps({"scenes": sorted(normalized), "graphics": graphics}, sort_keys=True).encode()).hexdigest()[:16]
    # Unity rejects AssetBundle build output inside Library. Its own import and
    # shader caches remain in Library; this stable output retains .manifest files.
    cache = project / "Builds/CLIPlay/.asset-cache" / key
    output_manifest = cache / "asset-build.json"
    content = (content_dir or player.parent / "content").resolve()
    request = uuid.uuid4().hex[:12]
    logs = project / "Logs/UnityPlay" / f"content-{request}"
    environment = {"UNITY_CLI_CONTENT_OUTPUT": str(cache), "UNITY_CLI_CONTENT_MANIFEST": str(output_manifest),
                   "UNITY_CLI_CONTENT_SCENES": json.dumps(normalized), "UNITY_CLI_CONTENT_GRAPHICS": graphics}
    command = [str(editor), "-batchmode", "-quit", "-nographics", "-projectPath", str(project),
               "-buildTarget", "Linux64", "-executeMethod", BUILD_METHOD, "-logFile", str(logs / "editor.log")]
    if dry_run:
        return {"dryRun": True, "command": command, "environment": environment, "cache": str(cache),
                "contentDirectory": str(content), "prepareOnly": prepare_only, "rebuildsPlayer": False}
    lock_path = project / "Logs/UnityPlay/content-export.lock"
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    with lock_path.open("a+") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise ToolError("Another content export is running for this project") from error
        require_idle(project)
        cache.mkdir(parents=True, exist_ok=True)
        logs.mkdir(parents=True, exist_ok=True)
        output_manifest.unlink(missing_ok=True)
        from settings import preserve_player_settings
        with preserve_player_settings(project):
            launched = run_process(command, project, logs / "launch.log", timeout, {**os.environ, **environment})
        if launched["exitCode"] != 0 or not output_manifest.is_file():
            raise ToolError(f"AssetBundle export failed; prior content is unchanged. Editor log: {logs / 'editor.log'}")
        raw = json.loads(output_manifest.read_text())
        if raw.get("schemaVersion") != 1 or not raw.get("success"):
            raise ToolError(f"AssetBundle build reported failure; prior content is unchanged: {raw.get('failure', '')}")
        if raw.get("engineVersion") != unity_version(project) or raw.get("graphicsApi") != graphics or raw.get("scenes") != normalized:
            raise ToolError("AssetBundle manifest does not match the requested version/scenes/graphics API")
        if raw.get("scriptingBackend") != "Mono2x" or raw.get("development") is not True:
            raise ToolError("Asset content must match a Linux Mono development Player")
        validate_bundles(raw, cache)
        preservation = Path(raw.get("typePreservationManifest", "")).resolve()
        if not preservation.is_relative_to(cache) or not preservation.is_file() or preservation.suffix != ".manifest":
            raise ToolError("Unity's type-preservation .manifest is missing from its bundle cache")
        for dependency in raw.get("sourceDependencies", []):
            _, path = content_dependency(project, dependency)
            check_file(path, dependency.get("sha256"))
        if not prepare_only:
            if source_contract(project, player, during_export=True) != contract:
                raise ToolError("Player compiler baseline changed during content export; prior content is unchanged")
            managed = player.parent / f"{player.stem}_Data/Managed"
            for name in raw.get("requiredAssemblies", []):
                if not isinstance(name, str) or not SAFE_NAME.fullmatch(name) or not (managed / f"{name}.dll").is_file():
                    raise ToolError(f"Content requires an assembly absent from this Player: {name}. Rebuild its shell.")
        revision_input = {key: raw.get(key) for key in ("engineVersion", "target", "scriptingBackend", "development", "graphicsApi", "scenes", "sceneBundle", "bundles", "sourceDependencies", "requiredAssemblies")}
        revision_input["compatibilityVerified"] = not prepare_only
        revision = hashlib.sha256(json.dumps(revision_input, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
        final_manifest = {**raw, "revision": revision, "ready": True,
                          "createdUtc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                          "compatibilityVerified": not prepare_only}
        revisions = content / "revisions"
        revisions.mkdir(parents=True, exist_ok=True)
        destination = revisions / revision
        pending = revisions / f".pending-{request}"
        try:
            if destination.exists():
                # A revision is immutable. Reuse it only after checking its bytes.
                existing = inspect_content(destination / "manifest.json")
                if {key: existing.get(key) for key in revision_input} != revision_input:
                    raise ToolError("Existing content revision metadata differs; refusing to overwrite it")
            else:
                pending.mkdir(exist_ok=False)
                for bundle in raw["bundles"]:
                    source = contained(cache, bundle["file"])
                    target = contained(pending, bundle["file"])
                    target.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copyfile(source, target)
                    metadata = source.with_name(source.name + ".manifest")
                    if metadata.is_file():
                        shutil.copyfile(metadata, target.with_name(target.name + ".manifest"))
                shutil.copyfile(preservation, pending / "types.manifest")
                write_json(pending / "manifest.json", final_manifest)
                inspect_content(pending / "manifest.json")
                os.replace(pending, destination)
            immutable_manifest = destination / "manifest.json"
            pointer = {"schemaVersion": 1, "revision": revision,
                       "manifest": immutable_manifest.relative_to(content).as_posix(),
                       "manifestSha256": digest(immutable_manifest)}
            if not prepare_only:
                write_json(content / "current.json", pointer)
            acceptance = None
            if not prepare_only:
                try:
                    acceptance = accept_content(project, player, inspect_content(immutable_manifest))
                except (ToolError, OSError, ValueError, KeyError) as error:
                    # Content remains valid even when unrelated edits prevent
                    # safe advancement of the separate code-refresh baseline.
                    acceptance = {"accepted": False, "reason": str(error)}
            result = {**launched, "published": not prepare_only, "prepareOnly": prepare_only,
                      "revision": revision, "manifest": str(immutable_manifest),
                      "current": str(content / "current.json") if not prepare_only else None,
                      "typePreservationManifest": str(preservation), "scenes": normalized,
                      "editorLog": str(logs / "editor.log"), "editorExportSeconds": raw.get("elapsedSeconds"),
                      "rebuildsPlayer": False, "runtimeNeedsEditor": False,
                      "codeRefreshAssetChecks": acceptance,
                      "notes": "Restart the prepared Player to load this revision. New/stripped types/modules require a shell rebuild; Addressables are separate."}
            write_json(logs / "export.json", result)
            return result
        finally:
            if pending.exists():
                shutil.rmtree(pending)
