"""Prepare a reusable Mono host in a separate tool-owned Unity project."""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import time


def _seed_package_sources(source_project: Path, destination: Path) -> None:
    from unity_play import require_idle, write_json
    require_idle(source_project)
    cache = source_project / "Library/PackageCache"
    destination.mkdir(parents=True, exist_ok=True)
    if cache.is_dir():
        for source in sorted(cache.iterdir()):
            if not (source / "package.json").is_file():
                continue
            target = destination / source.name
            if not target.exists():
                subprocess.run(["cp", "-a", "--reflink=auto", str(source), str(target)], check=True)
    resolution_path = source_project / "Library/PackageManager/projectResolution.json"
    if not resolution_path.is_file():
        return
    resolution = json.loads(resolution_path.read_text())
    entries = resolution.get("outputs", resolution.get("packages", resolution))
    lock_path = source_project / "Packages/packages-lock.json"
    locked = json.loads(lock_path.read_text()).get("dependencies", {}) if lock_path.is_file() else {}
    provenance = destination / ".unity-play-package-resolution.json"
    records = json.loads(provenance.read_text()).get("packages", {}) if provenance.is_file() else {}
    for package_id, record in entries.items():
        if not isinstance(record, dict) or "resolvedPath" not in record:
            continue
        target = destination / Path(record["resolvedPath"]).name
        name = record.get("name")
        if target.is_dir() and name in locked:
            records[name] = {"packageId": package_id, "root": target.name, "gitHash": locked[name].get("hash")}
    write_json(provenance, {"schemaVersion": 1, "packages": records})


def _resolve_packages(project: Path, unity: Path, isolated: Path, timeout: float) -> None:
    from unity_play import ToolError, require_idle, run_process, unity_version, write_json
    require_idle(isolated)
    isolated.mkdir(parents=True, exist_ok=True)
    (isolated / "Assets").mkdir(exist_ok=True)
    packages = isolated / "Packages"
    packages.mkdir(exist_ok=True)
    for source in (project / "Packages").iterdir():
        target = packages / source.name
        if source.is_dir():
            if target.exists():
                shutil.rmtree(target)
            subprocess.run(["cp", "-a", "--reflink=auto", str(source), str(target)], check=True)
        elif source.is_file():
            shutil.copyfile(source, target)
    manifest_path = packages / "manifest.json"
    manifest = json.loads(manifest_path.read_text())
    for name, requested in manifest.get("dependencies", {}).items():
        if requested.startswith("file:"):
            manifest["dependencies"][name] = "file:" + str((project / "Packages" / requested[5:]).resolve())
    write_json(manifest_path, manifest)
    settings = isolated / "ProjectSettings"
    settings.mkdir(exist_ok=True)
    (settings / "ProjectVersion.txt").write_text("m_EditorVersion: " + unity_version(project) + "\n")
    log = isolated.parent / "package-resolve.log"
    command = [str(unity), "-batchmode", "-nographics", "-quit", "-projectPath", str(isolated), "-logFile", str(log)]
    result = run_process(command, isolated, isolated.parent / "package-resolve-launch.log", timeout)
    if result["exitCode"] or not (isolated / "Library/PackageManager/projectResolution.json").is_file():
        raise ToolError(f"Isolated Unity package resolution failed; see {log}")


