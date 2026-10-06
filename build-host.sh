#!/usr/bin/env bash
set -euo pipefail

tool_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
host_project="$tool_root/HostProject"
host_version=$(awk '$1 == "m_EditorVersion:" { print $2 }' "$host_project/ProjectSettings/ProjectVersion.txt")
host_editor="${UNITY_EDITOR:-$HOME/Unity/Hub/Editor/$host_version/Editor/Unity}"
host_output="$host_project/Builds/Linux/UnityMicroEditorHost.x86_64"
host_dry_run=0
while (($#)); do
    case "$1" in
        --editor) host_editor=${2:?Missing --editor path}; shift 2 ;;
        --output) host_output=${2:?Missing --output path}; shift 2 ;;
        --dry-run) host_dry_run=1; shift ;;
        *) printf 'Usage: %s [--editor /absolute/Unity] [--output /absolute/Player.x86_64] [--dry-run]\n' "$0" >&2; exit 2 ;;
    esac
done
[[ $host_editor == /* && -f $host_editor && -x $host_editor ]] || { printf 'Unity executable must exist at an absolute path: %s\n' "$host_editor" >&2; exit 2; }
[[ $host_output == /* ]] || { printf 'Host output must be absolute: %s\n' "$host_output" >&2; exit 2; }
host_output_dir=$(dirname -- "$host_output")
host_report="$host_output_dir/host-build.json"
host_log="$host_project/Logs/host-build.log"
host_command=( "$host_editor" -batchmode -nographics -buildTarget Linux64 -projectPath "$host_project"
    -executeMethod Dungeon.CLIPlay.HostBuild.Build.FromCommandLine -logFile "$host_log" )
if ((host_dry_run)); then
    printf 'UNITY_RAW_HOST_OUTPUT=%q UNITY_RAW_HOST_REPORT=%q ' "$host_output" "$host_report"
    printf '%q ' "${host_command[@]}"
    printf '\n'
    exit 0
fi
# Avoid a concurrent Unity project process while constructing the host.
for host_proc in /proc/[0-9]*/cmdline; do
    [[ -r $host_proc ]] || continue
    host_args=()
    mapfile -d '' -t host_args < "$host_proc" 2>/dev/null || continue
    [[ ${host_args[0]:-} == */Unity ]] || continue
    for host_arg in "${host_args[@]}"; do
        if [[ $host_arg == -projectPath ]]; then
            host_pid=${host_proc#/proc/}
            printf 'Close the running Unity Editor before building the host. PID %s\n' "${host_pid%%/*}" >&2
            exit 2
        fi
    done
done
mkdir -p -- "$host_output_dir" "$host_project/Logs"
export UNITY_RAW_HOST_OUTPUT="$host_output" UNITY_RAW_HOST_REPORT="$host_report"
exec "${host_command[@]}"
