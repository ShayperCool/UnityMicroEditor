#!/usr/bin/env python3
"""Bounded standalone decoding probe for the Autodesk SDK packaged with Unity.

This does not invoke Unity's built-in ModelImporter or produce cooked Unity data.
No Editor is started. Source FBX is copied into scratch space before native import.
Bindings are derived and checked against the package's published NativeMethods.cs.
"""
from __future__ import annotations

import argparse
import ctypes
import hashlib
import json
from pathlib import Path
import re
import resource
import subprocess
import sys
import tarfile
import tempfile
import time

LIMIT = 64 * 1024 * 1024
PREFIX = "package/"
NATIVE = PREFIX + "Editor/Plugins/UnityFbxSdkNative.so"
WRAPPER = PREFIX + "Runtime/Scripts/NativeMethods.cs"
METHODS = ["FbxManager_Create", "FbxManager_Destroy", "FbxScene_Create__SWIG_0",
           "FbxImporter_Create__SWIG_0", "FbxIOBase_Initialize__SWIG_1", "FbxImporter_Import",
           "FbxScene_GetRootNode", "FbxNode_GetChildCount__SWIG_1", "FbxNode_GetChild",
           "FbxNode_GetMesh", "FbxGeometryBase_GetControlPointsCount", "FbxMesh_GetPolygonCount",
           "FbxImporter_SWIGUpcast", "FbxScene_SWIGUpcast", "FbxMesh_SWIGUpcast", "FbxGeometry_SWIGUpcast"]


def sha(content: bytes) -> str:
    return hashlib.sha256(content).hexdigest()


def abi_declarations(source: str) -> dict:
    pattern = (r'\[global::System\.Runtime\.InteropServices\.DllImport\(DllImportName,\s*EntryPoint="([^"]+)"\)\]'
               r'\s*public static extern ([\w:.]+) (\w+)\(([^)]*)\);')
    types = {"global::System.IntPtr": "pointer", "global::System.Runtime.InteropServices.HandleRef": "pointer",
             "string": "string", "int": "int", "bool": "bool", "void": "void"}
    records = {}
    for match in re.finditer(pattern, source):
        entry, returns, name, parameters = match.groups()
        if name not in METHODS:
            continue
        parameter_types = [value.strip().rsplit(" ", 1)[0] for value in parameters.split(",") if value.strip()]
        records[name] = {"entryPoint": entry, "returns": types[returns],
                         "parameters": [types[value] for value in parameter_types],
                         "declaration": match.group(0)}
    missing = set(METHODS) - set(records)
    if missing:
        raise ValueError("Published wrapper ABI not found for: " + ", ".join(sorted(missing)))
    return records


def native_child(library: Path, source: Path, abi: Path) -> dict:
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    types = {"pointer": ctypes.c_void_p, "string": ctypes.c_char_p,
             "int": ctypes.c_int, "bool": ctypes.c_bool, "void": None}
    sdk = ctypes.CDLL(str(library))
    functions = {}
    for name, declaration in json.loads(abi.read_text()).items():
        function = getattr(sdk, declaration["entryPoint"])
        function.restype = types[declaration["returns"]]
        function.argtypes = [types[value] for value in declaration["parameters"]]
        functions[name] = function
    started = time.monotonic()
    manager = functions["FbxManager_Create"]()
    if not manager:
        raise RuntimeError("FBX SDK did not create a manager")
    try:
        scene = functions["FbxScene_Create__SWIG_0"](manager, b"CLI SDK decoding probe")
        importer = functions["FbxImporter_Create__SWIG_0"](manager, b"CLI SDK importer")
        if not scene or not importer:
            raise RuntimeError("FBX SDK did not create scene/importer")
        io_base = functions["FbxImporter_SWIGUpcast"](importer)
        document = functions["FbxScene_SWIGUpcast"](scene)
        initialized = functions["FbxIOBase_Initialize__SWIG_1"](io_base, str(source).encode(), -1)
        imported = functions["FbxImporter_Import"](importer, document) if initialized else False
        if not initialized or not imported:
            raise RuntimeError("SDK Initialize/Import failed")
        queue = [functions["FbxScene_GetRootNode"](scene)]
        nodes, meshes, vertices, polygons = 0, 0, 0, 0
        seen_meshes = set()
        while queue:
            node = queue.pop()
            if not node:
                raise RuntimeError("SDK returned a null node")
            nodes += 1
            if nodes > 10000:
                raise RuntimeError("Bounded probe node limit exceeded")
            children = functions["FbxNode_GetChildCount__SWIG_1"](node)
            if children < 0 or children > 10000:
                raise RuntimeError("Invalid/excessive SDK child count")
            queue.extend(functions["FbxNode_GetChild"](node, index) for index in range(children))
            mesh = functions["FbxNode_GetMesh"](node)
            if mesh and mesh not in seen_meshes:
                seen_meshes.add(mesh)
                meshes += 1
                geometry = functions["FbxMesh_SWIGUpcast"](mesh)
                geometry_base = functions["FbxGeometry_SWIGUpcast"](geometry)
                vertices += functions["FbxGeometryBase_GetControlPointsCount"](geometry_base)
                polygons += functions["FbxMesh_GetPolygonCount"](mesh)
        return {"initialized": True, "imported": True, "nodesIncludingRoot": nodes,
                "meshes": meshes, "controlPoints": vertices, "polygons": polygons,
                "decodeSeconds": time.monotonic() - started}
    finally:
        functions["FbxManager_Destroy"](manager)


