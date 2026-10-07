# Unity Micro Editor runtime

The shared C# package implements source scene loading, lifecycle barriers, native content delivery, scene patches, runtime reports and the agent control bridge. It has no dependency on any game's assemblies.

The host project and target projects reference this directory as a local Unity package. It requires SRP Core `17.6.0` and Collections `6.6.0`. The assembly and root namespace are `UnityMicroEditor`. Script `.meta` GUIDs are stable, so component references in authored scenes keep resolving.

Public project methods annotated with `[AgentTool("name")]` become discoverable agent tools. `[AgentToolArgument]` describes required fields and constraints on public DTOs. Calls execute on the Unity main thread; public MonoBehaviour methods bind to explicit live targets. The runtime also provides game-loop pause/step, supported Input System events, public UI dispatch and screen/camera capture. See [the agent guide](../../docs/AGENT_MCP.md) for MCP registration, identity guards and retry semantics.

Unity `6000.6.3f1` is the verified engine version. Game-specific packages must match the native host profile. `Editor/` contains the allowlisted public preparation/build mailbox and isolated prefab/component/native adapters. Project builds proxy to the real Editor. General Editor extension execution is not exposed; `Tests/` contains optional package EditMode tests rather than an agent test runner.