def build_host(project: Path, unity: Path, output: Path, timeout: float = 1800, project_packages: bool = False) -> dict:
    from unity_play import BRIDGE, TOOL_ROOT, ToolError, require_idle, run_process, unity_version, write_json

    version = unity_version(project)
    if not unity.is_file():
        raise ToolError(f"Unity installation not found: {unity}")
    host_project = output.parent / "HostProject"
    require_idle(host_project)
    host_project.mkdir(parents=True, exist_ok=True)
    managed_root = host_project / "Assets/CLIPlay"
    copied = []
    for source in sorted((BRIDGE / "Runtime").rglob("*")):
        if not source.is_file():
            continue
        destination = managed_root / source.relative_to(BRIDGE / "Runtime")
        destination.parent.mkdir(parents=True, exist_ok=True)
        # Tool source is authoritative; generated host content is handled separately.
        if not destination.exists() or destination.read_bytes() != source.read_bytes():
            shutil.copyfile(source, destination)
        copied.append({"path": str(source.relative_to(BRIDGE)), "sha256": hashlib.sha256(source.read_bytes()).hexdigest()})
    editor = host_project / "Assets/Editor/HostBuild.cs"
    editor.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(TOOL_ROOT / "HostProject/Assets/Editor/HostBuild.cs", editor)
    settings = host_project / "ProjectSettings"
    settings.mkdir(exist_ok=True)
    (settings / "ProjectVersion.txt").write_text(f"m_EditorVersion: {version}\n")
    package_records = []
    if project_packages:
        require_idle(project)
        # This cache belongs to the tool. Raw compilation never discovers the
        # input project's Library; a cold machine can let UPM resolve packages.
        package_sources = output.parent / "PackageSources"
        package_sources.mkdir(exist_ok=True)
        _seed_package_sources(project, package_sources)
        from raw_compile import _package_roots, _data, RawCompileError
        def resolve_roots():
            previous = os.environ.get("UNITY_PLAY_PACKAGE_ROOT")
            extra = str(package_sources) + (os.pathsep + previous if previous else "")
            os.environ["UNITY_PLAY_PACKAGE_ROOT"] = extra
            try:
                return _package_roots(project, _data(unity))[0]
            finally:
                if previous is None:
                    os.environ.pop("UNITY_PLAY_PACKAGE_ROOT", None)
                else:
                    os.environ["UNITY_PLAY_PACKAGE_ROOT"] = previous
        try:
            package_roots = resolve_roots()
        except RawCompileError as error:
            if "has no raw sources" not in str(error):
                raise
            # On a fresh input project, UPM resolves into an isolated tool
            # project. The input still receives no Library or build output.
            resolution_project = output.parent / "PackageResolveProject"
            _resolve_packages(project, unity, resolution_project, timeout)
            _seed_package_sources(resolution_project, package_sources)
            package_roots = resolve_roots()
        for source in package_roots:
            info = json.loads((source / "package.json").read_text())
            package_records.append({"name": info["name"], "version": info["version"], "source": str(source)})
        for name in ("ProjectSettings.asset", "TagManager.asset", "DynamicsManager.asset", "TimeManager.asset", "InputManager.asset"):
            source = project / "ProjectSettings" / name
            if source.is_file():
                shutil.copyfile(source, settings / name)
    packages = host_project / "Packages"
    packages.mkdir(exist_ok=True)
    modules = ["animation", "assetbundle", "audio", "imageconversion", "jsonserialize", "physics", "physics2d", "ui", "imgui", "screencapture"]
    dependencies = {f"com.unity.modules.{name}": "1.0.0" for name in modules}
    if project_packages:
        # Embed exact resolved versions, including transitive dependencies, so
        # the host and importer compile identical public package APIs.
        for record in package_records:
            source = Path(record["source"])
            if source.is_relative_to(unity.parent / "Data/Resources/PackageManager/BuiltInPackages"):
                dependencies[record["name"]] = record["version"]
                continue
            destination = packages / record["name"]
            if not destination.exists():
                subprocess.run(["cp", "-a", "--reflink=auto", str(source), str(destination)], check=True)
            dependencies[record["name"]] = "file:" + record["name"]
    write_json(packages / "manifest.json", {"dependencies": dependencies})
    profile = host_project / "Assets/Editor/HostPackageProfile.cs"
    has_urp = any(record["name"] == "com.unity.render-pipelines.universal" for record in package_records)
    if has_urp:
        profile.write_text('''using UnityEditor;
using UnityEngine;
using UnityEngine.Rendering;
using UnityEngine.Rendering.Universal;
namespace Dungeon.CLIPlay.HostBuild {
    public static partial class Build {
        static partial void PreparePackageProfile() {
            const string rendererPath = "Assets/Resources/HostRenderer.asset";
            var renderer = AssetDatabase.LoadAssetAtPath<UniversalRendererData>(rendererPath);
            if (renderer == null) {
                renderer = ScriptableObject.CreateInstance<UniversalRendererData>();
                AssetDatabase.CreateAsset(renderer, rendererPath);
            }
            const string pipelinePath = "Assets/Resources/HostPipeline.asset";
            var pipeline = AssetDatabase.LoadAssetAtPath<UniversalRenderPipelineAsset>(pipelinePath);
            if (pipeline == null) {
                pipeline = UniversalRenderPipelineAsset.Create(renderer);
                AssetDatabase.CreateAsset(pipeline, pipelinePath);
            }
            GraphicsSettings.defaultRenderPipeline = pipeline;
            QualitySettings.renderPipeline = pipeline;
            AssetDatabase.SaveAssets();
        }
    }
}
''')
    else:
        profile.unlink(missing_ok=True)
    log = output.parent / "host-build.log"
    build_report = output.parent / "host-build-result.json"
    build_report.unlink(missing_ok=True)
    environment = os.environ.copy()
    environment.update(UNITY_RAW_HOST_OUTPUT=str(output), UNITY_RAW_HOST_REPORT=str(build_report))
    command = [str(unity), "-batchmode", "-nographics", "-projectPath", str(host_project),
               "-buildTarget", "Linux64", "-executeMethod", "Dungeon.CLIPlay.HostBuild.Build.FromCommandLine",
               "-logFile", str(log)]
    started = time.monotonic()
    process = run_process(command, host_project, output.parent / "host-launch.log", timeout, env=environment)
    if process["exitCode"] or not build_report.exists():
        raise ToolError(f"Universal host build failed; see {log}")
    report = json.loads(build_report.read_text())
    if not report.get("success") or not output.is_file():
        raise ToolError(f"Universal host build failed: {report.get('failure')}; see {log}")
    report.update({"wallSeconds": time.monotonic() - started, "hostProject": str(host_project),
                   "inputProjectAssetsBundled": False, "sourceFiles": copied,
                   "shaders": ["Standard", "Unlit/Texture", "Unlit/Color"],
                   "renderPipeline": "URP" if has_urp else "BuiltIn", "scriptingBackend": "Mono", "platform": "Linux64"})
    from raw_compile import _files
    for record in package_records:
        source = Path(record["source"])
        files = {path.relative_to(source).as_posix(): hashlib.sha256(path.read_bytes()).hexdigest()
                 for pattern in ("*.cs", "*.asmdef", "*.asmref", "*.dll", "*.rsp")
                 for path in _files(source, pattern)}
        record["sourceHashes"] = files
    report["packages"] = package_records
    report["packageProfile"] = "project" if project_packages else "minimal"
    package_assembly_names = {json.loads(path.read_text())["name"] for record in package_records
                              for path in _files(Path(record["source"]), "*.asmdef")}
    report["providedPackageAssemblies"] = [path.stem for path in
        (output.parent / (output.stem + "_Data") / "Managed").glob("*.dll")
        if path.stem != "Dungeon.CLIPlay" and (path.stem in package_assembly_names or not path.stem.startswith(("UnityEngine", "System", "Mono", "mscorlib", "netstandard")))] if project_packages else []
    write_json(output.parent / "host.json", report)
    return report


