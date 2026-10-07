using System.Text.Json;
using Mono.Cecil;
using Mono.Cecil.Cil;
using Mono.Cecil.Rocks;
using Unity.CompilationPipeline.Common.Diagnostics;
using Unity.CompilationPipeline.Common.ILPostProcessing;
using Unity.Netcode.Editor.CodeGen;

// The package's source mentions this Editor-only type in a runtime `if`,
// despite conditionally importing UnityEditor. Our requests reject
// UNITY_EDITOR, so that branch is unreachable. Keep the package sources
// unchanged and avoid loading UnityEditor assemblies into this SDK tool.
namespace Unity.Netcode.Editor.CodeGen
{
    internal sealed class InitializeOnLoadMethodAttribute : Attribute
    {
        public InitializeOnLoadMethodAttribute() => throw new InvalidOperationException("Editor initialization is unavailable in this runtime-only tool");
    }
}

internal sealed class CompiledInput : ICompiledAssembly
{
    public string Name { get; init; }
    public string[] References { get; init; }
    public string[] Defines { get; init; }
    public InMemoryAssembly InMemoryAssembly { get; set; }
}

internal sealed record Request(string Assembly, string[] References, string[] Defines, string Output, string[] LifecycleMethods);
internal sealed record RpcProof(string Type, string Method, string Handler, string[] SendCalls, string[] WriteCalls, string[] ReadCalls);
internal sealed record Initializer(string Assembly, string TypeName, string MethodName, int Phase);

internal static class Program
{
    private static readonly JsonSerializerOptions Json = new() { PropertyNamingPolicy = JsonNamingPolicy.CamelCase, PropertyNameCaseInsensitive = true };
    private static readonly string[] RpcAttributes = { "Unity.Netcode.RpcAttribute", "Unity.Netcode.ServerRpcAttribute", "Unity.Netcode.ClientRpcAttribute" };

