"""Cold Linux Mono compilation using only raw project sources and Unity SDK.

No Library, Bee response files, imported script database or Editor process is used.
This is deliberately a limited compiler: unsupported plugin import settings,
IL postprocessing, source generators and implicit runtime initializers fail closed.
Package sources must already be embedded, local, installed builtins or provisioned
in UNITY_PLAY_PACKAGE_ROOT (an os.pathsep-separated list of package directories).
"""
from __future__ import annotations

from dataclasses import dataclass, field
from contextlib import contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import time


class RawCompileError(ValueError):
    """The raw project cannot safely be compiled by the supported pipeline."""


def _within(path: Path, root: Path) -> bool:
    # Compare complete components; '/LibrarySibling' is not inside '/Library'.
    # Path.is_relative_to constructs every parent on some Python versions.
    parts, prefix = path.parts, root.parts
    return len(parts) >= len(prefix) and parts[:len(prefix)] == prefix


def _without_project_library(project: Path, path: Path, *, library_resolved: Path | None = None) -> Path:
    resolved = path.resolve()
    library = project / "Library"
    lexical = Path(os.path.abspath(path))
    canonical_library = library.resolve() if library_resolved is None else library_resolved
    if _within(lexical, library) or _within(resolved, library) or _within(resolved, canonical_library):
        raise RawCompileError(f"Raw compilation cannot use the project's Library: {path} resolves to {resolved}")
    return resolved


@dataclass
class Assembly:
    name: str
    root: Path
    config: dict = field(default_factory=dict)
    sources: list[Path] = field(default_factory=list)
    dependencies: list[str] = field(default_factory=list)
    defines: set[str] = field(default_factory=set)
    active: bool = True


def _json(path: Path) -> dict:
    try:
        value = json.loads(path.read_text())
        if not isinstance(value, dict):
            raise ValueError("expected JSON object")
        return value
    except (OSError, ValueError) as error:
        raise RawCompileError(f"Invalid JSON {path}: {error}") from error


def _guid(path: Path) -> str | None:
    meta = path.with_name(path.name + ".meta")
    if not meta.is_file():
        return None
    match = re.search(r"^guid:\s*([0-9a-fA-F]{32})\s*$", meta.read_text(), re.MULTILINE)
    return match.group(1).lower() if match else None


class _SourceInventory:
    """Directory listings shared within one source snapshot, never across builds."""

    suffixes = {".cs", ".asmdef", ".asmref", ".dll", ".rsp"}

    def __init__(self):
        self.roots: dict[Path, dict[str, list[Path]]] = {}

    def clear(self):
        self.roots.clear()

    def files(self, root: Path, pattern: str) -> list[Path]:
        suffix = pattern[1:] if pattern.startswith("*.") else ""
        if suffix not in self.suffixes:
            return _files(root, pattern)
        if root not in self.roots:
            groups = {suffix: [] for suffix in self.suffixes}
            # Match rglob's directory-symlink policy while pruning folders
            # Unity ignores. Only source/configuration files are retained.
            for current, directories, names in os.walk(root):
                directories[:] = [name for name in directories if not name.startswith(".") and not name.endswith("~")]
                for name in names:
                    if name.startswith(".") or name.endswith("~"):
                        continue
                    path = Path(current) / name
                    if path.suffix in groups and path.is_file():
                        groups[path.suffix].append(path)
            self.roots[root] = {suffix: sorted(paths) for suffix, paths in groups.items()}
        return self.roots[root][suffix]


def _files(root: Path, pattern: str, inventory: _SourceInventory | None = None) -> list[Path]:
    if inventory is not None:
        return inventory.files(root, pattern)
    # Unity ignores hidden directories and package Samples~/Documentation~.
    # Searching those would compile example projects or duplicate asmdefs.
    return [path for path in sorted(root.rglob(pattern)) if path.is_file()
            and not any(part.startswith(".") or part.endswith("~") for part in path.parts[len(root.parts):])]


def _source_inputs(project: Path, roots: list[Path], inventory: _SourceInventory | None = None) -> dict[str, str]:
    if inventory is not None:
        # The final validation takes an entirely fresh directory snapshot,
        # including files added or removed since compilation began.
        inventory.clear()
    library_resolved = (project / "Library").resolve()
    paths = {project / "ProjectSettings/ProjectVersion.txt"}
    for relative in ("ProjectSettings/ProjectSettings.asset", "Packages/manifest.json", "Packages/packages-lock.json"):
        path = project / relative
        if path.is_file():
            paths.add(path)
    for root in roots:
        if (root / "package.json").is_file():
            paths.add(root / "package.json")
        for pattern in ("*.cs", "*.asmdef", "*.asmref", "*.dll", "*.rsp"):
            for path in _files(root, pattern, inventory):
                paths.add(path)
                meta = path.with_name(path.name + ".meta")
                if meta.is_file():
                    paths.add(meta)
    return {str(_without_project_library(project, path, library_resolved=library_resolved)): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in sorted(paths)}


def _captured_hash(path: Path, inputs: dict[str, str]) -> str:
    digest = inputs.get(str(path.resolve()))
    if digest is None:
        raise RawCompileError(f"Raw source identity changed after its input snapshot: {path}; retry")
    return digest


def _data(unity: Path) -> Path:
    unity = unity.resolve()
    result = unity if unity.name == "Data" else unity.parent / "Data"
    if not result.is_dir():
        raise RawCompileError(f"Unity Data installation not found: {result}")
    return result


