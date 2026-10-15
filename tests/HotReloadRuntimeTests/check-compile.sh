#!/usr/bin/env bash
# check-compile.sh <prepared host>/<Name>_Data/Managed [output directory]
# Compiles the bridge the two ways it is compiled, and the applier as a session compiles it:
#   1. the session command of rust/src/compiler/host.rs (stage_host);
#   2. the same without -unsafe+, which is what the asmdef (allowUnsafeCode: false) allows in the host build;
#   3. tools/HotReloadRuntime with the session flags against the same references plus the bridge of step 1.
# Nothing is started; the Managed directory is only read.
set -euo pipefail
managed="${1:?usage: check-compile.sh <Managed directory of a prepared host> [output directory]}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
out="${2:-$here/bin/compile}"
data="${UNITY_EDITOR_DIR:-$HOME/Unity/Hub/Editor/6000.6.3f1/Editor}/Data"
dotnet="$data/DotNetSdk/dotnet"
csc="$(ls "$data"/DotNetSdk/sdk/*/Roslyn/bincore/csc.dll | sort | tail -1)"
export DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_NOLOGO=1
mkdir -p "$out"

# References in the order of stage_host; the first file of a name wins.
references="$out/references.rsp"
: > "$references"
declare -A seen
for file in "$data"/MonoBleedingEdge/lib/mono/unityaot-linux/*.dll "$data"/MonoBleedingEdge/lib/mono/unityaot-linux/Facades/*.dll "$managed"/*.dll; do
    base="$(basename "$file")"
    if [ "$base" = UnityMicroEditor.dll ] || [ -n "${seen[$base]:-}" ]; then continue; fi
    seen[$base]=1
    printf '"-reference:%s"\n' "$file" >> "$references"
done
flags=(-nologo -target:library -nostdlib+ -langversion:9 -warnaserror+ -nowarn:0649,0618)
if [ -f "$managed/Unity.Addressables.dll" ] && [ -f "$managed/Unity.ResourceManager.dll" ]; then flags+=(-define:UNITY_MICRO_EDITOR_ADDRESSABLES); fi
mapfile -t bridge < <(find "$root/Packages/com.unitymicroeditor.runtime/Runtime" -name '*.cs' | sort)

mkdir -p "$out/session" "$out/asmdef"
"$dotnet" "$csc" "${flags[@]}" -unsafe+ "-out:$out/session/UnityMicroEditor.dll" "@$references" "${bridge[@]}"
echo "bridge, session command: ok (${#bridge[@]} sources)"
"$dotnet" "$csc" "${flags[@]}" -unsafe- "-out:$out/asmdef/UnityMicroEditor.dll" "@$references" "${bridge[@]}"
echo "bridge, without unsafe code: ok"
"$dotnet" "$csc" "${flags[@]}" -unsafe+ "-out:$out/UnityMicroEditor.HotReload.dll" "@$references" "-reference:$out/session/UnityMicroEditor.dll" "$root"/tools/HotReloadRuntime/*.cs
echo "applier, session flags: ok"
