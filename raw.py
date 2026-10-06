"""Compile and snapshot raw input for the reusable host, without Unity Editor."""
from __future__ import annotations

import contextlib
import copy
import os
import hashlib
import json
from pathlib import Path
import re
import shutil
import time
import uuid


def _attach_lifecycle_protocol(manifest: dict, compiler: dict) -> None:
    from unity_play import ToolError

    manifest["lifecycleBarrierVersion"] = 0
    manifest["lifecycleBarrierAssemblies"] = []
    if compiler.get("capabilities", {}).get("lifecycleBarrier") is not True:
        return
    phases = {"Awake", "OnEnable", "OnDisable", "OnDestroy"}
    assemblies = compiler["assemblies"]
    paths = [item["path"] for item in assemblies]
    if len(set(paths)) != len(paths):
        raise ToolError("Lifecycle verification contains duplicate assembly paths.")
    for item in assemblies:
        proof = item.get("lifecycleBarrier", {})
        if (proof.get("schemaVersion") != 1 or proof.get("verified") is not True
                or set(proof.get("phases", [])) != phases):
            raise ToolError(f"Incomplete lifecycle verification for assembly: {item['path']}")
    manifest["lifecycleBarrierVersion"] = 1
    manifest["lifecycleBarrierAssemblies"] = paths


