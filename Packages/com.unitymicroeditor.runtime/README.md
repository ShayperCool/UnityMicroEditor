# Unity Micro Editor runtime

The shared C# package implements source scene loading, lifecycle barriers, native content delivery, scene patches, runtime reports and the agent control bridge. It has no compile-time dependency on Dungeon gameplay assemblies.

Both Dungeon and the minimal host reference this directory as a local Unity package. It requires SRP Core `17.6.0` and Collections `6.6.0`. Script `.meta` files, assembly names, and namespaces are preserved so existing component references continue to resolve. The assembly remains `Dungeon.CLIPlay` for compatibility.

Public project methods annotated with `[AgentTool("name")]` become discoverable agent tools. `[AgentToolArgument]` describes required fields and constraints on public DTOs. Calls execute on the Unity main thread; public MonoBehaviour methods bind to explicit live targets. The runtime also provides game-loop pause/step, supported Input System events, public UI dispatch and screen/camera capture. See [the agent guide](../../docs/AGENT_MCP.md) for MCP registration, identity guards and retry semantics.

Unity `6000.6.3f1` is the verified engine version. Game-specific packages must match the native host profile. `Editor/` contains the allowlisted public preparation/build mailbox and isolated prefab/component/native adapters, alongside earlier build and benchmark utilities. Project builds proxy to the real Editor. General Editor extension execution is not exposed; `Tests/` contains optional package EditMode tests rather than an agent test runner.
