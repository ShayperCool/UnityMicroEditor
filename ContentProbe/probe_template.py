#!/usr/bin/env python3
"""Boot installed Linux Mono template without any prebuilt project data, offscreen."""
import argparse
import json
import os
from pathlib import Path
import resource
import shutil
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--editor", required=True, type=Path)
    parser.add_argument("--assets", required=True, type=Path, help="Raw Assets directory, linked read-only by convention")
    parser.add_argument("--out", required=True, type=Path, help="New staging directory, preferably under ignored Logs")
    options = parser.parse_args()
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    data = options.editor.resolve(strict=True).parent / "Data"
    template = data / "PlaybackEngines/LinuxStandaloneSupport/Variations/linux64_player_development_mono"
    stage = options.out.resolve()
    stage.mkdir(parents=True, exist_ok=False)
    shutil.copy2(template / "LinuxPlayer", stage / "RawProject.x86_64")
    links = {
        "UnityPlayer.so": template / "UnityPlayer.so",
        "RawProject_Data/MonoBleedingEdge": template / "Data/MonoBleedingEdge",
        "RawProject_Data/Resources": template / "Data/Resources",
        "RawProject_Data/Managed": template.parent / "mono/Managed",
        # Without generated settings the installed template uses this default Mono search path.
        "External/MonoBleedingEdge/builds/monodistribution/lib": data / "MonoBleedingEdge/lib",
        "Assets": options.assets.resolve(strict=True),
    }
    for relative, source in links.items():
        destination = stage / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.symlink_to(source, target_is_directory=source.is_dir())
    (stage / "RawProject_Data/boot.config").write_text("")
    environment = os.environ.copy()
    for variable in ("DISPLAY", "WAYLAND_DISPLAY"):
        environment.pop(variable, None)
    log = stage / "player.log"
    command = [str(stage / "RawProject.x86_64"), "-batchmode", "-nographics", "-quit", "-logFile", str(log)]
    process = subprocess.run(command, cwd=stage, env=environment, capture_output=True,
                             text=True, timeout=20, check=False)
    log_text = log.read_text(errors="replace") if log.exists() else ""
    verified = process.returncode == 1 and "No GlobalGameManagers file was found" in log_text
    result = {"expectedBootstrapBoundaryVerified": verified, "exitCode": process.returncode,
              "command": command, "stdout": process.stdout, "stderr": process.stderr,
              "log": log_text, "hasImportedProjectLibrary": False, "hasPrebuiltProjectPlayerData": False}
    (stage / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({"expectedBootstrapBoundaryVerified": verified, "exitCode": process.returncode,
                      "playerLog": str(log), "result": str(stage / "result.json")}, indent=2))
    return 0 if verified else 1


if __name__ == "__main__":
    raise SystemExit(main())
