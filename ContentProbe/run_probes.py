#!/usr/bin/env python3
"""Isolated, bounded probes; does not start Unity Editor or a Player."""
import argparse
import json
from pathlib import Path
import resource
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--editor", required=True, type=Path, help="Installed Editor/Unity executable")
    parser.add_argument("--scene", required=True, type=Path, help="Raw YAML scene to archive byte-for-byte")
    parser.add_argument("--bundle", required=True, type=Path, help="Previously cooked bundle to inspect")
    parser.add_argument("--out", required=True, type=Path, help="Ignored directory for logs and created archive")
    options = parser.parse_args()
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    editor = options.editor.resolve(strict=True)
    data = editor.parent / "Data"
    dotnet = data / "DotNetSdk/dotnet"
    project = Path(__file__).resolve().parent
    output = options.out.resolve()
    output.mkdir(parents=True, exist_ok=True)
    scene = options.scene.resolve(strict=True)
    bundle = options.bundle.resolve(strict=True)

    # Offline build: no package feeds, no Editor process, no project content writes.
    build = subprocess.run(
        [str(dotnet), "build", str(project / "ContentProbe.csproj"),
         f"-p:UnityManagedPath={data / 'Managed/UnityEngine'}", "--configfile",
         str(project / "NuGet.Config"), "-v:q"],
        capture_output=True, text=True, timeout=60, check=False)
    (output / "build.log").write_text(build.stdout + build.stderr)
    if build.returncode:
        raise RuntimeError(f"Probe compilation failed; see {output / 'build.log'}")

    executable = project / "bin/Debug/net8.0/ContentProbe.dll"
    cases = {
        "editor-api": [],
        "native-editor": [str(editor)],
        "ufs": [str(data / "Tools/UnityFileSystemApi.so"), str(scene), str(bundle), str(output)],
        "ufs-raw-format": [str(data / "Tools/UnityFileSystemApi.so"), str(output / "raw-scene.unityfs")],
    }
    results = {}
    for mode, arguments in cases.items():
        process = subprocess.run([str(dotnet), str(executable), mode, *arguments],
                                 capture_output=True, text=True, timeout=20, check=False)
        (output / f"{mode}.stdout.log").write_text(process.stdout)
        (output / f"{mode}.stderr.log").write_text(process.stderr)
        try:
            payload = json.loads(process.stdout[process.stdout.find("{"):])
        except json.JSONDecodeError:
            payload = {"nativeStdout": process.stdout, "nativeStderr": process.stderr}
        results[mode] = {"exitCode": process.returncode, "result": payload}

    # Negative cases are findings, and must fail for the specific observed reasons.
    managed = results["editor-api"]
    native = results["native-editor"]
    archive = results["ufs"]
    raw_format = results["ufs-raw-format"]
    raw_rejected = (
        raw_format["exitCode"] == 0
        and raw_format["result"].get("returnCode") in (4, 6)
    )
    success = (
        managed["exitCode"] == 1
        and managed["result"].get("error") == "System.Security.SecurityException"
        and "ECall" in managed["result"].get("message", "")
        and native["exitCode"] == 1
        and "cannot dynamically load position-independent executable" in native["result"].get("message", "")
        and archive["exitCode"] == 0
        and archive["result"]["raw"]["byteIdentical"]
        and not archive["result"]["raw"]["importedOrSerialized"]
        and raw_rejected
        and any(node["objects"] is not None and node["objects"] > 0
                for node in archive["result"]["cookedBundleNodes"])
    )
    report = {"expectedFindingsVerified": success, "editor": str(editor),
              "rawScene": str(scene), "cookedBundle": str(bundle), "probes": results}
    (output / "results.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    return 0 if success else 1


if __name__ == "__main__":
    raise SystemExit(main())