def run_probe(unity: Path, source: Path, output: Path, timeout: float) -> dict:
    data = unity.resolve().parent / "Data"
    packages = sorted((data / "Resources/PackageManager/Editor").glob("com.autodesk.fbx-*.tgz"))
    if not packages:
        raise ValueError("Unity installation has no packaged Autodesk FBX SDK")
    package = packages[-1]
    source = source.resolve()
    output = output.resolve()
    if output.suffix != ".json" or any(parent.name == "Assets" and output.is_relative_to(parent) for parent in source.parents):
        raise ValueError("Probe report must be a .json file outside source project Assets")
    if source.suffix.lower() != ".fbx" or source.stat().st_size > LIMIT:
        raise ValueError("Probe needs a source FBX of at most 64 MiB")
    source_bytes = source.read_bytes()
    started = time.monotonic()
    with tarfile.open(package) as archive:
        for name in (NATIVE, WRAPPER):
            if archive.getmember(name).size > LIMIT:
                raise ValueError("Probe archive member size limit exceeded")
        library_bytes = archive.extractfile(NATIVE).read()
        wrapper_bytes = archive.extractfile(WRAPPER).read()
    declarations = abi_declarations(wrapper_bytes.decode("utf-8-sig"))
    with tempfile.TemporaryDirectory(prefix="unity-native-fbx-probe-") as temporary:
        scratch = Path(temporary)
        library = scratch / "UnityFbxSdkNative.so"
        library.write_bytes(library_bytes)
        input_file = scratch / source.name
        input_file.write_bytes(source_bytes)
        abi = scratch / "abi.json"
        abi.write_text(json.dumps(declarations))
        command = [sys.executable, str(Path(__file__).resolve()), "--child", str(library), str(input_file), str(abi)]
        try:
            process = subprocess.run(command, cwd=scratch, capture_output=True, text=True, timeout=timeout)
            try:
                decoded = json.loads(process.stdout) if process.returncode == 0 else None
            except ValueError:
                decoded = None
            failure = None if decoded else f"Native child exit {process.returncode}: " + (process.stdout + process.stderr)[-4000:]
            exit_code = process.returncode
        except subprocess.TimeoutExpired:
            decoded, failure, exit_code = None, f"Native probe exceeded {timeout}s", None
    report = {"success": decoded is not None, "editorStarted": False, "sourceAssetsModified": False,
              "sourceCopiedToScratch": True,
              "sourcePath": str(source), "sourceBytes": len(source_bytes), "sourceSha256": sha(source_bytes),
              "packagePath": str(package), "packageSha256": sha(package.read_bytes()),
              "libraryArchiveEntry": NATIVE, "libraryBytes": len(library_bytes), "librarySha256": sha(library_bytes),
              "publishedWrapperEntry": WRAPPER, "publishedWrapperSha256": sha(wrapper_bytes),
              "publishedAbi": declarations, "exitCode": exit_code, "result": decoded, "failure": failure,
              "wallSeconds": time.monotonic() - started,
              "isUnityModelImporter": False, "producesUnityRuntimeAssets": False,
              "limitation": "Standalone Autodesk SDK decoding only; Unity ModelImporter meta/rig/avatar/subasset/cooking behavior is not reproduced."}
    if sha(source.read_bytes()) != report["sourceSha256"]:
        report.update(success=False, failure="Source changed while probe ran")
    output.resolve().parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(report, indent=2) + "\n")
    return report


def main():
    if len(sys.argv) == 5 and sys.argv[1] == "--child":
        print(json.dumps(native_child(Path(sys.argv[2]), Path(sys.argv[3]), Path(sys.argv[4]))), flush=True)
        return 0
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--unity", type=Path, default=Path("/home/shaypercool/Unity/Hub/Editor/6000.6.3f1/Editor/Unity"))
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--timeout", type=float, default=10)
    args = parser.parse_args()
    if not 0 < args.timeout <= 60:
        parser.error("timeout must be in (0,60] seconds")
    result = run_probe(args.unity, args.source, args.output, args.timeout)
    print(json.dumps({key: result[key] for key in ("success", "exitCode", "result", "wallSeconds", "failure")}, indent=2))
    return 0 if result["success"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
