# Attributed tools, simulation and input reference

The separate `Assets/Scenes/AdvancedParity.unity` fixture exercises the agent runtime without changing `AgentParity.unity`. `AdvancedAgentProbe.cs` is created through Coplay `create_script`; the scene is created through Coplay `execute_code` using the public Editor calls in `CreateAdvancedParity.cs.txt`.

The executed reference uses Unity `6000.6.3f1`, Input System `1.20.0`, uGUI `2.6.0`, and the isolated project's explicit `activeInputHandler: 2` profile. Package installations have Coplay UPM receipts. Unity's native input-backend installation dialog blocked the connection; restarting the owned Editor with the explicit authored project profile activated the backend. The original scene hash remained `936d9b0db3ae1ca5dedb24ec7cf24f2c57e990566b995d5a9d6ab94c30b6e1d5`.

## Project API

Annotate public methods on public project types. The runtime discovers them automatically; projects do not need a registration bootstrap.

```csharp
using Dungeon.CLIPlay;

public sealed class IncreaseArgs
{
    [AgentToolArgument(Minimum = 1, Maximum = 10)]
    public int amount;
}

public static class ProjectTools
{
    [AgentTool("project.increase", Description = "Increase the project counter.")]
    public static IncreaseResult Increase(IncreaseArgs args)
    {
        // Project code, executed on Unity's main thread.
        return new IncreaseResult { value = args.amount };
    }
}

public sealed class IncreaseResult { public int value; }
```

`ReadOnly = true` advertises a method's read-only contract. Methods accept no parameter or one public DTO with writable public fields. Required fields are the default; `[AgentToolArgument(Required = false)]` makes a field optional. Values include booleans, strings, finite numbers, declared enum names, nullable value types, arrays, lists, bounded JSON dictionaries, and nested DTOs. Recursive schemas, Unity object parameters, unknown fields and unsupported types are rejected. The entire input shape and constraints are validated before DTO construction or method execution.

Public attributed `MonoBehaviour` methods require an explicit object target. Descriptors include live scene paths and source identities. Duplicate matching components on one target are rejected. Instance discovery follows scene component lifetime, including inactive objects. Destroyed instances disappear from the catalog. Static methods remain project-level tools.

Persistent game roots use the same discovery and hierarchy path. Public `FindObjectsByType` discovers valid loaded scenes omitted by `SceneManager.sceneCount`, including `DontDestroyOnLoad`; each scene's public root enumeration supplies root order. Inactive children remain discoverable. Entire owned importer and capture roots are excluded. The fixture's `advanced.persist` instance tool moves its Canvas root into the persistent scene and returns an actual scene-name-based `persistent` readback, then the driver checks catalog, instance invocation and hierarchy again.

Duplicate tool names are excluded and reported in catalog errors. Private methods are never discovered. Legacy `AgentRuntimeCommands.Register` remains compatible; collisions with attributed names are rejected. An exception or invalid output after execution begins reports uncertainty because the method may already have changed state. Inspect state before retrying.

The verified host uses Mono. Package `link.xml` preserves the owned agent API; SDK-compiled game DLLs retain attributed methods and DTO metadata. This does not establish an IL2CPP execution profile.

The fixture-only attributed `advanced.reset` tool restores value `5` and zeros callback/input counters at a paused checkpoint. This permits strict comparison without discarding counters that ran for different durations. Keep the existing overlay Canvas to test default UI behavior; its driven Transform/RectTransform values depend on each viewport. Declare viewport differences in the report instead of silently ignoring these fields.

## Executed Editor assertions

Receipts are under ignored `Temp/AgentParity/advanced-*.json`:

| Check | Editor result | Receipt |
| --- | --- | --- |
| Missing/unknown DTO fields, ranges and null string | Five inputs rejected before DTO constructor and method | `advanced-editor-tool-assertions.json` |
| Static and selected instance dispatch | Value `5 → 10`, two DTO constructors, main thread confirmed | `advanced-editor-tool-assertions.json` |
| Collision, unsupported contract, private method | Three names rejected; valid methods remain discoverable | `advanced-editor-tools.json` |
| Exception and non-JSON output | Both retain observable effects and report execution failure | `advanced-editor-tool-assertions.json` |
| Ambiguous target | Rejected before DTO construction | `advanced-editor-instance-lifetime.json` |
| Destroyed last instance | Instance tool disappears from catalog | `advanced-editor-instance-lifetime.json` |
| Editor pause | Update/FixedUpdate/LateUpdate remain `2/2/2` | `advanced-editor-paused-before.json` |
| Native Editor single step | Update and LateUpdate advance once; FixedUpdate advances zero times | `advanced-editor-step-input-assertions.json` |
| Public queued input during live Play | Actual game Update observes one A press and one left mouse press | `advanced-editor-live-input-assertions.json` |
| Public UI dispatch | Actual pointer-down/up/click handlers each run once | `advanced-editor-live-input-assertions.json` |