def refresh_bridge(project: Path, unity: Path, host: Path) -> dict:
    """Replace tool-only managed code; preserve the native host and package SDK."""
    import tempfile
    from raw_compile import _data, _files
    from unity_play import BRIDGE, ToolError, unity_version, write_json

    metadata_path = host.parent / "host.json"
    metadata = json.loads(metadata_path.read_text())
    if metadata.get("engineVersion") != unity_version(project) or metadata.get("inputProjectAssetsBundled") is not False:
        raise ToolError("Bridge refresh requires an isolated host matching the project Unity version.")
    for proc in Path("/proc").glob("[0-9]*"):
        try:
            if (proc / "exe").resolve() == host.resolve():
                raise ToolError("Stop this host before refreshing its managed bridge.")
        except (OSError, RuntimeError):
            continue
    expected = {record["path"]: record["sha256"] for record in metadata.get("sourceFiles", [])}
    # Serialized carriers and the authored assembly identity belong to native
    # player data. Those require a complete host build when they change.
    for relative in ("Runtime/Raw/ImportedAssetRegistry.cs", "Runtime/Dungeon.CLIPlay.asmdef"):
        source = BRIDGE / relative
        if source.is_file() and expected.get(relative) != hashlib.sha256(source.read_bytes()).hexdigest():
            raise ToolError(f"Serialized host contract changed: {relative}; use host-build.")
    data = _data(unity)
    sdk = data / "DotNetSdk"
    compiler = sorted(sdk.glob("sdk/*/Roslyn/bincore/csc.dll"))[-1]
    bcl = data / "MonoBleedingEdge/lib/mono/unityaot-linux"
    refs = {path.name: path for path in bcl.glob("*.dll")}
    for path in (bcl / "Facades").glob("*.dll"):
        refs.setdefault(path.name, path)
    engine = data / "PlaybackEngines/LinuxStandaloneSupport/Variations/mono/Managed"
    for path in engine.glob("UnityEngine*.dll"):
        refs[path.name] = path
    managed = host.parent / (host.stem + "_Data") / "Managed"
    for path in managed.glob("*.dll"):
        if path.name != "Dungeon.CLIPlay.dll":
            refs.setdefault(path.name, path)
    sources = _files(BRIDGE / "Runtime", "*.cs")
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix=".bridge-", dir=host.parent) as temporary:
        folder = Path(temporary)
        copied = []
        snapshots = []
        for index, source in enumerate(sources):
            content = source.read_bytes()
            target = folder / "Sources" / str(index) / source.name
            target.parent.mkdir(parents=True)
            target.write_bytes(content)
            snapshots.append(target)
        records = [{"path": str(source.relative_to(BRIDGE)), "sha256": hashlib.sha256(source.read_bytes()).hexdigest()}
                   for source in sorted((BRIDGE / "Runtime").rglob("*")) if source.is_file()]
        destination = folder / "Dungeon.CLIPlay.dll"
        args = ["-nologo", "-target:library", "-nostdlib+", "-langversion:9.0", "-deterministic+", "-debug:portable",
                "-define:UNITY_STANDALONE;UNITY_STANDALONE_LINUX;ENABLE_MONO;DEVELOPMENT_BUILD;NET_STANDARD_2_1;UNITY_6000_6_OR_NEWER",
                "-out:" + str(destination)]
        args += ["-reference:" + str(path) for path in refs.values()]
        args += [str(path) for path in snapshots]
        response = folder / "compile.rsp"
        response.write_text("\n".join('"' + arg.replace('"', '\\"') + '"' for arg in args))
        run = subprocess.run([str(sdk / "dotnet"), str(compiler), "@" + str(response)], capture_output=True, text=True, timeout=180)
        if run.returncode or not destination.is_file():
            raise ToolError("Managed host bridge compilation failed:\n" + run.stdout + run.stderr)
        for record in records:
            if hashlib.sha256((BRIDGE / record["path"]).read_bytes()).hexdigest() != record["sha256"]:
                raise ToolError("Bridge source changed during compilation; host preserved.")
        for path in folder.glob("Dungeon.CLIPlay.*"):
            os.replace(path, managed / path.name)
        metadata["sourceFiles"] = records
        metadata["bridgeRefresh"] = {"editorStarted": False, "seconds": time.monotonic() - started,
                                     "sha256": hashlib.sha256((managed / "Dungeon.CLIPlay.dll").read_bytes()).hexdigest()}
        write_json(metadata_path, metadata)
    return metadata["bridgeRefresh"]