def _package_roots(project: Path, data: Path) -> tuple[list[Path], dict[str, str]]:
    _without_project_library(project, data)
    manifest = project / "Packages/manifest.json"
    _without_project_library(project, manifest)
    dependencies = _json(manifest).get("dependencies", {}) if manifest.is_file() else {}
    lock_path = project / "Packages/packages-lock.json"
    _without_project_library(project, lock_path)
    locked = _json(lock_path).get("dependencies", {}) if lock_path.is_file() else {}
    builtin = data / "Resources/PackageManager/BuiltInPackages"
    provisioned = [Path(value).expanduser().resolve() for value in
                   os.environ.get("UNITY_PLAY_PACKAGE_ROOT", "").split(os.pathsep) if value]
    # An externally supplied package cache is permitted; the project's Library
    # is never a discovery source, even when it happens to exist.
    for path in provisioned:
        try:
            _without_project_library(project, path)
        except RawCompileError as error:
            raise RawCompileError("UNITY_PLAY_PACKAGE_ROOT cannot point into the raw project's Library") from error
    candidates, native_resolutions = {}, {}
    for folder in provisioned:
        resolution_path = folder / ".unity-play-package-resolution.json"
        if resolution_path.is_file():
            descriptor = _json(resolution_path)
            if descriptor.get("schemaVersion") == 1:
                for name, record in descriptor.get("packages", {}).items():
                    native_resolutions[name] = (folder, record)
        entries = [folder] if (folder / "package.json").is_file() else sorted(folder.glob("*"))
        for entry in entries:
            _without_project_library(project, entry)
            descriptor = entry / "package.json"
            if descriptor.is_file():
                _without_project_library(project, descriptor)
                info = _json(descriptor)
                candidates[(info.get("name"), info.get("version"))] = entry
    roots, versions, visiting = {}, {}, set()

    def resolve(name: str, requested: str):
        if name in roots:
            return
        if name in visiting:
            raise RawCompileError(f"Cyclic package dependency: {name}")
        visiting.add(name)
        version = locked.get(name, {}).get("version", requested)
        embedded = project / "Packages" / name
        if (embedded / "package.json").is_file():
            root = embedded
        elif requested.startswith("file:"):
            root = (project / "Packages" / requested[5:]).resolve()
        elif (builtin / name / "package.json").is_file():
            root = builtin / name
        else:
            root = candidates.get((name, version))
            if root is None and locked.get(name, {}).get("source") == "git" and name in native_resolutions:
                folder, record = native_resolutions[name]
                candidate = (folder / record.get("root", "")).resolve()
                if (record.get("packageId") == name + "@" + version
                        and record.get("gitHash") == locked[name].get("hash")
                        and _within(candidate, folder) and (candidate / "package.json").is_file()):
                    root = candidate
            if root is None:
                raise RawCompileError(
                    f"Package {name} ({version}) has no raw sources. Embed it in Packages/{name}, "
                    "use a file: dependency, or provision UNITY_PLAY_PACKAGE_ROOT. "
                    "Project Library/PackageCache is intentionally not used.")
        root = _without_project_library(project, root)
        descriptor = root / "package.json"
        _without_project_library(project, descriptor)
        if not descriptor.is_file():
            raise RawCompileError(f"Missing package descriptor: {descriptor}")
        info = _json(descriptor)
        if info.get("name") != name:
            raise RawCompileError(f"Package name mismatch: requested {name}, got {info.get('name')} in {root}")
        actual_version = str(info.get("version", ""))
        if not requested.startswith(("file:", "http:", "https:", "git:")) and actual_version != version:
            raise RawCompileError(f"Package {name} version mismatch: expected {version}, found {actual_version}")
        roots[name], versions[name] = root.resolve(), actual_version
        for child, child_version in info.get("dependencies", {}).items():
            resolve(child, child_version)
        visiting.remove(name)

    for name, version in dependencies.items():
        resolve(name, version)
    return list(roots.values()), versions


def _base_defines(project: Path) -> set[str]:
    version_file = project / "ProjectSettings/ProjectVersion.txt"
    version = version_file.read_text() if version_file.is_file() else ""
    match = re.search(r"m_EditorVersion:\s*(\d+)\.(\d+)\.(\d+)", version)
    if not match:
        raise RawCompileError("ProjectSettings/ProjectVersion.txt must declare a Unity version")
    major, minor, patch = map(int, match.groups())
    defines = {
        "UNITY_STANDALONE", "UNITY_STANDALONE_LINUX", "UNITY_64", "ENABLE_MONO",
        "ENABLE_MANAGED_JOBS", "ENABLE_PHYSICS", "ENABLE_PHYSICS2D", "ENABLE_AUDIO",
        "ENABLE_ANIMATION", "ENABLE_CLOTH", "ENABLE_TERRAIN", "ENABLE_VR", "ENABLE_UNITYWEBREQUEST",
        "UNITY_ASSERTIONS", "DEVELOPMENT_BUILD", "DEBUG", "CSHARP_7_3_OR_NEWER",
        f"UNITY_{major}", f"UNITY_{major}_{minor}", f"UNITY_{major}_{minor}_{patch}",
    }
    for unity_minor in range(7):
        defines.add(f"UNITY_5_{unity_minor}_OR_NEWER")
    for year in range(2017, 2024):
        defines.add(f"UNITY_{year}_1_OR_NEWER")
        defines.add(f"UNITY_{year}_2_OR_NEWER")
        defines.add(f"UNITY_{year}_3_OR_NEWER")
    if major >= 6000:
        for unity_minor in range(minor + 1):
            defines.add(f"UNITY_6000_{unity_minor}_OR_NEWER")
    settings = project / "ProjectSettings/ProjectSettings.asset"
    compatibility = 6
    if settings.is_file():
        text = settings.read_text()
        match = re.search(r"^  scriptingDefineSymbols:\s*\n((?:    .*\n)*)", text, re.MULTILINE)
        if match:
            for target, value in re.findall(r"^    ([^:]+):\s*(.*)$", match.group(1), re.MULTILINE):
                if target in {"Standalone", "LinuxStandalone64", "1"}:
                    defines.update(value.strip("\"'").split(";"))
        input_mode = re.search(r"^  activeInputHandler:\s*(\d+)", text, re.MULTILINE)
        mode = int(input_mode.group(1)) if input_mode else 0
        api = re.search(r"^  apiCompatibilityLevel:\s*(\d+)", text, re.MULTILINE)
        if api:
            compatibility = int(api.group(1))
        platform_api = re.search(r"^  apiCompatibilityLevelPerPlatform:\s*\n((?:    .*\n)*)", text, re.MULTILINE)
        if platform_api:
            for target, value in re.findall(r"^    ([^:]+):\s*(\d+)$", platform_api.group(1), re.MULTILINE):
                if target in {"Standalone", "LinuxStandalone64", "1"}:
                    compatibility = int(value)
    else:
        mode = 0
    if mode in {0, 2}:
        defines.add("ENABLE_LEGACY_INPUT_MANAGER")
    if mode in {1, 2}:
        defines.add("ENABLE_INPUT_SYSTEM")
    if compatibility == 6:
        defines.update({"NET_STANDARD_2_1", "NET_STANDARD", "NETSTANDARD2_1", "NETSTANDARD"})
    elif compatibility == 3:
        defines.add("NET_4_6")
    else:
        raise RawCompileError(f"Unsupported raw compiler API compatibility level: {compatibility}")
    return {value for value in defines if value}


def _constraint(value: str, defines: set[str]) -> bool:
    # Unity supports OR within one constraint and AND between entries.
    return any((term[1:] not in defines if term.startswith("!") else term in defines)
               for term in (part.strip() for part in value.split("||")))


