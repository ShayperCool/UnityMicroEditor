#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 6 ]]; then
  printf 'Usage: %s SOURCE_PROJECT OWNED_REFERENCE_PROJECT EVIDENCE_DIR CLOSED_LIBRARY_SEED UNITY_EDITOR SCENE_PATH\n' "$0" >&2
  exit 2
fi

reference_tool_root=${UNITY_MICRO_EDITOR_ROOT:-$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)}
reference_source=$(realpath -- "$1")
reference_project=$(realpath -m -- "$2")
reference_output=$(realpath -m -- "$3")
reference_library=$(realpath -- "$4")
reference_unity=$(realpath -- "$5")
reference_scene=$6

for reference_path in Assets Packages ProjectSettings; do test -d "$reference_source/$reference_path"; done
test -d "$reference_library/Library"
test -x "$reference_unity"
test -f "$reference_source/$reference_scene"
[[ "$reference_scene" == Assets/* && "$reference_scene" == *.unity ]]
[[ "$reference_project" == "$reference_tool_root/Temp/"* ]]
[[ "$reference_output" == "$reference_tool_root/Temp/"* ]]
[[ "$reference_project" != "$reference_source" && "$reference_project" != "$reference_source/"* ]]
[[ "$reference_output" != "$reference_project" && "$reference_output" != "$reference_project/"* ]]
if [[ -e "$reference_project" ]]; then
  printf 'Reference destination already exists; choose a fresh owned path: %s\n' "$reference_project" >&2
  exit 2
fi
if pgrep -af '[U]nity.*-projectPath' | awk -v target="$reference_library" '{ for (i=1;i<NF;i++) if ($i=="-projectPath" && $(i+1)==target) found=1 } END { exit !found }'; then
  printf 'The Library seed project is open in an Editor: %s\n' "$reference_library" >&2
  exit 2
fi
command -v jq >/dev/null
mkdir -p -- "$reference_project/Library" "$reference_output"
find "$reference_source/Assets" "$reference_source/Packages" "$reference_source/ProjectSettings" -type f -print0 |
  sort -z | xargs -0 sha256sum > "$reference_output/original-authored-before.sha256"
cp -a --reflink=auto -- "$reference_source/Assets" "$reference_source/Packages" "$reference_source/ProjectSettings" "$reference_project/"
shopt -s dotglob nullglob
for reference_entry in "$reference_library/Library/"*; do
  # Keep asset/package download caches. Resolve packages and recompile managed
  # assemblies for the new explicit project instead of reusing process state.
  case "${reference_entry##*/}" in
    UnityMicroEditor|PackageManager|ScriptAssemblies|Bee|Search|burst.pid|ilpp.pid) continue ;;
  esac
  cp -a --reflink=auto -- "$reference_entry" "$reference_project/Library/"
done
mkdir -p -- "$reference_project/Assets/Editor/AgentParityValidation"
cp -- "$reference_tool_root/Integrations/EditorParity/DungeonEditorReference.cs" "$reference_project/Assets/Editor/AgentParityValidation/"
jq --arg runtime "file:$reference_tool_root/Packages/com.unitymicroeditor.runtime" \
  '.dependencies["com.unitymicroeditor.runtime"]=$runtime' \
  "$reference_project/Packages/manifest.json" > "$reference_project/Packages/manifest.reference.json"
mv -- "$reference_project/Packages/manifest.reference.json" "$reference_project/Packages/manifest.json"

reference_status=0
DISPLAY="${DISPLAY:-:0}" \
AGENT_DUNGEON_REFERENCE_PROJECT="$reference_project" \
AGENT_DUNGEON_REFERENCE_OUTPUT="$reference_output" \
AGENT_DUNGEON_REFERENCE_SCENE="$reference_scene" \
  "$reference_unity" -batchmode -projectPath "$reference_project" -buildTarget Linux64 \
  -executeMethod AgentParity.Editor.DungeonEditorReference.Run \
  -logFile "$reference_output/editor.log" || reference_status=$?

find "$reference_source/Assets" "$reference_source/Packages" "$reference_source/ProjectSettings" -type f -print0 |
  sort -z | xargs -0 sha256sum > "$reference_output/original-authored-after.sha256"
cmp -- "$reference_output/original-authored-before.sha256" "$reference_output/original-authored-after.sha256"
test -f "$reference_output/play.json"
exit "$reference_status"