def stage_player(host: Path, revision: Path, compiler: dict) -> dict:
    """Register source DLLs at native startup in an isolated reflink of the host."""
    from unity_play import ToolError, write_json

    started = time.monotonic()
    data = host.parent / (host.stem + "_Data")
    table_path = data / "ScriptingAssemblies.json"
    try:
        table = json.loads(table_path.read_text())
    except (OSError, ValueError) as error:
        raise ToolError(f"Host native assembly registration table is unavailable: {table_path}") from error
    names, kinds = table.get("names"), table.get("types")
    if (not isinstance(names, list) or not isinstance(kinds, list) or len(names) != len(kinds)
            or any(not isinstance(name, str) or Path(name).name != name or not name.endswith(".dll") for name in names) or len(set(names)) != len(names)
            or any(type(kind) is not int for kind in kinds)):
        raise ToolError("Host native assembly registration table has an invalid schema.")
    originals = table_path.read_bytes()
    records = compiler.get("assemblies", [])
    source_names = set()
    for record in records:
        source = Path(record["path"])
        if source.name in source_names or source.name in names or (data / "Managed" / source.name).exists():
            raise ToolError(f"Source assembly conflicts with the native host: {source.name}")
        if source.name != record["name"] + ".dll" or hashlib.sha256(source.read_bytes()).hexdigest() != record["sha256"]:
            raise ToolError(f"Source assembly changed before native startup registration: {source}")
        source_names.add(source.name)
    output = revision / "Player"
    output.mkdir(exist_ok=False)
    files = [host, data, host.parent / "host.json"]
    files.extend(path for path in sorted(host.parent.iterdir())
                 if path.is_file() and (path.name.endswith(".so") or ".so." in path.name or path.name == "UnityCrashHandler64"))
    copied = subprocess.run(["cp", "-a", "--reflink=auto", "--", *map(str, files), str(output)], capture_output=True, text=True)
    if copied.returncode:
        raise ToolError("Cannot prepare the isolated native Player: " + copied.stderr.strip())
    assemblies = []
    for record in records:
        source = Path(record["path"])
        destination = output / data.name / "Managed" / source.name
        shutil.copyfile(source, destination)
        if hashlib.sha256(destination.read_bytes()).hexdigest() != record["sha256"]:
            raise ToolError(f"Source assembly changed during native startup registration: {source}")
        pdb = source.with_suffix(".pdb")
        if pdb.exists():
            payload = pdb.read_bytes()
            destination.with_suffix(".pdb").write_bytes(payload)
            if hashlib.sha256(pdb.read_bytes()).digest() != hashlib.sha256(payload).digest():
                raise ToolError(f"Source symbols changed during native startup registration: {pdb}")
        # 16 is the UserAssembly flag emitted by this host's Unity build for
        # project/package runtime DLLs. Engine flags remain exactly as built.
        names.append(source.name)
        kinds.append(16)
        assemblies.append(str(destination))
    write_json(output / data.name / "ScriptingAssemblies.json", table)
    if table_path.read_bytes() != originals:
        raise ToolError("Host assembly registration changed during isolated Player preparation.")
    return {"player": str(output / host.name), "assemblies": assemblies,
            "registeredAssemblies": sorted(source_names), "stageSeconds": time.monotonic() - started,
            "hostModified": False}
