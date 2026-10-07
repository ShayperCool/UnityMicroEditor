# Unity Editor reference

This harness runs a real Unity Editor as a reference and compares its readbacks with the Unity Micro Editor Player. It is used when changing source authoring, the compiler graph or the runtime bridge.

It prepares an isolated Unity `6000.6.3f1` project from `Samples/RawProject` and installs the Coplay MCP package and server, version `10.2.0` (commit `30d22075093d1d35dfb0091c1c7550e9ad948577`). The sample's own assets are not touched, and the runtime package comes from this checkout. Coplay's server runs in its own Python environment, confined to the reference; the comparison clients are Go.

## Start the reference

Build the host first (`./build-host.sh`), then:

```bash
./Integrations/EditorParity/start-reference.sh
```

`UNITY_PARITY_EDITOR` selects the Editor installation. The generated project, third-party sources, logs and evidence live under the ignored `Temp/AgentParity`. The server listens only on `http://127.0.0.1:8087/mcp`. The script reuses a running server or an already open project, and does not stop other Editors.

On first start, `EditorParityReference.cs` creates `Assets/Scenes/AgentParity.unity` with a cube prefab, an inactive child, a camera, a light and a lifecycle probe (`AgentParityProbe.cs`). It registers a Coplay custom tool, `agent_parity_reference`, with these actions:

- `snapshot` reads the scene with the same public `AgentRuntimeSnapshot` reader the Player bridge uses: inactive objects, root order, world and local transforms, built-in component readbacks and serialized MonoBehaviour fields.
- `export_graph` captures Unity's Player assembly graph and MonoScript GUID/type identities.
- `scene_proof` records the project, Unity version, authored scene hash, dirty and Play Mode state, and lightmap and light probe counts.

To stop the reference, terminate only the Editor and Coplay processes this setup started.

## Clients

Run these from the repository root.

`mcp-call` performs an MCP handshake and one request over Streamable HTTP, and exits nonzero on a JSON-RPC error, a tool `isError` or a nested `success=false`:

```bash
go run ./Integrations/EditorParity/cmd/mcp-call -method tools/list
go run ./Integrations/EditorParity/cmd/mcp-call -method tools/call \
  -params '{"name":"agent_parity_reference","arguments":{"action":"snapshot","output":"reference.json"}}'
```

`compare` checks a Player hierarchy against an Editor snapshot:

```bash
go run ./Integrations/EditorParity/cmd/compare \
  -reference /absolute/play-shared.json \
  -actual /absolute/player-hierarchy-response.json \
  -output /absolute/comparison.json
```

It compares object order, root sibling indices, component order and types, inactive states, serialized fields and lifecycle counts. Numbers use an absolute tolerance of `1e-5`, and a quaternion matches its negation. Source identities and the Editor-only `globalObjectId` and `prefabInstance` provenance are ignored, as is the loader's own resource owner object. Missing components and unresolved references are mismatches. Exit status `0` means equal, `1` a behavioral difference, `2` invalid or incomplete evidence. A paginated hierarchy must be complete before comparison.

`compare-source` checks authored source against a public `SerializedObject` readback produced in a batch Editor by `SourceAuthoringReference.cs.txt`:

```bash
go run ./Integrations/EditorParity/cmd/compare-source \
  -source /absolute/final-hierarchy.json \
  -assets /absolute/asset-sources.json \
  -editor /absolute/editor-readback.json \
  -output /absolute/source-parity.json
```

`compare-png` measures rendering separately from serialized state. Capture both sides at the same size first:

```bash
go run ./Integrations/EditorParity/cmd/compare-png \
  -reference /absolute/editor-960x540.png \
  -actual /absolute/player-960x540.png \
  -ignore-bottom 24 -output /absolute/render-comparison.json
```

The report always includes full-frame differences. `-ignore-bottom` adds a second metric that skips the development-build watermark rows.

`advanced-check` drives a Player through attributed tools, simulation stepping, input and screenshots against the `AdvancedParity` scene, which `CreateAdvancedParity.cs.txt` and `AdvancedAgentProbe.cs` set up:

```bash
go build -o /tmp/parity-mcp-call ./Integrations/EditorParity/cmd/mcp-call
go run ./Integrations/EditorParity/cmd/advanced-check -client /tmp/parity-mcp-call \
  -url http://127.0.0.1:8093/mcp -output /absolute/new-evidence-directory
```

## Comparing correctly

- Compare authoring snapshots separately from Play Mode snapshots. Entering Play Mode changes lifecycle counters, and runtime changes are not authoring edits.
- Reload the saved scene in the Editor before a Play Mode readback, so its in-memory scene cannot hide an external edit. External writers do not join the MCP authoring lock; let writers take turns.
- Use a fresh Player hierarchy after each update, not one from an earlier PID or revision.
- Do not normalize away unsupported components or null references to make a comparison pass.
- After a lost reply, verify state before retrying a mutation.

## Known reference behavior

- The root `siblingIndex` in snapshots is the position in `Scene.GetRootGameObjects()`. The standalone Player returns `0` from `Transform.GetSiblingIndex()` for every root.
- Coplay's `manage_components` fails on this Unity version because it calls a removed Editor API. `manage_gameobject(action="modify", component_properties=...)` works.
- Changing a custom tool's parameter schema during a domain reload does not replace the definition the server registered first. Restart the Coplay server.
- A scene changed on disk by another writer can leave Unity's reload dialog open during a domain reload and block Coplay's reconnect. Restart the reference Editor.
- Screenshots use `manage_camera(action="screenshot")`, and the output folder must be inside the reference project.