def prepare_raw(project: Path, unity: Path, host: Path, scene: str, output: Path, *, native_import: bool = False,
                native_catalog: Path | dict | None = None, native_cache: Path | None = None,
                previous_build: dict | None = None, changed_paths=None, session_cache=None) -> dict:
    from raw_compile import RawCompileError, compile_project
    from raw_scene import RawSceneError, prepare_scene
    from unity_play import ToolError, unity_version, write_json
    from raw_cache import RawCacheError

    started = time.monotonic()
    if session_cache is not None:
        session_cache.begin_attempt()

    host_data = host.parent / (host.stem + "_Data")
    if not host.is_file() or not (host_data / "Managed/Dungeon.CLIPlay.dll").is_file():
        raise ToolError(f"Universal host missing: {host}. Run 'unity-play host-build' once.")
    host_info = host.parent / "host.json"
    if not host_info.is_file():
        raise ToolError(f"Host compatibility manifest missing: {host_info}")
    if session_cache is not None:
        try:
            metadata = json.loads(session_cache.read(host_info)[0].decode("utf-8-sig"))
            version_file = project / "ProjectSettings/ProjectVersion.txt"
            version_text = session_cache.read(version_file)[0].decode("utf-8-sig")
        except (RawCacheError, OSError, UnicodeError) as error:
            raise ToolError(str(error)) from error
        version_match = re.search(r"^m_EditorVersion:\s*(\S+)", version_text, re.MULTILINE)
        if version_match is None:
            raise ToolError(f"Cannot read Unity version from {version_file}")
        project_version = version_match.group(1)
    else:
        metadata = json.loads(host_info.read_text())
        project_version = unity_version(project)
    if metadata.get("engineVersion") != project_version:
        raise ToolError("Raw project and universal host must use the same Unity version.")
    if metadata.get("inputProjectAssetsBundled") is not False:
        raise ToolError("Host is not an isolated universal host.")
    timings = {"nativeValidationSeconds": 0.0, "compilerSeconds": 0.0,
               "sceneSeconds": 0.0, "publicationSeconds": 0.0}
    context = None
    candidate_inventory = None
    initial_roots = None
    trusted = None
    fast = False
    if session_cache is not None:
        context = _session_context(project, unity, host, scene, output, metadata,
                                   native_import, native_catalog, native_cache)
        trusted = session_cache._trusted
        if trusted is not None and trusted["context"] == context:
            candidate_inventory = session_cache.inventory(trusted["roots"], files=trusted["files"],
                                                          exclude=trusted["exclude"])
            fast = (previous_build is not None and previous_build.get("revision") == trusted["build"]["revision"]
                    and _content_only_changes(trusted["inventory"], candidate_inventory, project,
                                              trusted["critical"], session_cache.content_suffixes))
        if not fast:
            roots, files, exclude = _session_inputs(project, unity, host, output, metadata)
            initial_roots = roots
            candidate_inventory = session_cache.inventory(roots, files=files, exclude=exclude)
    revision = output / "revisions" / uuid.uuid4().hex
    revision.mkdir(parents=True, exist_ok=False)
    manifest_path = revision / "scene.json"
    try:
        native_started = time.monotonic()
        imported = None
        if fast:
            imported = trusted["imported"]
        elif native_import:
            from imported_assets import export_imported_assets
            imported = export_imported_assets(project, unity, [scene], cache_dir=native_cache, seed_library=True)
        elif native_catalog is not None:
            from imported_assets import inspect_catalog
            imported = inspect_catalog(native_catalog) if isinstance(native_catalog, Path) else copy.deepcopy(native_catalog)
        timings["nativeValidationSeconds"] = time.monotonic() - native_started
        if session_cache is not None and not fast and imported is not None:
            # Include the native importer's explicitly resolved package sources
            # before GUID indexing. Otherwise the first content edit computes a
            # different index key and needlessly reparses every package .meta.
            roots, files, exclude = _session_inputs(project, unity, host, output, metadata, imported)
            expanded_inventory = session_cache.inventory(roots, files=files, exclude=exclude)
            for name, expected in candidate_inventory.items():
                if expanded_inventory.get(name) != expected:
                    raise ToolError(f"Raw source inventory changed during native verification; retry: {name}")
            prefixes = tuple(str(Path(os.path.abspath(root))) + os.sep for root in initial_roots)
            for name in expanded_inventory.keys() - candidate_inventory.keys():
                if any(name.startswith(prefix) for prefix in prefixes):
                    raise ToolError(f"Raw source appeared during native verification; retry: {name}")
            candidate_inventory = expanded_inventory
            initial_roots = roots
        scene_started = time.monotonic()
        scene_arguments = {} if session_cache is None else {"session_cache": session_cache,
                                                           "source_inventory": candidate_inventory,
                                                           "catalog_is_owned": True}
        try:
            if imported is not None:
                if imported.get("engineVersion") != metadata["engineVersion"]:
                    raise ToolError("Native asset catalog and universal host must use the same Unity version.")
                if imported.get("project") != str(project.resolve()):
                    raise ToolError("Native asset catalog belongs to another input project.")
                manifest = prepare_scene(project, scene, manifest_path, native_catalog=imported, **scene_arguments)
            else:
                manifest = prepare_scene(project, scene, manifest_path, **scene_arguments)
        except RawSceneError as error:
            if not (fast and native_import and _needs_native_closure(error.report)):
                raise
            # A newly referenced existing heavy asset may be outside the native
            # cache's previous dependency closure. Only this precise diagnostic
            # starts the importer; malformed scenes keep the running revision.
            shutil.rmtree(revision)
            replacement = prepare_raw(project, unity, host, scene, output, native_import=native_import,
                                      native_catalog=native_catalog, native_cache=native_cache,
                                      session_cache=session_cache)
            replacement["reloadFallbackReason"] = "native-closure-expanded"
            replacement["nativeClosureRequirements"] = error.report["nativeImportRequirements"]
            replacement["prepareSeconds"] = time.monotonic() - started
            write_json(Path(replacement["manifest"]).parent / "build.json", replacement)
            write_json(output / "current.json", replacement)
            if session_cache._trusted is not None:
                session_cache._trusted["build"] = copy.deepcopy(replacement)
            return replacement
        scene_seconds = time.monotonic() - started
        timings["sceneSeconds"] = time.monotonic() - scene_started
        compiler_started = time.monotonic()
        if fast:
            compiler = copy.deepcopy(trusted["build"]["compiler"])
            compiler["cacheHit"] = True
            compiler["compiledAssemblyCount"] = 0
            compiler["reusedAssemblyCount"] = len(compiler["assemblies"])
            compiler["elapsedSeconds"] = 0.0
        else:
            with _package_environment(metadata, imported):
                compiler = compile_project(project, unity, revision / "Managed", host_data / "Managed", cache_dir=output / "compiler-cache")
        timings["compilerSeconds"] = time.monotonic() - compiler_started
        publication_started = time.monotonic()
        manifest["assemblies"] = [item["path"] for item in compiler["assemblies"]]
        manifest["scripts"] = compiler.get("scripts", [])
        _attach_lifecycle_protocol(manifest, compiler)
        player_stage = None
        if fast:
            # Existing Unity native MonoScripts bind the originally staged DLL
            # identities. A content reload must keep those exact assembly paths.
            player_stage = copy.deepcopy(trusted["build"].get("playerStage"))
            manifest["assemblies"] = list(trusted["manifest"]["assemblies"])
            if manifest["lifecycleBarrierVersion"] == 1:
                manifest["lifecycleBarrierAssemblies"] = list(trusted["manifest"]["lifecycleBarrierAssemblies"])
        elif imported is not None:
            from host import stage_player
            player_stage = stage_player(host, revision, compiler)
            manifest["assemblies"] = player_stage["assemblies"]
            if manifest["lifecycleBarrierVersion"] == 1:
                manifest["lifecycleBarrierAssemblies"] = player_stage["assemblies"]
        manifest["runtimeInitializers"] = compiler.get("runtimeInitializers", [])
        manifest["scriptableObjectBarrierVersion"] = 1 if compiler.get("capabilities", {}).get("scriptableObjectBarrier") else 0
        if manifest.get("requiresScriptableObjectBarrier") and manifest["scriptableObjectBarrierVersion"] != 1:
            raise ToolError("Scene requires verified ScriptableObject initialization; rebuild a matching host and compiler.")
        # Keep texture payload immutable throughout a launch; YAML scalar graphs are already embedded.
        snapshots = []
        source_hashes = {str(Path(item.get("sourcePath", project / item["path"])).resolve()): item["sha256"]
                         for item in manifest.get("sourceDependencies", [])}
        for index, asset in enumerate(manifest.get("assets", [])):
            if asset.get("kind") not in ("texture", "Texture2D"):
                continue
            source = Path(asset["path"])
            if not source.is_absolute():
                source = project / source
            destination = revision / "Payload" / f"{index}-{source.name}"
            destination.parent.mkdir(parents=True, exist_ok=True)
            if session_cache is not None:
                payload, copied_hash = session_cache.read(source)
                destination.write_bytes(payload)
                copied_hash = hashlib.sha256(destination.read_bytes()).hexdigest()
            else:
                shutil.copyfile(source, destination)
                copied_hash = hashlib.sha256(destination.read_bytes()).hexdigest()
            expected_hash = source_hashes.get(str(source.resolve()))
            if expected_hash is None or copied_hash != expected_hash:
                raise ToolError(f"Raw texture changed during snapshot preparation; retry: {source}")
            asset["path"] = str(destination)
            snapshots.append({"source": str(source), "sha256": copied_hash})
        manifest["hostEngineVersion"] = metadata["engineVersion"]
        manifest["revision"] = revision.name
        for dependency in manifest.get("sourceDependencies", []):
            source = Path(dependency.get("sourcePath", project / dependency["path"]))
            if session_cache is not None:
                session_cache.assert_digest(source, dependency["sha256"])
            elif not source.is_file() or hashlib.sha256(source.read_bytes()).hexdigest() != dependency["sha256"]:
                raise ToolError(f"Raw source changed during preparation; retry: {source}")
        session_state = None
        if session_cache is not None:
            if fast:
                roots, files, exclude = trusted["roots"], trusted["files"], trusted["exclude"]
            else:
                roots, files, exclude = _session_inputs(project, unity, host, output, metadata, imported, compiler, player_stage)
            final_inventory = session_cache.inventory(roots, files=files, exclude=exclude)
            # Newly resolved external packages are verified by the compiler and
            # native importer; every initially inventoried source must remain
            # identical throughout that verification and graph preparation.
            for name, expected in candidate_inventory.items():
                if final_inventory.get(name) != expected:
                    raise ToolError(f"Raw source inventory changed during preparation; retry: {name}")
            if not fast:
                prefixes = tuple(str(Path(os.path.abspath(root))) + os.sep for root in initial_roots)
                for name in final_inventory.keys() - candidate_inventory.keys():
                    if any(name.startswith(prefix) for prefix in prefixes):
                        raise ToolError(f"Raw source appeared during preparation; retry: {name}")
            if fast and final_inventory != candidate_inventory:
                raise ToolError("Raw source inventory changed during content preparation; retry.")
            session_cache.assert_observed_stable()
            critical = set(compiler.get("compilerInputs", {}))
            if imported:
                critical.update(item["physicalPath"] for item in imported.get("sourceDependencies", []))
                critical.update(item["bundlePath"] for item in imported.get("packs", []))
                if imported.get("catalogPath"):
                    critical.add(imported["catalogPath"])
            critical.update(str(path) for path in files)
            session_state = {"context": context, "roots": roots, "files": files, "exclude": exclude,
                             "inventory": final_inventory, "critical": critical, "imported": imported,
                             "manifest": manifest}
        write_json(manifest_path, manifest)
        result = {"schemaVersion": 1, "success": True, "manifest": str(manifest_path),
                  "host": str(host), "project": str(project), "scene": scene, "revision": revision.name,
                  "prepareSeconds": time.monotonic() - started, "sceneSeconds": scene_seconds,
                  "compiler": compiler, "snapshots": snapshots, "editorStarted": False,
                  "reloadCompatible": fast, "preparationMode": "content-only" if fast else "full",
                  "requiresProjectLibrary": False, "requiresCookedProjectContent": imported is not None, "requiresCookedScene": False}
        result["player"] = player_stage["player"] if player_stage else str(host)
        result["playerStage"] = player_stage
        result["nativeImport"] = {"enabled": imported is not None, "automatic": native_import,
                                  "cacheDirectory": str(native_cache) if native_cache else None,
                                  "catalog": imported.get("catalogPath") if imported else None,
                                  "cacheHit": imported.get("cacheHit") if imported else None,
                                  "containsScenes": False}
        result["editorStarted"] = bool(imported and imported.get("cacheHit") is False and native_import)
        if imported:
            result["nativeImport"]["wallSeconds"] = imported.get("wallSeconds", 0)
        if fast:
            result["nativeImport"]["cacheHit"] = True if imported else None
            result["nativeImport"]["wallSeconds"] = 0.0
            result["editorStarted"] = False
        timings["publicationSeconds"] = time.monotonic() - publication_started
        result["stageTimings"] = timings
        if session_cache is not None:
            result["cacheStats"] = session_cache.cache_stats()
            result["prepareSeconds"] = time.monotonic() - started
        write_json(revision / "build.json", result)
        write_json(output / "current.json", result)
        if session_state is not None:
            session_state["build"] = copy.deepcopy(result)
            session_cache._trusted = session_state
        return result
    except (RawCompileError, RawSceneError, RawCacheError, ToolError, OSError) as error:
        if revision.exists():
            shutil.rmtree(revision)
        raise ToolError(str(error)) from error


