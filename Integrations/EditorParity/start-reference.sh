#!/usr/bin/env bash
set -euo pipefail

tool_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
unity_editor=${UNITY_PARITY_EDITOR:-/home/shaypercool/Unity/Hub/Editor/6000.6.3f1/Editor/Unity}
parity_root="$tool_root/Temp/AgentParity"
editor_project="$parity_root/editor-project"
coplay_source="$parity_root/thirdparty/unity-mcp"
coplay_commit=30d22075093d1d35dfb0091c1c7550e9ad948577
coplay_url=http://127.0.0.1:8087

test -x "$unity_editor"
command -v uv >/dev/null
command -v jq >/dev/null
mkdir -p "$parity_root/thirdparty" "$parity_root/logs"
if [[ ! -d "$coplay_source/.git" ]]; then
  git clone --depth 1 --branch v10.2.0 https://github.com/CoplayDev/unity-mcp.git "$coplay_source"
fi
test "$(git -C "$coplay_source" rev-parse HEAD)" = "$coplay_commit"
if [[ ! -d "$editor_project" ]]; then
  cp -a "$tool_root/Samples/RawProject" "$editor_project"
  jq --arg runtime "file:$tool_root/Packages/com.unitymicroeditor.runtime" \
     --arg coplay "file:$coplay_source/MCPForUnity" \
     '.dependencies["com.unitymicroeditor.runtime"]=$runtime | .dependencies["com.coplaydev.unity-mcp"]=$coplay' \
     "$tool_root/HostProject/Packages/manifest.json" > "$editor_project/Packages/manifest.json"
fi
mkdir -p "$editor_project/Assets/Editor" "$editor_project/Assets/Scripts"
cp "$tool_root/Integrations/EditorParity/EditorParityReference.cs" "$editor_project/Assets/Editor/EditorParityReference.cs"
if [[ ! -f "$editor_project/Assets/Scripts/AgentParityProbe.cs" ]]; then
  cp "$tool_root/Integrations/EditorParity/AgentParityProbe.cs" "$editor_project/Assets/Scripts/AgentParityProbe.cs"
fi
uv sync --project "$coplay_source/Server" --no-dev

if curl --silent --fail --max-time 2 "$coplay_url/api/instances" >/dev/null; then
  printf 'Coplay already responds at %s; reusing its server.\n' "$coplay_url"
else
  nohup uv run --project "$coplay_source/Server" mcp-for-unity --transport http --http-url "$coplay_url" \
    > "$parity_root/logs/coplay-server.log" 2>&1 &
  printf '%s\n' "$!" > "$parity_root/coplay-server.pid"
fi
editor_running=false
for cmdline in /proc/[0-9]*/cmdline; do
  process_args=()
  if ! mapfile -d '' -t process_args < "$cmdline" 2>/dev/null; then
    continue
  fi
  # Import workers and Players can mention the same project path. Only the
  # explicitly selected Editor executable owns the interactive reference.
  [[ ${process_args[0]:-} == "$unity_editor" ]] || continue
  for ((index=1; index+1<${#process_args[@]}; index++)); do
    if [[ ${process_args[index]} == -projectPath && ${process_args[index+1]} == "$editor_project" ]]; then
      editor_running=true
      break
    fi
  done
  [[ $editor_running == true ]] && break
done
if [[ $editor_running == true ]]; then
  printf 'The owned Editor project is already running at %s.\n' "$editor_project"
else
  AGENT_PARITY_COPLAY_URL="$coplay_url" AGENT_PARITY_UVX_PATH="$(command -v uvx)" DISPLAY="${DISPLAY:-:0}" nohup "$unity_editor" \
    -projectPath "$editor_project" -buildTarget Linux64 \
    -executeMethod AgentParity.Editor.EditorParityReference.StartFromCommandLine \
    -logFile "$parity_root/logs/unity-editor.log" > "$parity_root/logs/unity-editor.stdout" 2>&1 &
  printf '%s\n' "$!" > "$parity_root/unity-editor.pid"
fi
printf 'Reference project: %s\nMCP endpoint: %s/mcp\nLogs: %s/logs\n' "$editor_project" "$coplay_url" "$parity_root"