The first native step/input assertion remains a failed receipt: expecting one fixed tick was false, and default Editor routing consumed keyboard/mouse events outside the Game view. A second live-input reference sets public `InputSettings.editorInputBehaviorInPlayMode` to `AllDeviceInputAlwaysGoesToGameView`; actual game handlers then observe both events. The Player's native input route is checked independently.

## Simulation contract

`simulation_pause` gates game `FixedUpdate`, `PreUpdate`, `Update`, and `PreLateUpdate` stages through public PlayerLoop APIs. Bridge polling and rendering continue; native `Time.frameCount` advances. Audio listener pause applies while gated. This differs from stopping the Editor's whole Play loop.

`simulation_step` requests one game Update, one fixed tick and one LateUpdate, then pauses again. Its reply waits for actual stage counters. A failed invariant reports `step_failed`; a timeout reports `outcome_unknown`. The executed Player verified all three game callback deltas on the first step and three repeated steps. This fixed-tick contract is additional functionality: native `EditorApplication.Step` did not guarantee a fixed tick in the executed reference.

Legacy `pause`/`resume` and explicit `scaled_time_pause`/`scaled_time_resume` only control `Time.timeScale`.

## Input contract

Keyboard and mouse actions create owned virtual Input System devices and queue public state events for the next package update. They require the target's declared Input System profile (1 or 2), the matching host package, and initialized native package support. Legacy Input and OS injection are not advertised. UI actions use the live public EventSystem and targeted pointer down/up/click dispatch; they do not claim OS pointer hit-testing.

Readbacks include public AudioSource, Animator, Canvas and RectTransform values and a bounded whitelist of optional UI/input properties. Package-private serialized fields and generated instance IDs are excluded. Moving audio time, animation state and game counters need aligned pause/step checkpoints for meaningful comparisons.

## Screenshot contract

Default `screen` mode waits for the public end-of-frame capture API and returns the actual native viewport, including Screen Space Overlay UI. Bridge polling and the owned capture coroutine remain live during a gated simulation pause. The host must retain the public ScreenCapture module and have a graphics device. A requested width or height that differs from the actual viewport is rejected; omitting both uses its current dimensions. A resize or scene revision change before capture completes rejects that result.

Explicit `camera` mode selects a public Camera by source identity or scene path, or uses `Camera.main`. It renders into an owned render texture at a requested size through public built-in or supported render-pipeline APIs. It restores camera and render-texture state and does not include Screen Space Overlay UI. An unsupported render pipeline reports an error. The reply records mode, dimensions, frame, viewport, SHA-256 and immutable PNG path; it does not claim a resized camera image is a full-screen capture.

The runtime package compiled in the connected reference Editor after the capture changes (`advanced-capture-runtime-ready.json`) and in the independent host/native regression. The persistent-root additions compiled in `advanced-persistent-source-ready.json`. The fresh Advanced07 Player verified paused end-of-frame capture and overlay pixels.

## Executed Player and Editor comparison

The fresh host preparation completed through the independent public Editor bridge and SDK compiler: `Temp/AgentExpansion/advanced-auto07.json`. Actual startup is recorded in `advanced-ready07.json`: Unity `6000.6.3f1`, Player PID `3581708`, revision `9ca98db610bb672ed52bef3b411c5378`, NVIDIA hardware rendering, zero startup errors and exceptions. Earlier immutable failed preparations and startup receipts remain alongside it. They exposed Editor-only Input System types in the graph and stale Editor package code producing the wrong built-in Skybox resource path. The successful preparation uses the public Player compilation proof and an explicit owned-package refresh before host build.

The Go driver saved **26 passing checks** in `Temp/AgentExpansion/advanced-check07/assertions.json`, including two release-event assertions:

| Check | Actual Player result |
| --- | --- |
| Pause and 60 live render frames | Game Update/FixedUpdate/LateUpdate stay unchanged; bridge/render frames advance |
| Single step | Actual game counters each advance once, then freeze again |
| Keyboard, mouse and UI click | Game observes one A press, one left mouse press, and one pointer-down/up/click each |
| Release events | Both queued; subsequent repeated steps observe no new presses |
| DTO validation | Five invalid inputs rejected before DTO constructor or method |
| Dynamic MCP static and instance tools | Value `5 → 10`, two constructors, selected target and main thread confirmed |
| Exceptions and invalid JSON output | Both rejected with their observable effects retained; toolCalls `4` |
| Catalog | Valid attributed tools discovered automatically; collisions, unsupported contract and private method excluded |
| Paused full-screen screenshot | Native `960×540` screen PNG contains exactly `20,000` green Overlay pixels |
| Explicit camera screenshot | `960×540`, zero green Overlay pixels; independent dimensions honored |
| Screen size mismatch | Rejected instead of silently resizing the native viewport |
| Persistent receiver | Actual scene readback is `DontDestroyOnLoad`; catalog, selected invocation and four-object hierarchy remain available |

`advanced-repeat-assertions07.json` verifies three additional steps on the persistent receiver: cumulative game Update/FixedUpdate/LateUpdate counters `1/1/1`, `2/2/2`, `3/3/3`. Previously queued release events produce zero new key/mouse presses.