def _matches_version(version: str, expression: str) -> bool:
    if not expression.strip():
        return True
    def parts(value):
        unity_release = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)([abfp])(\d+)", value.strip())
        if unity_release:
            major, minor, patch, phase, revision = unity_release.groups()
            return (int(major), int(minor), int(patch), {"a": -2, "b": -1, "f": 1, "p": 2}[phase], ((0, int(revision)),))
        match = re.fullmatch(r"(\d+)(?:\.(\d+))?(?:\.(\d+))?(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?", value.strip())
        if not match:
            raise RawCompileError(f"Unsupported version expression: {expression}")
        major, minor, patch, prerelease = match.groups()
        suffix = tuple((0, int(token)) if token.isdigit() else (1, token)
                       for token in prerelease.split(".")) if prerelease else ()
        return (int(major), int(minor or 0), int(patch or 0), 0 if prerelease else 1, suffix)
    current = parts(version)
    if expression.startswith(("[", "(")):
        if not expression.endswith(("]", ")")):
            raise RawCompileError(f"Unsupported version range: {expression}")
        body = expression[1:-1]
        if "," not in body:
            return current == parts(body)
        low, high = body.split(",", 1)
        return ((not low.strip() or current >= parts(low) if expression[0] == "[" else
                 not low.strip() or current > parts(low)) and
                (not high.strip() or current <= parts(high) if expression[-1] == "]" else
                 not high.strip() or current < parts(high)))
    return current >= parts(expression)


def _graph(project: Path, roots: list[Path], defines: set[str], versions: dict[str, str], host_names: set[str] | None = None,
           inventory: _SourceInventory | None = None) -> dict[str, Assembly]:
    assemblies, owners, guids = {}, {}, {}
    for root in roots:
        for path in _files(root, "*.asmdef", inventory):
            config = _json(path)
            name = config.get("name", "")
            if not re.fullmatch(r"[A-Za-z0-9_.-]+", name):
                raise RawCompileError(f"Unsafe or missing assembly name in {path}")
            if name in assemblies:
                raise RawCompileError(f"Duplicate assembly name {name}")
            local_defines = set(defines)
            for rule in config.get("versionDefines", []):
                version = versions.get(rule.get("name"))
                if version and _matches_version(version, rule.get("expression", "")):
                    local_defines.add(rule["define"])
            include, exclude = config.get("includePlatforms", []), config.get("excludePlatforms", [])
            active = (not include or "LinuxStandalone64" in include) and "LinuxStandalone64" not in exclude
            active = active and "TestAssemblies" not in config.get("optionalUnityReferences", [])
            active = active and all(_constraint(value, local_defines) for value in config.get("defineConstraints", []))
            if host_names and root != project / "Assets":
                # The native host build is authoritative for package player
                # inclusion, including tests, platform constraints and ILPP.
                active = name in host_names
            assembly = Assembly(name, path.parent, config, defines=local_defines, active=active)
            assemblies[name], owners[path.parent.resolve()] = assembly, assembly
            guid = _guid(path)
            if guid:
                if guid in guids:
                    raise RawCompileError(f"Duplicate assembly GUID {guid}")
                guids[guid] = name
    for name in host_names or set():
        assemblies.setdefault(name, Assembly(name, project / "Packages", active=True))
    default = Assembly("Assembly-CSharp", project / "Assets", defines=set(defines))
    if default.name in assemblies:
        raise RawCompileError("Assembly-CSharp is reserved for sources without an asmdef")
    assemblies[default.name] = default
    for root in roots:
        for path in _files(root, "*.asmref", inventory):
            reference = _json(path).get("reference", "")
            name = guids.get(reference[5:].lower()) if reference.startswith("GUID:") else reference
            if name not in assemblies:
                raise RawCompileError(f"Unknown assembly reference {reference} in {path}")
            if path.parent.resolve() in owners:
                raise RawCompileError(f"Multiple assembly definitions/references in {path.parent}")
            owners[path.parent.resolve()] = assemblies[name]
    for root in roots:
        canonical_root = root.resolve()
        for source in _files(root, "*.cs", inventory):
            parent = source.parent.resolve()
            owner = None
            while _within(parent, canonical_root):
                if parent in owners:
                    owner = owners[parent]
                    break
                if parent == canonical_root:
                    break
                parent = parent.parent
            if owner is None:
                if "Editor" in source.parts[len(root.parts):]:
                    continue
                if root != project / "Assets":
                    if host_names:
                        continue
                    raise RawCompileError(f"Package C# source has no asmdef: {source}")
                owner = default
            owner.sources.append(source)
    for assembly in assemblies.values():
        if not assembly.active or assembly.name in (host_names or set()):
            continue
        for reference in assembly.config.get("references", []):
            name = guids.get(reference[5:].lower()) if reference.startswith("GUID:") else reference
            if name not in assemblies:
                raise RawCompileError(f"Assembly {assembly.name} references missing assembly {reference}")
            assembly.dependencies.append(name)
    default.dependencies = [name for name, assembly in assemblies.items()
                            if name != default.name and assembly.active and assembly.sources
                            and assembly.config.get("autoReferenced", True)]
    return assemblies


def _order(assemblies: dict[str, Assembly], host_names: set[str]) -> list[Assembly]:
    result, visited, visiting = [], set(), set()
    def visit(name):
        if name in visited or name in host_names:
            return
        if name in visiting:
            raise RawCompileError(f"Cyclic assembly dependency: {name}")
        assembly = assemblies[name]
        if not assembly.active:
            raise RawCompileError(f"Runtime assembly depends on excluded assembly {name}")
        visiting.add(name)
        for dependency in assembly.dependencies:
            visit(dependency)
        visiting.remove(name)
        visited.add(name)
        if assembly.sources:
            result.append(assembly)
    for name, assembly in assemblies.items():
        if assembly.active and assembly.sources:
            visit(name)
    return result


def _verify_host_bridge(assembly: Assembly, host_assemblies: Path, inventory: _SourceInventory | None = None) -> None:
    metadata = Path(host_assemblies).resolve().parent.parent / "host.json"
    if not metadata.is_file():
        raise RawCompileError("Dungeon.CLIPlay source cannot be skipped without the host sourceFiles compatibility manifest")
    records = _json(metadata).get("sourceFiles", [])
    expected = {record.get("path"): record.get("sha256") for record in records}
    expected_sources = {name for name in expected if isinstance(name, str) and name.startswith("Runtime/") and name.endswith(".cs")}
    actual_sources = {"Runtime/" + source.relative_to(assembly.root).as_posix() for source in assembly.sources}
    if actual_sources != expected_sources:
        raise RawCompileError("Project Dungeon.CLIPlay differs from bundled host bridge sources; rebuild the universal host or rename the game assembly")
    checked = [*assembly.sources, *_files(assembly.root, "*.asmdef", inventory), *_files(assembly.root, "*.asmref", inventory)]
    for path in checked:
        logical = "Runtime/" + path.relative_to(assembly.root).as_posix()
        if expected.get(logical) != hashlib.sha256(path.read_bytes()).hexdigest():
            raise RawCompileError(f"Project bridge differs from prepared host: {path}; rebuild the universal host")