    public static int Main(string[] args)
    {
        try
        {
            if (args.Length != 1) throw new InvalidDataException("Expected a postprocess request JSON");
            var request = JsonSerializer.Deserialize<Request>(File.ReadAllText(args[0]), Json) ?? throw new InvalidDataException("Empty request");
            if (!Path.IsPathRooted(request.Assembly) || !Path.IsPathRooted(request.Output) || request.References == null ||
                request.References.Length > 1024 || request.Defines == null || request.Defines.Contains("UNITY_EDITOR"))
                throw new InvalidDataException("Invalid standalone runtime postprocess request");
            foreach (var reference in request.References) if (!Path.IsPathRooted(reference)) throw new InvalidDataException("Reference must be absolute");
            var input = new CompiledInput
            {
                Name = Path.GetFileNameWithoutExtension(request.Assembly), References = request.References, Defines = request.Defines,
                InMemoryAssembly = new InMemoryAssembly(File.ReadAllBytes(request.Assembly), File.ReadAllBytes(Path.ChangeExtension(request.Assembly, ".pdb")))
            };
            using (var original = ModuleDefinition.ReadModule(new MemoryStream(input.InMemoryAssembly.PeData)))
            {
                // Roslyn drops unused references. Unity's assembly graph can
                // expose NGO to Assembly-CSharp without any emitted NGO usage;
                // the package ILPP cannot resolve its base module in that case.
                if (!original.AssemblyReferences.Any(r => r.Name == "Unity.Netcode.Runtime"))
                {
                    File.WriteAllBytes(request.Output, input.InMemoryAssembly.PeData);
                    File.WriteAllBytes(Path.ChangeExtension(request.Output, ".pdb"), input.InMemoryAssembly.PdbData);
                    Console.WriteLine(JsonSerializer.Serialize(new
                    {
                        schemaVersion = 1, verified = true, assembly = input.Name, required = false, rpcCount = 0,
                        rpcs = Array.Empty<RpcProof>(), lifecycleMethodsVerified = request.LifecycleMethods?.Length ?? 0,
                        runtimeInitializers = Array.Empty<Initializer>()
                    }, Json));
                    return 0;
                }
            }
            // These are the package's own IL postprocessors, compiled unchanged
            // together with this entry point. No Unity Editor process is involved.
            ILPostProcessor[] processors = { new INetworkSerializableILPP(), new INetworkMessageILPP(), new NetworkBehaviourILPP() };
            var diagnostics = new List<string>();
            foreach (var processor in processors)
            {
                if (!processor.WillProcess(input)) continue;
                var result = processor.Process(input) ?? throw new InvalidDataException("Netcode postprocessor returned no result");
                foreach (var diagnostic in result.Diagnostics)
                {
                    diagnostics.Add(diagnostic.MessageData);
                    if (diagnostic.DiagnosticType == DiagnosticType.Error) throw new InvalidDataException(diagnostic.MessageData);
                }
                input.InMemoryAssembly = result.InMemoryAssembly;
            }
            var resolver = new DefaultAssemblyResolver();
            foreach (var folder in request.References.Select(Path.GetDirectoryName).Distinct()) resolver.AddSearchDirectory(folder);
            using var assembly = AssemblyDefinition.ReadAssembly(new MemoryStream(input.InMemoryAssembly.PeData), new ReaderParameters
            {
                AssemblyResolver = resolver, ReadSymbols = true, SymbolStream = new MemoryStream(input.InMemoryAssembly.PdbData),
                SymbolReaderProvider = new PortablePdbReaderProvider(), InMemory = true
            });
            var module = assembly.MainModule;
            var rpcs = new List<RpcProof>();
            foreach (var type in module.GetTypes())
            {
                foreach (var method in type.Methods.Where(m => m.CustomAttributes.Any(a => RpcAttributes.Contains(a.AttributeType.FullName))))
                {
                    uint id = method.Hash();
                    var handler = type.Methods.SingleOrDefault(m => m.Name == "__rpc_handler_" + id)
                        ?? throw new InvalidDataException("Missing generated RPC handler: " + method.FullName);
                    string[] calls = Calls(method);
                    var sends = calls.Where(c => c.Contains("::__beginSend") || c.Contains("::__endSend")).ToArray();
                    if (sends.Length < 2 || !Calls(type.Methods.Single(m => m.Name == "__initializeRpcs")).Any(c => c.Contains("::__registerRpc")))
                        throw new InvalidDataException("RPC has no generated send/registration path: " + method.FullName);
                    var writes = calls.Where(c => c.Contains("Write") || c.Contains("Serialize")).ToArray();
                    var reads = Calls(handler).Where(c => c.Contains("Read") || c.Contains("Deserialize")).ToArray();
                    bool payload = method.Parameters.Any(p => !p.ParameterType.FullName.EndsWith("RpcParams", StringComparison.Ordinal));
                    if (payload && (writes.Length == 0 || reads.Length == 0)) throw new InvalidDataException("RPC payload has no generated serializer: " + method.FullName);
                    rpcs.Add(new RpcProof(type.FullName.Replace('/', '+'), method.Name, handler.Name, sends, writes, reads));
                }
            }
            // Preserve the source compiler's lifecycle bridges through ILPP.
            var allMethods = module.GetTypes().SelectMany(t => t.Methods).Select(m => m.DeclaringType.FullName.Replace('/', '+') + "." + m.Name).ToHashSet();
            foreach (var name in request.LifecycleMethods ?? Array.Empty<string>())
                if (!allMethods.Contains(name)) throw new InvalidDataException("ILPP removed a lifecycle bridge: " + name);
            var generated = module.Types.SingleOrDefault(t => t.FullName == "__GEN.NetworkVariableSerializationHelper")
                ?? throw new InvalidDataException("NGO did not generate NetworkVariable serialization initialization");
            var initialize = generated.Methods.Single(m => m.Name == "InitializeSerialization");
            // UnityPlay manually invokes generated runtime initialization through
            // a public bridge, just as it does for authored initialization.
            foreach (var attribute in initialize.CustomAttributes.Where(a => a.AttributeType.FullName == "UnityEngine.RuntimeInitializeOnLoadMethodAttribute").ToArray()) initialize.CustomAttributes.Remove(attribute);
            var bridgeType = new TypeDefinition("__GEN", "UnityPlayNetcodeInitializers", TypeAttributes.Public | TypeAttributes.Abstract | TypeAttributes.Sealed, module.TypeSystem.Object);
            var bridge = new MethodDefinition("__UnityPlay_RuntimeInitialize_Netcode", MethodAttributes.Public | MethodAttributes.Static, module.TypeSystem.Void);
            bridge.Body.Instructions.Add(Instruction.Create(OpCodes.Call, initialize));
            bridge.Body.Instructions.Add(Instruction.Create(OpCodes.Ret));
            bridgeType.Methods.Add(bridge);
            module.Types.Add(bridgeType);
            using var pe = new MemoryStream();
            using var pdb = new MemoryStream();
            assembly.Write(pe, new WriterParameters { WriteSymbols = true, SymbolWriterProvider = new PortablePdbWriterProvider(), SymbolStream = pdb });
            File.WriteAllBytes(request.Output, pe.ToArray());
            File.WriteAllBytes(Path.ChangeExtension(request.Output, ".pdb"), pdb.ToArray());
            Console.WriteLine(JsonSerializer.Serialize(new
            {
                schemaVersion = 1, verified = true, assembly = input.Name, required = true, rpcCount = rpcs.Count, rpcs, diagnostics,
                lifecycleMethodsVerified = request.LifecycleMethods?.Length ?? 0,
                runtimeInitializers = new[] { new Initializer(input.Name, bridgeType.FullName, bridge.Name, 0) }
            }, Json));
            return 0;
        }
        catch (Exception exception) { Console.Error.WriteLine(exception); return 1; }
    }

    private static string[] Calls(MethodDefinition method) => method.HasBody
        ? method.Body.Instructions.Select(i => i.Operand).OfType<MethodReference>().Select(m => m.FullName).ToArray()
        : Array.Empty<string>();
}
