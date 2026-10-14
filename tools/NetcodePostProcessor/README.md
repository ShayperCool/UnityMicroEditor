# Netcode post-processing after SDK compilation

The compiler runs this small C# executable after Roslyn for assemblies whose prepared graph references `Unity.Netcode.Runtime`. It uses the unmodified `Editor/CodeGen` sources from the project's pinned Netcode for GameObjects package: `INetworkSerializableILPP`, `INetworkMessageILPP` and `NetworkBehaviourILPP`. Each processor is called through the public `ILPostProcessor.Process(ICompiledAssembly)`. The Unity Editor is not started.

The helper is built with the .NET 8 SDK from the Unity installation. It references `Unity.CompilationPipeline.Common`, the Mono.Cecil package, Unity runtime modules and the unstripped standalone Netcode reference from `Library/Bee/artifacts/StandaloneLinux64_CodeGen`. The last is needed because the Player's linker removes friend-access metadata that the package's CodeGen requires. When processing game DLLs, the resolver gets the real Player references.

In the package source, a reference to an Editor initialization attribute sits inside an ordinary `if`, although `using UnityEditor` is conditional. The helper has an unreachable placeholder for that type. Requests with `UNITY_EDITOR` defined are refused and UnityEditor assemblies are not loaded. Third-party package sources are not changed.

After post-processing, the helper checks RPC handlers, send and registration paths, and serializer calls for payloads. Public runtime bridges added by the lifecycle compiler must survive. Netcode initialization for `NetworkVariable` is wrapped in a public `__UnityPlay_RuntimeInitialize_Netcode`; its descriptor is passed to the loader, which calls it after the scene is created and before `Start`.

Roslyn drops unused assembly references. If the emitted DLL does not actually use Netcode, the helper returns its original bytes with a confirmed `required=false`.

The cache key covers the helper source, the package CodeGen source, SDK and compiler binaries, every helper dependency and the hashes of the built helper. Processed DLLs and PDBs are verified on reuse.

SDK-only check, without a Player or Editor:

```bash
UME_NETCODE_SDK=/absolute/Unity/Editor/Unity \
UNITY_MICRO_EDITOR_TEST_PROJECT=/absolute/prepared/game/project \
UME_HOT_RELOAD_HOST=/absolute/prepared/game/project/Library/UnityMicroEditor/stage/project/Builds/UnityMicroEditor/<id>-host/Project.x86_64 \
  cargo test --locked --lib compiler::tests::netcode_postprocessor_sdk -- --nocapture
```

`UME_HOT_RELOAD_HOST` names the host Player that `auto-prepare` built for the project, whose `Managed` directory holds the assemblies the fixture is compiled against. Without it the test looks for `Builds/CLIPlayHybridHost/UnityPlayHost.x86_64` in the project, where earlier hosts were built.