def _check_inputs(project: Path, roots: list[Path], source_inputs: dict, compiler_inputs: dict,
                  inventory: _SourceInventory | None = None) -> None:
    if _source_inputs(project, roots, inventory) != source_inputs:
        raise RawCompileError("Raw compiler inputs changed during compilation; no DLLs published. Retry with stable sources.")
    for name, expected in compiler_inputs.items():
        if source_inputs.get(name) == expected:
            # Already byte-checked by the fresh full source inventory above.
            continue
        path = Path(name)
        if not path.is_file() or hashlib.sha256(path.read_bytes()).hexdigest() != expected:
            raise RawCompileError(f"Raw compiler input changed during preparation: {path}; retry")


@contextmanager
def _cache_lock(cache: Path, key: str):
    cache.mkdir(parents=True, exist_ok=True)
    with (cache / (key + ".lock")).open("a") as stream:
        fcntl.flock(stream, fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(stream, fcntl.LOCK_UN)


def _valid_lifecycle_proof(proof: dict, assembly: dict) -> bool:
    phases = {"Awake", "OnEnable", "OnDisable", "OnDestroy"}
    if not isinstance(proof, dict) or proof.get("schemaVersion") != 1 or proof.get("verified") is not True:
        return False
    if not isinstance(proof.get("phases"), list) or len(proof["phases"]) != 4 or set(proof["phases"]) != phases:
        return False
    methods = proof.get("methods")
    if not isinstance(methods, list) or type(proof.get("callbacks")) is not int or proof["callbacks"] != len(methods):
        return False
    if type(proof.get("scriptableObjectBarrier")) is not bool:
        return False
    if any(type(proof.get(field)) is not int or proof[field] < 0 for field in ("activeAndEnabledReads", "setActiveCalls")):
        return False
    if any(type(proof.get(field, 0)) is not int or proof.get(field, 0) < 0
           for field in ("transformRootReads", "getComponentInChildrenCalls")):
        return False
    sources = assembly.get("sources", [])
    for method in methods:
        if (not isinstance(method, dict) or not isinstance(method.get("Type"), str) or not method["Type"]
                or method.get("Phase") not in phases or method.get("Source") not in sources
                or type(method.get("Order")) is not int or type(method.get("Synthetic")) is not bool):
            return False
    initializers = proof.get("runtimeInitializers")
    if not isinstance(initializers, list):
        return False
    for initializer in initializers:
        if (not isinstance(initializer, dict) or initializer.get("assembly") != assembly["name"]
                or not isinstance(initializer.get("typeName"), str) or not initializer["typeName"]
                or not isinstance(initializer.get("methodName"), str)
                or not re.fullmatch(r"__UnityPlay_RuntimeInitialize_\d+", initializer["methodName"])
                or type(initializer.get("phase")) is not int or not 0 <= initializer["phase"] <= 4):
            return False
    return True


def _read_cache_unlocked(entry: Path, key: str, inputs: dict) -> tuple[dict, dict[str, bytes]] | None:
    try:
        metadata = _json(entry / "compile.json")
        if metadata.get("cacheSchema") != 1 or metadata.get("compilerInputHash") != key or metadata.get("compilerInputs") != inputs:
            return None
        assemblies = metadata["assemblies"]
        files = metadata["cacheFiles"]
        if not isinstance(assemblies, list) or not isinstance(files, dict):
            return None
        if "assemblyConfiguration" in inputs:
            configuration = metadata.get("assemblyConfiguration")
            if not isinstance(configuration, dict) or len(assemblies) != 1:
                return None
            digest = hashlib.sha256(json.dumps(configuration, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
            assembly = assemblies[0]
            if (digest != inputs["assemblyConfiguration"] or assembly.get("name") != configuration.get("name")
                    or assembly.get("sources") != configuration.get("sources")
                    or assembly.get("references") != configuration.get("dependencies")):
                return None
        enabled = str(Path(__file__).resolve().parent / "LifecycleCompiler" / "Program.cs") in inputs
        if metadata.get("capabilities", {}).get("lifecycleBarrier") is not enabled:
            return None
        names = set()
        for assembly in assemblies:
            name = assembly["name"]
            if not re.fullmatch(r"[A-Za-z0-9_.-]+", name) or name in names:
                return None
            names.add(name)
            if files.get(name + ".dll") != assembly["sha256"]:
                return None
            if enabled and not _valid_lifecycle_proof(assembly.get("lifecycleBarrier"), assembly):
                return None
        payloads = {}
        for filename, expected in files.items():
            path = Path(filename)
            if path.name != filename or path.suffix not in {".dll", ".pdb"} or path.stem not in names:
                return None
            content = (entry / filename).read_bytes()
            if hashlib.sha256(content).hexdigest() != expected:
                return None
            payloads[filename] = content
        if {name + suffix for name in names for suffix in (".dll", ".pdb")} - set(payloads):
            return None
        return metadata, payloads
    except (OSError, ValueError, TypeError, KeyError):
        return None


def _read_cache(cache: Path, key: str, inputs: dict) -> tuple[dict, dict[str, bytes]] | None:
    with _cache_lock(cache, key):
        return _read_cache_unlocked(cache / key, key, inputs)


def _write_cache(cache: Path, key: str, manifest: dict, output: Path) -> None:
    with _cache_lock(cache, key):
        entry = cache / key
        if _read_cache_unlocked(entry, key, manifest["compilerInputs"]) is not None:
            return
        with tempfile.TemporaryDirectory(prefix=".publish-", dir=cache) as temporary:
            staging = Path(temporary) / "entry"
            staging.mkdir()
            files = {}
            for assembly in manifest["assemblies"]:
                for suffix in (".dll", ".pdb"):
                    filename = assembly["name"] + suffix
                    source = output / filename
                    if suffix == ".pdb" and not source.exists():
                        continue
                    content = source.read_bytes()
                    digest = hashlib.sha256(content).hexdigest()
                    if suffix == ".dll" and digest != assembly["sha256"]:
                        raise RawCompileError(f"Compiled DLL changed before cache publication: {source}")
                    (staging / filename).write_bytes(content)
                    files[filename] = digest
            metadata = dict(manifest, cacheSchema=1, cacheFiles=files)
            (staging / "compile.json").write_text(json.dumps(metadata, indent=2) + "\n")
            # A corrupt entry is removed only while owning its per-key lock.
            # Successful entries are never rewritten, and directories publish atomically.
            if entry.exists():
                if entry.is_dir() and not entry.is_symlink():
                    shutil.rmtree(entry)
                else:
                    entry.unlink()
            staging.rename(entry)


def _assembly_configuration(assembly: Assembly, native_base_assemblies: set[str]) -> dict:
    return {"cacheSchema": 1, "name": assembly.name, "root": str(assembly.root),
            "config": assembly.config, "defines": sorted(assembly.defines),
            "sources": [str(source) for source in assembly.sources],
            "dependencies": assembly.dependencies,
            "nativeBaseAssemblies": sorted(native_base_assemblies)}


def _assembly_cache_inputs(assembly: Assembly, source_inputs: dict, compiler_inputs: dict,
                           references: list[Path], native_base_assemblies: set[str]) -> dict:
    # Other game assemblies' source bytes must not invalidate this assembly.
    # Their emitted DLL hashes are semantic inputs below, so dependents still
    # rebuild whenever the compiled dependency changes.
    inputs = {name: digest for name, digest in compiler_inputs.items() if name not in source_inputs}
    for source in assembly.sources:
        for path in (source, source.with_name(source.name + ".meta")):
            name = str(path.resolve())
            if name in source_inputs:
                inputs[name] = source_inputs[name]
    configuration = _assembly_configuration(assembly, native_base_assemblies)
    inputs["assemblyConfiguration"] = hashlib.sha256(json.dumps(configuration, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
    # Staging paths change between compilations. DLL filenames and contents are
    # stable identities; actual reference paths are still used by Roslyn.
    reference_inputs = [{"name": path.name, "sha256": compiler_inputs.get(str(path.resolve())) or hashlib.sha256(path.read_bytes()).hexdigest()}
                        for path in dict.fromkeys(references)]
    inputs["assemblyReferences"] = hashlib.sha256(json.dumps(reference_inputs, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
    return inputs


def _lifecycle_helper(dotnet: Path, compiler: Path, inputs: dict[str, str]) -> Path:
    """Build the SDK-only semantic rewriter once, outside authored projects."""
    source_root = Path(__file__).resolve().parent / "LifecycleCompiler"
    paths = [source_root / "LifecycleCompiler.csproj", source_root / "Program.cs"]
    identity = {str(path): inputs[str(path.resolve())] for path in paths}
    identity.update({name: digest for name, digest in inputs.items()
                     if _within(Path(name), dotnet.parent)})
    key = hashlib.sha256(json.dumps(identity, sort_keys=True).encode()).hexdigest()
    cache = Path(tempfile.gettempdir()) / f"unity-play-lifecycle-compiler-{os.getuid()}"
    with _cache_lock(cache, key):
        entry = cache / key
        metadata = entry / "files.json"
        try:
            records = json.loads(metadata.read_text())
            if isinstance(records, dict) and records and all(Path(name).name == name and (entry / name).is_file() and
                               hashlib.sha256((entry / name).read_bytes()).hexdigest() == digest
                               for name, digest in records.items()):
                return entry / "LifecycleCompiler.dll"
        except (OSError, ValueError, TypeError):
            pass
        with tempfile.TemporaryDirectory(prefix=".build-", dir=cache) as temporary:
            build_root = Path(temporary)
            for path in paths:
                content = path.read_bytes()
                if hashlib.sha256(content).hexdigest() != inputs[str(path.resolve())]:
                    raise RawCompileError("Lifecycle compiler source changed while preparing compilation")
                (build_root / path.name).write_bytes(content)
            # SDK reference packs and Roslyn ship with Unity; no packages or
            # editor process are needed and no obj/bin directories touch Assets.
            (build_root / "NuGet.Config").write_text(
                '<configuration><packageSources><clear /></packageSources></configuration>\n')
            payload = build_root / "payload"
            try:
                run = subprocess.run([str(dotnet), "build", str(build_root / paths[0].name),
                                      "--nologo", "-v:q", "-o", str(payload),
                                      "-p:RoslynPath=" + str(compiler.parent),
                                      "-p:NuGetAudit=false"], cwd=build_root,
                                     capture_output=True, text=True, timeout=180)
            except subprocess.TimeoutExpired as error:
                raise RawCompileError("Lifecycle compiler helper build timed out") from error
            if run.returncode or not (payload / "LifecycleCompiler.dll").is_file():
                raise RawCompileError("Lifecycle compiler helper build failed:\n" + run.stdout + run.stderr)
            files = {path.name: hashlib.sha256(path.read_bytes()).hexdigest()
                     for path in payload.iterdir() if path.is_file()}
            (payload / "files.json").write_text(json.dumps(files, sort_keys=True))
            if entry.exists():
                shutil.rmtree(entry)
            payload.rename(entry)
        return entry / "LifecycleCompiler.dll"


def _instrument_lifecycle(dotnet: Path, helper: Path, assembly: Assembly,
                          references: list[Path], snapshots: dict[Path, Path],
                          staging: Path, native_base_assemblies: list[str] | None = None) -> tuple[list[Path], dict]:
    folder = staging / "Lifecycle" / assembly.name
    folder.mkdir(parents=True)
    sources = [{"Path": str(snapshots[source]), "OriginalPath": str(source),
                "OutputPath": str(folder / str(index) / source.name)}
               for index, source in enumerate(assembly.sources)]
    marker = folder / "UnityPlayLifecycleMarker.cs"
    request = folder / "request.json"
    execution_orders = {}
    for source in assembly.sources:
        meta = source.with_name(source.name + ".meta")
        if meta.is_file():
            setting = re.search(r"^\s+executionOrder:\s*(-?\d+)\s*$", meta.read_text(), re.MULTILINE)
            if setting and int(setting.group(1)):
                execution_orders[str(source)] = int(setting.group(1))
    request.write_text(json.dumps({"AssemblyName": assembly.name,
                                  "ExecutionOrders": execution_orders,
                                  "NativeBaseAssemblies": native_base_assemblies or [],
                                  "References": [str(path) for path in dict.fromkeys(references)],
                                  "Defines": sorted(assembly.defines), "Sources": sources,
                                  "MarkerPath": str(marker),
                                  "AllowUnsafe": bool(assembly.config.get("allowUnsafeCode"))}))
    try:
        run = subprocess.run([str(dotnet), str(helper), str(request)],
                             cwd=staging, capture_output=True, text=True, timeout=180)
    except subprocess.TimeoutExpired as error:
        raise RawCompileError(f"Lifecycle verification timed out for {assembly.name}") from error
    if run.returncode:
        raise RawCompileError(f"Lifecycle verification failed for {assembly.name}:\n" + run.stdout + run.stderr)
    try:
        proof = json.loads(run.stdout)
        if proof["SchemaVersion"] != 1 or proof["Verified"] is not True or set(proof["Phases"]) != {
                "Awake", "OnEnable", "OnDisable", "OnDestroy"}:
            raise ValueError("invalid lifecycle proof")
        callbacks = proof["Callbacks"]
        if not isinstance(callbacks, list) or not marker.is_file():
            raise ValueError("missing callbacks or marker")
    except (ValueError, KeyError, TypeError) as error:
        raise RawCompileError(f"Invalid lifecycle compiler result for {assembly.name}: {error}") from error
    paths = [Path(source["OutputPath"]) for source in sources] + [marker]
    return paths, {"schemaVersion": 1, "verified": True, "callbacks": len(callbacks),
                   "phases": proof["Phases"], "methods": callbacks,
                   "activeAndEnabledReads": proof.get("ActiveAndEnabledReads", 0),
                   "setActiveCalls": proof.get("SetActiveCalls", 0),
                   "transformRootReads": proof.get("TransformRootReads", 0),
                   "getComponentInChildrenCalls": proof.get("GetComponentInChildrenCalls", 0),
                   "scriptableObjectBarrier": proof.get("ScriptableObjectBarrier", False),
                   "runtimeInitializers": [{"assembly": item["Assembly"], "typeName": item["TypeName"],
                                            "methodName": item["MethodName"], "phase": item["Phase"]}
                                           for item in proof.get("RuntimeInitializers", [])]}


def compile_project(project: Path, unity: Path, output: Path, host_assemblies: Path | None = None,
                    cache_dir: Path | None = None) -> dict:
    """Compile a fresh raw project into Linux Mono DLLs; raise on unsupported inputs.

    Host DLLs are references only. Dungeon.CLIPlay is the sole source assembly
    owned by the universal host; a conflicting game assembly is an error.
    No binaries are published on compilation failure. An optional external code
    cache uses full source/configuration/SDK/host input hashes; every cached DLL
    and PDB is verified before copying it into a new immutable content revision.
    """
    started = time.monotonic()
    project, output = Path(project).resolve(), Path(output).resolve()
    data = _data(Path(unity))
    _without_project_library(project, data)
    if not (project / "Assets").is_dir():
        raise RawCompileError(f"Raw project Assets directory is missing: {project}")
    _without_project_library(project, project / "Assets")
    if _within(output, project / "Assets"):
        raise RawCompileError("Compiler output cannot be placed in raw Assets")
    cache = Path(cache_dir).resolve() if cache_dir is not None else None
    if cache is not None and _within(cache, project / "Assets"):
        raise RawCompileError("Compiler cache cannot be placed in raw Assets")
    packages, versions = _package_roots(project, data)
    roots = [project / "Assets", *packages]
    inventory = _SourceInventory()
    source_inputs = _source_inputs(project, roots, inventory)
    checked_packages, checked_versions = _package_roots(project, data)
    if checked_packages != packages or checked_versions != versions:
        raise RawCompileError("Package graph changed while preparing raw compilation; retry")
    defines = _base_defines(project)
    requested_version = re.search(r"m_EditorVersion:\s*(\S+)",
                                 (project / "ProjectSettings/ProjectVersion.txt").read_text()).group(1)
    installed_version = data.parent.parent.name
    if re.fullmatch(r"\d+\.\d+\.\d+[abfp]\d+", installed_version) and requested_version != installed_version:
        raise RawCompileError(f"Project requires Unity {requested_version}, SDK is {installed_version}; use matching host and SDK")
    constraint_versions = dict(versions)
    constraint_versions["Unity"] = requested_version
    if host_assemblies:
        _without_project_library(project, Path(host_assemblies))
    host = {path.stem: path for path in Path(host_assemblies).resolve().glob("*.dll")} if host_assemblies else {}
    host_metadata_path = Path(host_assemblies).resolve().parent.parent / "host.json" if host_assemblies else None
    host_metadata_info = _json(host_metadata_path) if host_metadata_path and host_metadata_path.is_file() else {}
    package_host_names = set(host_metadata_info.get("providedPackageAssemblies", [])) & set(host)
    if package_host_names:
        expected_packages = {record["name"]: record for record in host_metadata_info.get("packages", [])}
        for root in packages:
            info = _json(root / "package.json")
            expected = expected_packages.get(info["name"])
            files = {path.relative_to(root).as_posix(): _captured_hash(path, source_inputs)
                     for pattern in ("*.cs", "*.asmdef", "*.asmref", "*.dll", "*.rsp") for path in _files(root, pattern, inventory)}
            if not expected or expected.get("version") != info.get("version") or expected.get("sourceHashes") != files:
                raise RawCompileError(f"Package {info['name']} differs from the host's native package baseline; rebuild host --project-packages")
    assemblies = _graph(project, roots, defines, constraint_versions, package_host_names, inventory)
    owned = ({"Dungeon.CLIPlay"} & set(host)) | package_host_names
    for name in package_host_names & set(assemblies):
        if assemblies[name].sources and any(_within(source, project / "Assets") for source in assemblies[name].sources):
            raise RawCompileError(f"Host package assembly {name} conflicts with project Assets sources")
    if "Dungeon.CLIPlay" in owned and "Dungeon.CLIPlay" in assemblies:
        _verify_host_bridge(assemblies["Dungeon.CLIPlay"], Path(host_assemblies), inventory)
    for name in set(assemblies) & set(host) - owned:
        if assemblies[name].sources and assemblies[name].active:
            raise RawCompileError(f"Project assembly {name} is already bundled in host; use a generic host without game code")
    order = _order(assemblies, owned)
    for root in roots:
        if package_host_names and root in packages:
            continue
        for extension in ("*.dll", "*.rsp"):
            for path in _files(root, extension, inventory):
                raise RawCompileError(f"Raw plugin/response import settings are not supported yet: {path}")
    lifecycle_enabled = "Dungeon.CLIPlay" in host
    for assembly in order:
        if assembly.config.get("overrideReferences") or assembly.config.get("precompiledReferences"):
            raise RawCompileError(f"Assembly {assembly.name} needs precompiled plugin references, not supported yet")
        if any(name.startswith(("Unity.Entities", "Unity.Jobs.CodeGen", "Unity.NetCode")) for name in assembly.dependencies):
            raise RawCompileError(f"Assembly {assembly.name} requires unsupported IL postprocessing")
        for source in assembly.sources:
            text = source.read_text(encoding="utf-8-sig")
            if not lifecycle_enabled and re.search(r"\[\s*(?:UnityEngine\.)?RuntimeInitializeOnLoadMethod(?:Attribute)?\b", text):
                raise RawCompileError(f"RuntimeInitializeOnLoadMethod registration is not supported in raw plugins: {source}")
            if re.search(r"\[\s*(?:Unity\.Burst\.)?BurstCompile(?:Attribute)?\b", text):
                raise RawCompileError(f"Burst native compilation is not supported in raw plugins: {source}")
            if "ILPostProcessor" in text:
                raise RawCompileError(f"IL postprocessors are not supported in raw compilation: {source}")
    sdk = data / "DotNetSdk"
    compilers = sorted(sdk.glob("sdk/*/Roslyn/bincore/csc.dll"),
                       key=lambda p: tuple(int(value) for value in re.findall(r"\d+", p.parents[2].name)))
    dotnet = sdk / "dotnet"
    if not dotnet.is_file() or not compilers:
        raise RawCompileError(f"Bundled Roslyn SDK is missing: {sdk}")
    bcl = data / "MonoBleedingEdge/lib/mono/unityaot-linux"
    engine = data / "PlaybackEngines/LinuxStandaloneSupport/Variations/mono/Managed"
    base = {path.name: path for path in sorted(bcl.glob("*.dll"))}
    for path in sorted((bcl / "Facades").glob("*.dll")):
        base.setdefault(path.name, path)
    if "mscorlib.dll" not in base or not engine.is_dir():
        raise RawCompileError("Linux Mono runtime/reference assemblies are missing from Unity installation")
    engine_refs = sorted(engine.glob("UnityEngine*.dll")) + sorted(engine.glob("Unity.Scripting.dll"))
    installation_names = set(base) | {path.name for path in engine_refs}
    host_references = [path for path in host.values() if path.name not in installation_names]
    lifecycle_enabled = "Dungeon.CLIPlay" in host
    compiler_inputs = dict(source_inputs)
    host_metadata = Path(host_assemblies).resolve().parent.parent / "host.json" if host_assemblies else None
    metadata_inputs = [host_metadata] if host_metadata is not None and host_metadata.is_file() else []
    compiler_runtime = [*compilers[-1].parent.glob("*.dll"), *compilers[-1].parent.glob("*.json"),
                        *sdk.glob("shared/*/*/.version"), *sdk.glob("host/fxr/*/libhostfxr.so")]
    helper_inputs = [Path(__file__).resolve().parent / "LifecycleCompiler" / name
                     for name in ("LifecycleCompiler.csproj", "Program.cs")] if lifecycle_enabled else []
    generator_directory = data / "Tools/BuildPipeline/Unity.SourceGenerators"
    # Unity's own generators define the managed ABI serialized into imported
    # UXML, including each [UxmlElement]'s nested UxmlSerializedData class.
    # Reuse the matching installed SDK implementations rather than imitating
    # their serialization or producing empty compatibility types.
    source_generators = [generator_directory / name for name in
                         ("Unity.Properties.SourceGenerator.dll", "Unity.SourceGenerators.dll", "Unity.UIToolkit.SourceGenerator.dll")]
    if not all(path.is_file() for path in source_generators):
        raise RawCompileError(f"Unity source generators are missing from the selected SDK: {generator_directory}")
    for path in [Path(__file__).resolve(), dotnet, *compiler_runtime, *base.values(), *engine_refs, *host.values(), *metadata_inputs, *helper_inputs, *source_generators]:
        compiler_inputs[str(_without_project_library(project, path))] = hashlib.sha256(path.read_bytes()).hexdigest()
    compiler_hash = hashlib.sha256(json.dumps(compiler_inputs, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
    output.mkdir(parents=True, exist_ok=True)
    cached = _read_cache(cache, compiler_hash, compiler_inputs) if cache is not None else None
    if cached is not None:
        manifest, payloads = cached
        with tempfile.TemporaryDirectory(prefix=".raw-cache-", dir=output) as temporary:
            staging = Path(temporary)
            for filename, content in payloads.items():
                (staging / filename).write_bytes(content)
            _check_inputs(project, roots, source_inputs, compiler_inputs, inventory)
            for path in staging.iterdir():
                shutil.copy2(path, output / path.name)
        manifest.pop("cacheSchema", None)
        manifest.pop("cacheFiles", None)
        for assembly in manifest["assemblies"]:
            assembly["path"] = str(output / (assembly["name"] + ".dll"))
            assembly["cacheHit"] = True
        manifest.update(cacheHit=True, compiledAssemblyCount=0, reusedAssemblyCount=len(manifest["assemblies"]),
                        elapsedSeconds=time.monotonic() - started)
        (output / "compile.json").write_text(json.dumps(manifest, indent=2) + "\n")
        return manifest
    diagnostics, published, scripts = [], [], []
    for name in package_host_names:
        for source in assemblies[name].sources:
            guid = _guid(source)
            if guid:
                scripts.append({"guid": guid, "path": str(source), "assembly": name, "hostProvided": True})
    lifecycle_helper = None
    assembly_entries = []
    with tempfile.TemporaryDirectory(prefix=".raw-compile-", dir=output) as temporary:
        staging = Path(temporary)
        snapshot_sources = {}
        for index, source in enumerate(dict.fromkeys(source for assembly in order for source in assembly.sources)):
            content = source.read_bytes()
            if hashlib.sha256(content).hexdigest() != source_inputs[str(source.resolve())]:
                raise RawCompileError(f"Source changed while preparing raw compilation: {source}; retry")
            snapshot = staging / "Sources" / str(index) / source.name
            snapshot.parent.mkdir(parents=True, exist_ok=True)
            snapshot.write_bytes(content)
            snapshot_sources[source] = snapshot
        built = {}
        for assembly in order:
            references = [*base.values()]
            if not assembly.config.get("noEngineReferences"):
                references.extend(engine_refs)
            references.extend(host_references)
            for dependency in assembly.dependencies:
                if dependency in built:
                    references.append(built[dependency])
                elif dependency not in host:
                    raise RawCompileError(f"Assembly {assembly.name} has dependency without emitted DLL: {dependency}")
            assembly_inputs = _assembly_cache_inputs(assembly, source_inputs, compiler_inputs, references, package_host_names)
            assembly_hash = hashlib.sha256(json.dumps(assembly_inputs, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
            assembly_cached = _read_cache(cache / "assemblies", assembly_hash, assembly_inputs) if cache is not None else None
            dll = staging / f"{assembly.name}.dll"
            if assembly_cached is not None:
                descriptor, payloads = assembly_cached
                # Each per-assembly entry owns exactly this assembly. A full
                # project entry or changed source identity cannot be reused.
                cached_assemblies = descriptor["assemblies"]
                if (len(cached_assemblies) != 1 or cached_assemblies[0].get("name") != assembly.name
                        or cached_assemblies[0].get("sources") != [str(source) for source in assembly.sources]
                        or cached_assemblies[0].get("references") != assembly.dependencies):
                    assembly_cached = None
                else:
                    for filename, content in payloads.items():
                        (staging / filename).write_bytes(content)
                    result = dict(cached_assemblies[0], path=str(output / dll.name), cacheHit=True,
                                  assemblyInputHash=assembly_hash)
                    published.append(result)
                    diagnostics.extend(descriptor.get("diagnostics", []))
                    built[assembly.name] = dll
                    for source in assembly.sources:
                        guid = _guid(source)
                        if guid:
                            scripts.append({"guid": guid, "path": str(source), "assembly": assembly.name})
                    continue
            lifecycle_proof = None
            emitted_sources = [snapshot_sources[source] for source in assembly.sources]
            if lifecycle_enabled:
                if lifecycle_helper is None:
                    lifecycle_helper = _lifecycle_helper(dotnet, compilers[-1], compiler_inputs)
                emitted_sources, lifecycle_proof = _instrument_lifecycle(
                    dotnet, lifecycle_helper, assembly, references, snapshot_sources, staging, sorted(package_host_names))
            arguments = ["-nologo", "-target:library", "-nostdlib+", "-langversion:9.0", "-deterministic+",
                         "-debug:portable", "-optimize-", "-unsafe+" if assembly.config.get("allowUnsafeCode") else "-unsafe-",
                         f"-out:{dll}", "-define:" + ";".join(sorted(assembly.defines))]
            arguments.extend(f"-reference:{path}" for path in dict.fromkeys(references))
            arguments.extend(f"-analyzer:{path}" for path in source_generators)
            generated = staging / "Generated" / assembly.name
            generated.mkdir(parents=True)
            additional_file = staging / (assembly.name + ".UnityAdditionalFile.txt")
            additional_file.write_text(str(project))
            arguments.extend((f"-generatedfilesout:{generated}", f"-additionalfile:{additional_file}"))
            arguments.extend(str(source) for source in emitted_sources)
            response = staging / f"{assembly.name}.rsp"
            response.write_text("\n".join('"' + value.replace('"', '\\"') + '"' for value in arguments) + "\n")
            try:
                run = subprocess.run([str(dotnet), str(compilers[-1]), "@" + str(response)],
                                     cwd=project, capture_output=True, text=True, timeout=180)
            except subprocess.TimeoutExpired as error:
                raise RawCompileError(f"Compiler timed out for {assembly.name}") from error
            assembly_diagnostics = []
            for line in (run.stdout + run.stderr).splitlines():
                for source in assembly.sources:
                    line = line.replace(str(snapshot_sources[source]), str(source))
                if line:
                    diagnostics.append(line)
                    assembly_diagnostics.append(line)
            generator_failed = any(re.search(r"\bCS(?:8032|8784|8785)\b", line) for line in assembly_diagnostics)
            if run.returncode or not dll.is_file() or generator_failed:
                raise RawCompileError(f"Cold compilation failed for {assembly.name}:\n" + "\n".join(diagnostics[-40:]))
            built[assembly.name] = dll
            published.append({"name": assembly.name, "path": str(output / dll.name),
                              "sources": [str(source) for source in assembly.sources],
                              "references": assembly.dependencies,
                              "cacheHit": False, "assemblyInputHash": assembly_hash,
                              "generatedSources": [{"path": path.relative_to(generated).as_posix(),
                                                     "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
                                                    for path in sorted(generated.rglob("*.cs"))],
                              "sha256": hashlib.sha256(dll.read_bytes()).hexdigest()})
            if lifecycle_proof is not None:
                published[-1]["lifecycleBarrier"] = lifecycle_proof
            assembly_entries.append((assembly_hash, {"compilerInputHash": assembly_hash, "compilerInputs": assembly_inputs,
                                     "assemblyConfiguration": _assembly_configuration(assembly, package_host_names),
                                     "assemblies": [published[-1]], "diagnostics": assembly_diagnostics,
                                     "capabilities": {"lifecycleBarrier": lifecycle_enabled}}))
            for source in assembly.sources:
                guid = _guid(source)
                if guid:
                    scripts.append({"guid": guid, "path": str(source), "assembly": assembly.name})
        _check_inputs(project, roots, source_inputs, compiler_inputs, inventory)
        # Publish reusable entries only after the entire project compiled and
        # all original inputs passed the race check. A failing dependent must
        # never publish a partial successful revision.
        if cache is not None:
            for key, descriptor in assembly_entries:
                _write_cache(cache / "assemblies", key, descriptor, staging)
        for path in staging.glob("*.dll"):
            shutil.copy2(path, output / path.name)
        for path in staging.glob("*.pdb"):
            shutil.copy2(path, output / path.name)
    manifest = {"schema": 1, "project": str(project), "unity": str(Path(unity).resolve()),
                "assemblies": published, "scripts": scripts, "packages": versions,
                "defines": sorted(defines), "diagnostics": diagnostics,
                "compilerInputs": compiler_inputs, "compilerInputHash": compiler_hash,
                "cacheHit": False,
                "compiledAssemblyCount": sum(not item["cacheHit"] for item in published),
                "reusedAssemblyCount": sum(item["cacheHit"] for item in published),
                "elapsedSeconds": time.monotonic() - started,
                "editorStarted": False, "projectLibraryUsed": False,
                "capabilities": {"coldCompile": True, "target": "LinuxStandalone64-Mono",
                                 "asmdef": True, "asmref": True, "ilPostprocessing": False,
                                 "burst": False, "runtimeInitializeRegistration": False,
                                 "packageDownload": False, "nativePluginImport": False}}
    manifest["capabilities"]["lifecycleBarrier"] = lifecycle_enabled
    manifest["capabilities"]["scriptableObjectBarrier"] = lifecycle_enabled and all(
        item.get("lifecycleBarrier", {}).get("scriptableObjectBarrier") is True for item in published)
    manifest["runtimeInitializers"] = [initializer for item in published
                                       for initializer in item.get("lifecycleBarrier", {}).get("runtimeInitializers", [])]
    manifest["capabilities"]["runtimeInitializeRegistration"] = lifecycle_enabled
    manifest["capabilities"]["unitySourceGenerators"] = True
    (output / "compile.json").write_text(json.dumps(manifest, indent=2) + "\n")
    if cache is not None:
        _write_cache(cache, compiler_hash, manifest, output)
        manifest["elapsedSeconds"] = time.monotonic() - started
        (output / "compile.json").write_text(json.dumps(manifest, indent=2) + "\n")
    return manifest
