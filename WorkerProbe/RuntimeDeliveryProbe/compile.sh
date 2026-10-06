#!/usr/bin/env bash
set -euo pipefail

# Compile the isolated probe against the installed Linux Mono Player API.
# The output is test data; no Editor process or Python wrapper is involved.
unity_data=${UNITY_DATA:-$HOME/Unity/Hub/Editor/6000.6.3f1/Editor/Data}
probe_source=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/WorkerRuntimeDeliveryProbe.cs
probe_output=${1:?Usage: compile.sh /absolute/ignored/output-directory}
mkdir -p -- "$probe_output"
probe_output=$(cd -- "$probe_output" && pwd)
probe_args="$probe_output/compile.rsp"
{
    printf '%s\n' '-nologo' '-target:library' '-nostdlib+' '-langversion:9' '-warnaserror+'
    printf '%s\n' "-out:\"$probe_output/WorkerRuntimeDeliveryProbe.dll\""
    for assembly in "$unity_data"/MonoBleedingEdge/lib/mono/unityaot-linux/*.dll "$unity_data"/MonoBleedingEdge/lib/mono/unityaot-linux/Facades/*.dll; do
        printf '%s\n' "-reference:\"$assembly\""
    done
    for assembly in "$unity_data"/PlaybackEngines/LinuxStandaloneSupport/Variations/mono/Managed/UnityEngine*.dll; do
        printf '%s\n' "-reference:\"$assembly\""
    done
    printf '%s\n' "\"$probe_source\""
    printf '%s\n' "\"$(dirname -- "$probe_source")/NativeContentProductionProof.cs\""
    printf '%s\n' "\"$(dirname -- "$probe_source")/FastPatchProductionProof.cs\""
} > "$probe_args"
probe_compilers=( "$unity_data"/DotNetSdk/sdk/*/Roslyn/bincore/csc.dll )
probe_csc=${probe_compilers[-1]}
test -f "$probe_csc"
"$unity_data/DotNetSdk/dotnet" "$probe_csc" "@$probe_args"
printf '%s\n' "$probe_output/WorkerRuntimeDeliveryProbe.dll"
