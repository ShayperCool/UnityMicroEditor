#!/usr/bin/env bash
set -euo pipefail

unity_data=${UNITY_DATA:-$HOME/Unity/Hub/Editor/6000.6.3f1/Editor/Data}
proof_sources=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
proof_output=${1:?Usage: compile.sh /absolute/Temp/output /absolute/Player_Data/Managed}
proof_managed=${2:?Usage: compile.sh /absolute/Temp/output /absolute/Player_Data/Managed}
mkdir -p -- "$proof_output"
proof_output=$(cd -- "$proof_output" && pwd)
proof_managed=$(cd -- "$proof_managed" && pwd)
proof_args="$proof_output/compile.rsp"
proof_compilers=( "$unity_data"/DotNetSdk/sdk/*/Roslyn/bincore/csc.dll )
proof_csc=${proof_compilers[-1]}
test -f "$proof_csc"
proof_tool=$(cd -- "$proof_sources/../.." && pwd)
mkdir -p -- "$proof_output/LifecycleCompiler"
cp -- "$proof_tool/LifecycleCompiler/Program.cs" "$proof_tool/LifecycleCompiler/LifecycleCompiler.csproj" "$proof_output/LifecycleCompiler/"
printf '%s\n' '<configuration><packageSources><clear /></packageSources></configuration>' > "$proof_output/LifecycleCompiler/NuGet.Config"
"$unity_data/DotNetSdk/dotnet" build "$proof_output/LifecycleCompiler/LifecycleCompiler.csproj" --nologo -v:q \
  -o "$proof_output/LifecycleCompiler/payload" "-p:RoslynPath=$(dirname -- "$proof_csc")" -p:NuGetAudit=false
proof_request_args=(instrumentation-request --managed "$proof_managed" --sources "$proof_sources" --output "$proof_output/Sources" --request "$proof_output/lifecycle-request.json")
if [[ ${MULTIPLAYER_OBSERVER_ONLY:-0} == 1 ]]; then proof_request_args+=(--observer-only); fi
(cd -- "$proof_sources/.." && go run ./MultiplayerProof "${proof_request_args[@]}")
"$unity_data/DotNetSdk/dotnet" "$proof_output/LifecycleCompiler/payload/LifecycleCompiler.dll" "$proof_output/lifecycle-request.json" > "$proof_output/lifecycle-proof.json"
{
    printf '%s\n' '-nologo' '-target:library' '-nostdlib+' '-langversion:9' '-warnaserror+' '-nowarn:0649'
    printf '%s\n' "-out:\"$proof_output/UnityPlay.MultiplayerProof.dll\""
    for assembly in "$proof_managed"/*.dll; do
        if [[ $(basename -- "$assembly") == UnityPlay.MultiplayerProof.dll ]]; then continue; fi
        printf '%s\n' "-reference:\"$assembly\""
    done
    printf '%s\n' "\"$proof_output/Sources/MultiplayerObserver.cs\""
    if [[ ${MULTIPLAYER_OBSERVER_ONLY:-0} != 1 ]]; then
        printf '%s\n' "\"$proof_output/Sources/RawContentProvider.cs\""
    fi
    printf '%s\n' "\"$proof_output/Sources/LifecycleMarker.cs\""
} > "$proof_args"
"$unity_data/DotNetSdk/dotnet" "$proof_csc" "@$proof_args"
printf '%s\n' "$proof_output/UnityPlay.MultiplayerProof.dll"