def _session_context(project, unity, host, scene, output, metadata, native_import, native_catalog, native_cache):
    catalog = (hashlib.sha256(json.dumps(native_catalog, sort_keys=True).encode()).hexdigest()
               if isinstance(native_catalog, dict) else str(native_catalog))
    return (str(project.resolve()), str(unity.resolve()), str(host.resolve()), scene, str(output.resolve()),
            json.dumps(metadata, sort_keys=True), bool(native_import), catalog, str(native_cache),
            os.environ.get("UNITY_PLAY_PACKAGE_ROOT", ""), str((project / "Library").resolve()))


def _needs_native_closure(report):
    if not isinstance(report, dict) or not report.get("nativeImportRequirements"):
        return False
    errors = [item for item in report.get("diagnostics", []) if item.get("severity") == "error"]
    return bool(errors) and all(item.get("code") in {"native-import-required", "asset-fileid"} for item in errors)


def _session_inputs(project, unity, host, output, metadata, imported=None, compiler=None, player_stage=None):
    tool = Path(__file__).resolve().parent
    data = unity.parent / "Data"
    roots = [project / name for name in ("Assets", "Packages", "ProjectSettings")]
    roots.extend(Path(record["source"]) for record in metadata.get("packages", []))
    roots.extend(Path(record["source"]) for record in (imported or {}).get("sourceRoots", []))
    roots.extend(Path(path) for path in os.environ.get("UNITY_PLAY_PACKAGE_ROOT", "").split(os.pathsep) if path)
    roots.extend(data / name for name in ("Resources/PackageManager/BuiltInPackages", "DotNetSdk/sdk",
                                          "DotNetSdk/host/fxr", "DotNetSdk/shared", "MonoBleedingEdge/lib/mono/unityaot-linux",
                                          "PlaybackEngines/LinuxStandaloneSupport/Variations/mono/Managed",
                                          "Tools/BuildPipeline/Unity.SourceGenerators"))
    roots.extend([host.parent, tool])
    files = [host, host.parent / "host.json", unity, data / "DotNetSdk/dotnet"]
    for name in (compiler or {}).get("compilerInputs", {}):
        path = Path(name)
        files.append(path)
        if path.name == "package.json":
            roots.append(path.parent)
    for assembly in (compiler or {}).get("assemblies", []):
        path = Path(assembly["path"])
        files.append(path)
        if path.with_suffix(".pdb").is_file():
            files.append(path.with_suffix(".pdb"))
    for name in (player_stage or {}).get("assemblies", []):
        files.append(Path(name))
    if imported:
        if imported.get("catalogPath"):
            files.append(Path(imported["catalogPath"]))
        files.extend(Path(item["bundlePath"]) for item in imported.get("packs", []))
        files.extend(Path(item["physicalPath"]) for item in imported.get("sourceDependencies", []))
    exclude = [output, project / "Library"]
    exclude.extend(host.parent / "HostProject" / name for name in ("Library", "Temp", "Logs", "Builds", "UserSettings"))
    exclude.append(tool / "__pycache__")
    for folder in tool.iterdir():
        if folder.is_dir():
            exclude.extend(folder / name for name in ("bin", "obj", "Library", "Temp", "Builds", "Logs", "__pycache__"))
    return list(dict.fromkeys(roots)), list(dict.fromkeys(files)), exclude


