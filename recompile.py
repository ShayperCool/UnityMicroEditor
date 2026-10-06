"""Replay a captured Linux Mono Roslyn invocation without running Unity.

This experiment accepts edits to existing method bodies. It does not rebuild
serialized assets, runtime initialization metadata, native/Burst code, or ILPP.
It cannot prove that a C# edit preserves types, public APIs, or serialized fields.
"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import time
import uuid

from unity_play import ToolError, require_idle, write_json

SCHEMA = 1
ASSEMBLY_NAME = re.compile(r"^[A-Za-z0-9_.-]+$")
SAFE_ANALYZERS = {
    "Microsoft.Extensions.Logging.Generators.dll", "System.Text.Json.SourceGeneration.dll",
    "Unity.Analyzers.Common.dll", "Unity.Analyzers.LongLiving.dll",
    "Unity.Properties.SourceGenerator.dll", "Unity.SourceGenerators.dll",
    "Unity.UIToolkit.SourceGenerator.dll",
}
VALUE_FLAGS = {
    "target", "define", "d", "langversion", "warn", "nowarn", "debug",
    "runtimemetadataversion", "preferreduilang",
}
SWITCH_FLAGS = {"deterministic", "optimize+", "optimize-", "nologo", "utf8output", "unsafe+", "unsafe-"}
FILE_FLAGS = {"r", "reference", "analyzer", "additionalfile", "additionalfiles", "ruleset"}
OUTPUT_FLAGS = {"out", "refout", "pdb"}
LIMITATION = (
    "Experimental method-body edits only. Type/public API/serialized-field changes, "
    "RuntimeInitializeOnLoad additions, assets, native/Burst code and IL postprocessing require a Unity build. "
    "The tool does not automatically prove that an edit is body-only."
)


def digest(path: Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def resolve(project: Path, value: str) -> Path:
    path = Path(value)
    return (path if path.is_absolute() else project / path).resolve()


def content_dependency(project: Path, record: dict) -> tuple[str, Path]:
    """Resolve Unity's logical asset path without guessing package cache versions.

    Unity provides physicalPath for virtual Packages paths through PackageInfo.
    Only the same embedded package or its project-local cache is accepted.
    """
    project = project.resolve()
    if not isinstance(record, dict):
        raise ToolError("Content source dependency must be an object")

    def relative(value):
        if not isinstance(value, str) or not value or "\\" in value:
            raise ToolError(f"Content source path must be project-relative: {value}")
        parts = value.split("/")
        if Path(value).is_absolute() or any(part in {"", ".", ".."} for part in parts):
            raise ToolError(f"Content source path escapes its directory: {value}")
        return parts

    logical = record.get("path")
    parts = relative(logical)
    if len(parts) < 2 or parts[0] not in {"Assets", "Packages"}:
        raise ToolError(f"Content source must be under Assets or Packages: {logical}")
    physical = record.get("physicalPath")
    if parts[0] == "Assets":
        if physical is not None and physical != "" and physical != logical:
            raise ToolError("Only package dependencies may override their physical path")
        physical = logical
    else:
        if len(parts) < 3:
            raise ToolError(f"Content package source has no relative file: {logical}")
        physical = logical if physical is None or physical == "" else physical
        physical_parts = relative(physical)
        embedded = physical_parts == parts
        cached = (len(physical_parts) >= 4 and physical_parts[:2] == ["Library", "PackageCache"]
                  and physical_parts[2].startswith(parts[1] + "@")
                  and len(physical_parts[2]) > len(parts[1]) + 1
                  and physical_parts[3:] == parts[2:])
        if not embedded and not cached:
            raise ToolError(f"Content package physical path does not match its package: {logical}")
    path = (project / physical).resolve()
    if not path.is_relative_to(project):
        raise ToolError(f"Content source path escapes its project: {physical}")
    if parts[0] == "Packages" and path.relative_to(project).as_posix() != physical:
        raise ToolError(f"Content package physical path resolves outside its package: {physical}")
    return logical, path


def tokens(text: str) -> list[str]:
    # Bee writes one argument per line. shlex removes argument quotes; no shell
    # ever sees the response contents or the compiler command.
    try:
        return shlex.split(text, comments=True, posix=True)
    except ValueError as error:
        raise ToolError(f"Invalid compiler response file: {error}") from error


def flag(token: str) -> tuple[str, str | None] | None:
    if not token.startswith(("-", "/")):
        return None
    name, separator, value = token[1:].partition(":")
    return name.lower(), value if separator else None


def inspect_rsp(project: Path, text: str, data: Path) -> dict:
    definitions, sources, inputs, outputs = set(), [], {}, {}
    if any(len(tokens(line)) > 1 for line in text.splitlines()):
        raise ToolError("Offline refresh requires Bee's one-argument-per-line response format")
    for token in tokens(text):
        option = flag(token)
        # An absolute Unix source path begins with '/'. Bee normally writes
        # relative paths, but absolute existing C# paths remain valid sources.
        if token.lower().endswith(".cs") and resolve(project, token).is_file():
            sources.append(str(resolve(project, token)))
            continue
        if option is None:
            raise ToolError(f"Unsupported compiler argument: {token}")
        name, value = option
        if name in {"define", "d"} and value is not None:
            definitions.update(re.split(r"[;,]", value))
        if name in OUTPUT_FLAGS:
            if not value or name in outputs:
                raise ToolError(f"Invalid or repeated compiler output: {token}")
            outputs[name] = str(resolve(project, value))
        elif name in FILE_FLAGS:
            if not value:
                raise ToolError(f"Missing compiler input path: {token}")
            # Unity uses single paths, without aliases or comma-separated refs.
            path = resolve(project, value)
            if not path.is_file():
                raise ToolError(f"Compiler input is missing: {path}")
            if name == "analyzer" and (path.name not in SAFE_ANALYZERS or not path.is_relative_to(data)):
                raise ToolError(f"Analyzer/source generator requires a Unity build: {path}")
            inputs[str(path)] = {"sha256": digest(path), "kind": name}
        elif name in VALUE_FLAGS:
            if value is None:
                raise ToolError(f"Missing compiler option value: {token}")
            if name == "target" and value != "library":
                raise ToolError("Only library assemblies can be refreshed")
        elif name not in SWITCH_FLAGS:
            raise ToolError(f"Compiler option is not supported for offline refresh: {token}")
    if "out" not in outputs or not sources:
        raise ToolError("Response has no implementation output or C# sources")
    if Path(outputs["out"]).name.endswith(".ref.dll"):
        raise ToolError("A reference assembly cannot replace a Player implementation")
    return {"defines": sorted(definitions), "sources": sorted(set(sources)), "inputs": inputs, "outputs": outputs}


def is_linux_mono(text: str) -> bool:
    definitions = set()
    for token in tokens(text):
        option = flag(token)
        if option and option[0] in {"define", "d"} and option[1]:
            definitions.update(re.split(r"[;,]", option[1]))
    return (
        {"UNITY_STANDALONE_LINUX", "ENABLE_MONO"}.issubset(definitions)
        and not any(value == "ENABLE_IL2CPP" or value.startswith(("UNITY_EDITOR", "UNITY_WEBGL"))
                    or value == "WEBGL" for value in definitions)
    )


def project_files(project: Path) -> tuple[dict[str, str], dict[str, str]]:
    sources, assets = {}, {}
    for root in ("Assets", "Packages", "ProjectSettings"):
        folder = project / root
        if not folder.is_dir():
            continue
        for file in sorted(folder.rglob("*")):
            if file.is_file():
                relative = file.relative_to(project).as_posix()
                (sources if file.suffix.lower() == ".cs" else assets)[relative] = digest(file)
    return sources, assets


def find_compiler(editor: Path) -> tuple[Path, Path]:
    sdk = editor.parent / "Data/DotNetSdk"
    compiler = sorted(sdk.glob("sdk/*/Roslyn/bincore/csc.dll"))
    dotnet = sdk / "dotnet"
    if not dotnet.is_file() or not compiler:
        raise ToolError(f"Unity Roslyn compiler is missing under {sdk}")
    # The installation normally contains one SDK. Pick by version components
    # rather than lexicographic path order when several SDKs are installed.
    compiler.sort(key=lambda p: tuple(int(n) for n in re.findall(r"\d+", p.parents[2].name)))
    return dotnet, compiler[-1]


def managed_folder(player: Path) -> Path:
    folder = player.parent / f"{player.stem}_Data/Managed"
    if not folder.is_dir():
        raise ToolError(f"Linux Mono Player Managed directory is missing: {folder}")
    return folder.resolve()


def capture(project: Path, editor: Path, player: Path) -> dict:
    """Capture only invocations whose raw implementation matches this Player."""
    project, editor, player = project.resolve(), editor.resolve(), player.resolve()
    managed = managed_folder(player)
    dotnet, compiler = find_compiler(editor)
    data = (editor.parent / "Data").resolve()
    assemblies, skipped = {}, {}
    candidates = sorted((project / "Library/Bee/artifacts").rglob("*.rsp"), key=lambda p: p.stat().st_mtime, reverse=True)
    for rsp in candidates:
        name = rsp.stem
        if name.endswith(".dll.mvfrm") or not ASSEMBLY_NAME.fullmatch(name):
            continue
        shipped = managed / f"{name}.dll"
        if not shipped.is_file() or name in assemblies:
            continue
        try:
            text = rsp.read_text()
            if not is_linux_mono(text):
                continue
            record = inspect_rsp(project, text, data)
            artifact = Path(record["outputs"]["out"])
            if artifact.name != shipped.name or not artifact.is_file() or digest(artifact) != digest(shipped):
                raise ToolError("Raw compiler output differs from shipped DLL; IL postprocessing/stripping or stale build")
            if not all(Path(source).is_relative_to(project / "Assets") for source in record["sources"]):
                raise ToolError("Offline refresh supports project Assets source assemblies only")
            if "netcode" in name.lower() or "burst" in name.lower():
                raise ToolError("Netcode/Burst assemblies require their Unity postprocessors")
            suspicious = ("Unity.Netcode", "Unity.Burst", "Unity.Entities", "Unity.Jobs")
            if any(Path(path).name.startswith(suspicious) for path in record["inputs"]):
                raise ToolError("Assembly references Netcode/Burst/Entities/Jobs; native or IL processing is not replayed")
            snapshot = player.parent / "compiler" / f"{name}.rsp"
            snapshot.parent.mkdir(parents=True, exist_ok=True)
            snapshot.write_text(text)
            record.update(rsp=str(snapshot), rspSha256=digest(snapshot), dll=str(shipped),
                          dllSha256=digest(shipped), sourceHashes={source: digest(Path(source)) for source in record["sources"]})
            assemblies[name] = record
            skipped.pop(name, None)
        except (ToolError, OSError, ValueError) as error:
            skipped[name] = str(error)
    source_hashes, asset_hashes = project_files(project)
    config = {"schema": SCHEMA, "project": str(project), "player": str(player), "editor": str(editor),
              "dotnet": str(dotnet), "dotnetSha256": digest(dotnet), "compiler": str(compiler),
              "compilerSha256": digest(compiler), "sources": source_hashes, "assets": asset_hashes,
              "assemblies": assemblies, "skipped": skipped, "limitation": LIMITATION}
    path = player.parent / "code-refresh.json"
    write_json(path, config)
    return {"manifest": str(path), "assemblies": sorted(assemblies), "skipped": skipped, "limitation": LIMITATION}


def player_processes(player: Path, proc_root: Path = Path("/proc")) -> list[int]:
    running = []
    for proc in proc_root.glob("[0-9]*"):
        try:
            # exe also identifies launches through symlinks and relative paths.
            executable = (proc / "exe").resolve(strict=True)
            args = (proc / "cmdline").read_bytes().split(b"\0")
            if executable == player or args and args[0] and Path(os.fsdecode(args[0])).resolve() == player:
                running.append(int(proc.name))
        except (OSError, ValueError, RuntimeError):
            continue
    return running


def require_stopped(project: Path, player: Path) -> None:
    require_idle(project)
    processes = player_processes(player)
    if processes:
        raise ToolError(f"Stop the target Player before replacing its assembly; running PIDs: {processes}")


def verify_file(path: Path, expected: str, label: str) -> None:
    if not path.is_file() or digest(path) != expected:
        raise ToolError(f"{label} changed or disappeared: {path}. Rebuild the baseline Player.")


def compare_files(baseline: dict[str, str], current: dict[str, str]) -> list[str]:
    return sorted(path for path in baseline.keys() | current.keys() if baseline.get(path) != current.get(path))


def accept_content(project: Path, player: Path, content: dict) -> dict:
    """Advance asset checks only for unchanged code and verified cooked files.

    The exporter calls this after validating and publishing a complete revision.
    An unrelated asset, metadata, assembly-definition or setting change leaves
    the compiler baseline untouched and continues to block offline code replay.
    """
    project, player = project.resolve(), player.resolve()
    if not content.get("success") or not content.get("ready") or content.get("target") != "StandaloneLinux64":
        raise ToolError("Only successfully published Linux content can advance compiler asset checks")
    manifest = player.parent / "code-refresh.json"
    config = json.loads(manifest.read_text())
    if config.get("schema") != SCHEMA or config.get("project") != str(project) or config.get("player") != str(player):
        raise ToolError("Content/compiler baseline belongs to another project/Player or schema")
    sources, assets = project_files(project)
    if sources != config["sources"]:
        raise ToolError("C# source changed; content export cannot advance the compiler baseline")
    cooked = {}
    for dependency in content.get("sourceDependencies", []):
        logical, path = content_dependency(project, dependency)
        verify_file(path, dependency["sha256"], "Cooked source dependency")
        if logical.startswith("Assets/"):
            cooked[logical] = dependency["sha256"]
    changed = compare_files(config["assets"], assets)
    rejected = [path for path in changed
                if not path.startswith("Assets/") or Path(path).suffix.lower() in {".meta", ".asmdef", ".asmref", ".cs"}
                or path not in cooked or assets.get(path) != cooked[path]]
    if rejected:
        raise ToolError(f"Uncooked assets/meta/assembly definitions/settings still require a baseline build: {rejected[:10]}")
    for record in config["assemblies"].values():
        verify_file(Path(record["dll"]), record["dllSha256"], "Player DLL")
    if changed:
        config["assets"] = assets
        write_json(manifest, config)
    return {"accepted": True, "paths": changed, "revision": content.get("revision")}


def rewrite_rsp(text: str, output: Path) -> str:
    rewritten = []
    for line in text.splitlines():
        parsed = tokens(line)
        option = flag(parsed[0]) if len(parsed) == 1 else None
        if option and option[0] in OUTPUT_FLAGS:
            suffix = {"out": ".dll", "refout": ".ref.dll", "pdb": ".pdb"}[option[0]]
            destination = output.with_suffix(suffix)
            # Quote for the compiler response-file grammar, not for a shell.
            rewritten.append(f'-{option[0]}:"{destination}"')
        else:
            rewritten.append(line)
    return "\n".join(rewritten) + "\n"


def refresh(project: Path, player: Path, assembly: str, *, dry_run: bool = False, timeout: float = 120) -> dict:
    project, player = project.resolve(), player.resolve()
    if not ASSEMBLY_NAME.fullmatch(assembly) or assembly.endswith(".dll"):
        raise ToolError("Specify an assembly name without .dll")
    require_stopped(project, player)
    manifest = player.parent / "code-refresh.json"
    if not manifest.is_file():
        raise ToolError("No compiler baseline captured. Run 'unity-play build' first.")
    config = json.loads(manifest.read_text())
    if config.get("schema") != SCHEMA or config.get("project") != str(project) or config.get("player") != str(player):
        raise ToolError("Compiler baseline belongs to a different project/Player or schema. Rebuild it.")
    record = config["assemblies"].get(assembly)
    if record is None:
        reason = config.get("skipped", {}).get(assembly, "no matching Linux Mono compiler response")
        raise ToolError(f"Assembly {assembly} cannot be refreshed: {reason}")
    expected_target = managed_folder(player) / f"{assembly}.dll"
    if Path(record["dll"]) != expected_target:
        raise ToolError("Compiler manifest points outside the target Player Managed directory")
    verify_file(expected_target, record["dllSha256"], "Player DLL")
    snapshot = Path(record["rsp"])
    verify_file(snapshot, record["rspSha256"], "Captured response file")
    for key in ("dotnet", "compiler"):
        verify_file(Path(config[key]), config[key + "Sha256"], key)
    for path, item in record["inputs"].items():
        verify_file(Path(path), item["sha256"], "Compiler reference/input")
    source_hashes, asset_hashes = project_files(project)
    if source_hashes.keys() != config["sources"].keys():
        changes = sorted(source_hashes.keys() ^ config["sources"].keys())
        raise ToolError(f"C# sources added/removed; rebuild the Player: {changes[:10]}")
    assets_changed = compare_files(config["assets"], asset_hashes)
    if assets_changed:
        raise ToolError(f"Assets/meta/assembly definitions/settings changed; rebuild the Player: {assets_changed[:10]}")
    selected = {Path(path).relative_to(project).as_posix() for path in record["sources"]}
    changed = compare_files(config["sources"], source_hashes)
    unrelated = sorted(set(changed) - selected)
    if unrelated:
        raise ToolError(f"Sources outside selected assembly changed; rebuild or revert them: {unrelated[:10]}")
    for path, expected in record["sourceHashes"].items():
        if not Path(path).is_file():
            raise ToolError(f"Captured C# source disappeared: {path}")
    work = project / "Temp/UnityPlay/recompile" / ("dry-run" if dry_run else uuid.uuid4().hex[:12])
    output, response = work / f"{assembly}.dll", work / f"{assembly}.rsp"
    command = [config["dotnet"], config["compiler"], f"@{response}"]
    result = {"assembly": assembly, "changedSources": changed, "command": command,
              "runtimeNeedsEditor": False, "limitation": LIMITATION}
    if dry_run:
        return {**result, "dryRun": True, "output": str(output), "target": str(expected_target)}
    if not changed:
        return {**result, "updated": False, "reason": "No source changes"}
    work.mkdir(parents=True, exist_ok=False)
    response.write_text(rewrite_rsp(snapshot.read_text(), output))
    started = time.monotonic()
    log = work / "compiler.log"
    try:
        with log.open("w") as stream:
            compiled = subprocess.run(command, cwd=project, stdout=stream, stderr=subprocess.STDOUT, timeout=timeout)
    except subprocess.TimeoutExpired as error:
        raise ToolError(f"Compiler exceeded {timeout:g}s; Player DLL unchanged. Log: {log}") from error
    if compiled.returncode != 0 or not output.is_file():
        raise ToolError(f"Compiler failed with exit code {compiled.returncode}; Player DLL unchanged. Log: {log}")
    # Source/assets and references must not change while compilation runs.
    fresh_sources, fresh_assets = project_files(project)
    if fresh_sources != source_hashes or fresh_assets != asset_hashes:
        raise ToolError("Project files changed during compilation; Player DLL unchanged. Retry after edits stop.")
    for path, item in record["inputs"].items():
        verify_file(Path(path), item["sha256"], "Compiler reference/input")
    require_stopped(project, player)
    verify_file(expected_target, record["dllSha256"], "Player DLL")
    backup = player.parent / "code-backups" / uuid.uuid4().hex[:12]
    backup.mkdir(parents=True, exist_ok=False)
    targets = [(output, expected_target)]
    pdb = output.with_suffix(".pdb")
    if pdb.is_file():
        targets.insert(0, (pdb, expected_target.with_suffix(".pdb")))
    staging = []
    installed = []
    try:
        for source, target in targets:
            if target.is_file():
                shutil.copy2(target, backup / target.name)
            stage = target.with_name(f".{target.name}.{uuid.uuid4().hex}.tmp")
            staging.append((stage, target))
            shutil.copyfile(source, stage)
        for stage, target in staging:
            os.replace(stage, target)
            installed.append(target)
    except OSError:
        for target in installed:
            previous = backup / target.name
            if previous.is_file():
                restore = target.with_name(f".{target.name}.restore.tmp")
                shutil.copyfile(previous, restore)
                os.replace(restore, target)
            else:
                target.unlink(missing_ok=True)
        raise
    finally:
        for stage, _ in staging:
            stage.unlink(missing_ok=True)
    record["dllSha256"] = digest(expected_target)
    record["sourceHashes"] = {path: digest(Path(path)) for path in record["sources"]}
    config["sources"] = source_hashes
    write_json(manifest, config)
    return {**result, "updated": True, "target": str(expected_target), "backup": str(backup),
            "compilerLog": str(log), "wallSeconds": round(time.monotonic() - started, 3)}