The Editor performs the same public attributed persistence call and subsequent selected instance invocation through real Coplay. `Temp/AgentParity/advanced-editor-persistent-assertions.json` confirms the special scene, three roots and four objects. Both paused engines then call the fixture reset tool before full native/component snapshots.

The strict comparison in `Temp/AgentExpansion/advanced-persistent-parity07.json` retains **18 differences**, all driven Overlay viewport geometry: Editor Game view `1223×455`, Player `960×540`. Root/child screen positions and Canvas RectTransform dimensions therefore differ. All other compared fields, component type/order and reset game counters match. No viewport or callback fields were discarded by the comparator.

The independent camera PNG comparison is exact: `advanced-camera-pixels07.json` reports **zero changed pixels** across `518,400` pixels, with identical SHA-256 `e5b66db2de3bcd55fe90128e1c9437c0a19fa82592794a1864356c4345cdf6f3`. Full screen images (`advanced-editor-screen.png`, `advanced-player-screen.png`) keep their actual dimensions. The Player image also contains Unity's Development Build watermark, so these full screen images are not claimed identical.

The final rebuilt CLI repeats the entire run against the same prepared Advanced07 source/host profile with a new persistent session output. `advanced-session-final-ready.json` confirms actual Player PID `3663014`, revision `26278a73a8232ed9eb10ef2c05434607`, zero startup errors/exceptions and NVIDIA OpenGLCore rendering. `advanced-check-final/assertions.json` passes all **26 checks**; `advanced-repeat-assertions-final.json` passes the three repeated callback steps and release readbacks. The final camera remains byte-identical to the native Editor reference (`advanced-camera-pixels-final.json`), and the final reset snapshot retains exactly the same 18 declared viewport differences (`advanced-persistent-parity-final.json`). `advanced-final-handoff-status.json` records a live paused Player with four completed steps; fixture counters/value are reset for inspection. The Editor and Coplay remain running in idle Edit Mode.

After the compiler, orphan-model override and large native animation fixes, the complete rebuilt CLI repeats those assertions once more. `advanced-complete-ready-confirmed.json` records actual Player PID `3908774`, revision `ca2a85ce8a9839aab97de2b563749676`, zero startup errors/exceptions and NVIDIA rendering. `advanced-check-complete/assertions.json` passes all 26 checks; `advanced-repeat-assertions-complete.json` verifies three further callback steps and zero new input presses after release. The camera PNG remains byte-identical to the Editor (`advanced-camera-pixels-complete.json`, zero changed pixels). `advanced-persistent-parity-complete.json` retains the same 18 viewport differences byte for byte. `advanced-complete-handoff-status.json` confirms that the Player remains paused with four completed steps, and `advanced-complete-handoff-reset.json` records value `5` and zero fixture counters. The public MCP endpoint and Player remain available.

The final verified CLI uses the refreshed public Editor graph and scene proof in
`advanced-auto-08-refresh`, with the existing compatible host and hash-validated
native catalog. This refresh resolves the changed bridge DLL; the launcher also
now finds its exact assembly configuration through the explicit owned source
manifest. No cache hash was manually relabeled. The fresh Player PID `4116864`,
revision `fbd4320a59c188544da7d3ffc17ad89c`, reported zero startup errors and
exceptions on NVIDIA OpenGLCore in `advanced-verified-ready-confirmed.json`.
`advanced-check-verified/assertions.json` passes all 26 checks, and
`advanced-repeat-assertions-verified.json` confirms three further exact callback
steps and no new key or mouse presses after release. The camera PNG is
byte-identical to the Editor, with zero changed pixels across 518,400 pixels in
`advanced-camera-pixels-verified.json`. The full four-object comparison retains
exactly the same 18 viewport geometry differences in
`advanced-persistent-parity-verified.json`; those difference records also match
the preceding report byte for byte. `advanced-verified-handoff-status.json`
records a live paused Player with four completed steps, while
`advanced-verified-handoff-reset.json` confirms value `5` and zero fixture
counters. MCP remains available on `127.0.0.1:8093`; authored game files and the
reference Editor/Coplay state were not changed by this verification.

## Repeatable Player assertions

Start the configured Player for the explicit isolated AdvancedParity project, wait for readiness, and keep this fixture exclusive while the assertions run. The driver pauses it and uses the fixture reset tool. It leaves the Player paused for inspection.

```bash
go build -o /tmp/parity-mcp-call ./cmd/mcp-call
go run ./cmd/advanced-check -client /tmp/parity-mcp-call \
  -url http://127.0.0.1:8093/mcp \
  -output /explicit/ignored/new-evidence-directory
```

Run these commands from `Integrations/EditorParity`. Each command receipt and `assertions.json` persist in the requested directory. The checks read game counters after queued input and a real simulation step, compare them after 60 bridge/render frames, and inspect PNG pixels for the fixture's green Overlay image. A successful input receipt alone cannot pass. An explicit pending receipt is polled with the same immutable request ID; uncertain or rejected execution is retained without a new mutation ID.