def _content_only_changes(previous, current, project, critical, suffixes):
    assets = str(project.resolve() / "Assets") + os.sep
    for name in previous.keys() | current.keys():
        if previous.get(name) == current.get(name):
            continue
        if name in critical or not name.startswith(assets):
            return False
        if any(value is not None and len(value) > 1 and isinstance(value[0], tuple)
               for value in (previous.get(name), current.get(name))):
            # An alias of a native dependency must not bypass the critical-path
            # check merely because its authored lexical path has another name.
            return False
        if name.endswith(os.sep):
            # A source folder can be added or removed together with ordinary raw
            # content. Its contained files still pass the individual checks.
            continue
        path = Path(name)
        if path.suffix.lower() == ".meta":
            path = path.with_suffix("")
        if path.suffix.lower() not in suffixes:
            return False
    return True



@contextlib.contextmanager
def _package_environment(host_metadata: dict, imported: dict | None):
    previous = os.environ.get("UNITY_PLAY_PACKAGE_ROOT")
    paths = [record["source"] for record in host_metadata.get("packages", [])]
    if paths:
        # This tool-owned directory also carries exact UPM git resolution provenance.
        paths.extend(str(Path(path).parent) for path in paths if (Path(path).parent / ".unity-play-package-resolution.json").is_file())
    paths.extend(record["source"] for record in (imported or {}).get("sourceRoots", []))
    if previous:
        paths.extend(previous.split(os.pathsep))
    if paths:
        os.environ["UNITY_PLAY_PACKAGE_ROOT"] = os.pathsep.join(dict.fromkeys(paths))
    try:
        yield
    finally:
        if previous is None:
            os.environ.pop("UNITY_PLAY_PACKAGE_ROOT", None)
        else:
            os.environ["UNITY_PLAY_PACKAGE_ROOT"] = previous


def current_manifest(output: Path) -> Path:
    from unity_play import ToolError

    pointer = output / "current.json"
    if not pointer.is_file():
        raise ToolError(f"No prepared raw content: {pointer}")
    return Path(json.loads(pointer.read_text())["manifest"])
