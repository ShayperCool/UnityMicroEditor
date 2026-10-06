using System.Text.Json;
using System.Buffers.Binary;
using System.Reflection.Metadata;
using System.Reflection.Metadata.Ecma335;
using System.Reflection.PortableExecutable;
using System.Security.Cryptography;
using System.Text;
using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.CSharp;
using Microsoft.CodeAnalysis.CSharp.Syntax;

internal sealed record SourceInput(string Path, string OriginalPath, string OutputPath);
internal sealed record Request(string AssemblyName, string[] References, string[] Defines,
    SourceInput[] Sources, string MarkerPath, bool AllowUnsafe = false,
    Dictionary<string, int>? ExecutionOrders = null, string[]? NativeBaseAssemblies = null);
internal sealed record CallbackInfo(string Type, string Phase, string Source, int Order, bool Synthetic = false);
internal sealed record RuntimeInitializerInfo(string Assembly, string TypeName, string MethodName, int Phase);

internal static class DeclarationHash
{
    // This deliberately fingerprints more than the public API. Private and
    // internal declarations, compiler attributes, constants, generic/layout
    // records, assembly references and friend-assembly attributes are included.
    // New body-only MemberRef/local-signature records may cause an extra rebuild.
    // IL bodies, their offsets, user-string literals and MVID are excluded.
    internal static Dictionary<string, string> Read(string[] paths)
    {
        var result = new Dictionary<string, string>(StringComparer.Ordinal);
        if (paths.Length == 0 || paths.Length > 512)
            throw new InvalidOperationException("Expected a bounded list of managed DLLs");
        foreach (var path in paths)
        {
            using var stream = File.OpenRead(path);
            if (stream.Length > 1024L * 1024 * 1024)
                throw new InvalidOperationException("Declaration input exceeds the DLL bound");
            using var pe = new PEReader(stream);
            if (!pe.HasMetadata)
                throw new InvalidOperationException("Declaration input has no managed metadata");
            var reader = pe.GetMetadataReader();
            if (!reader.IsAssembly)
                throw new InvalidOperationException("Declaration input must be an assembly");
            var metadata = pe.GetMetadata().GetContent().ToArray();
            using var hash = IncrementalHash.CreateHash(HashAlgorithmName.SHA256);
            hash.AppendData(Encoding.UTF8.GetBytes("unity-play-declarations-v1\n" + reader.MetadataVersion + "\n"));
            foreach (var table in Enum.GetValues<TableIndex>())
            {
                int rows = reader.GetTableRowCount(table);
                if (rows == 0) continue;
                int rowSize = reader.GetTableRowSize(table);
                int offset = reader.GetTableMetadataOffset(table);
                byte[] data = metadata.AsSpan(offset, checked(rows * rowSize)).ToArray();
                if (table == TableIndex.MethodDef)
                {
                    // ECMA-335 MethodDef starts with the body RVA. Moving or
                    // changing a method body must not invalidate dependents.
                    for (int row = 0; row < rows; row++)
                        data.AsSpan(row * rowSize, sizeof(uint)).Clear();
                }
                AppendFrame(hash, (int)table, rows, data);
            }
            foreach (var heap in new[] { HeapIndex.String, HeapIndex.Blob })
            {
                int size = reader.GetHeapSize(heap);
                int offset = reader.GetHeapMetadataOffset(heap);
                AppendFrame(hash, 256 + (int)heap, size, metadata.AsSpan(offset, size));
            }
            result.Add(path, Convert.ToHexString(hash.GetHashAndReset()).ToLowerInvariant());
        }
        return result;
    }

    private static void AppendFrame(IncrementalHash hash, int kind, int count, ReadOnlySpan<byte> bytes)
    {
        Span<byte> header = stackalloc byte[12];
        BinaryPrimitives.WriteInt32LittleEndian(header, kind);
        BinaryPrimitives.WriteInt32LittleEndian(header[4..], count);
        BinaryPrimitives.WriteInt32LittleEndian(header[8..], bytes.Length);
        hash.AppendData(header);
        hash.AppendData(bytes);
    }

    internal static Dictionary<string, string[]> TypeDefinitions(string[] paths)
    {
        if (paths.Length == 0 || paths.Length > 512)
            throw new InvalidOperationException("Expected a bounded list of managed DLLs");
        var result = new Dictionary<string, string[]>(StringComparer.Ordinal);
        foreach (var path in paths)
        {
            using var stream = File.OpenRead(path);
            if (stream.Length > 1024L * 1024 * 1024)
                throw new InvalidOperationException("Type input exceeds the DLL bound");
            using var pe = new PEReader(stream);
            if (!pe.HasMetadata)
                throw new InvalidOperationException("Type input has no managed metadata");
            var reader = pe.GetMetadataReader();
            if (!reader.IsAssembly)
                throw new InvalidOperationException("Type input must be an assembly");
            string FullName(TypeDefinitionHandle handle)
            {
                var definition = reader.GetTypeDefinition(handle);
                string name = reader.GetString(definition.Name);
                var parent = definition.GetDeclaringType();
                if (!parent.IsNil) return FullName(parent) + "+" + name;
                string ns = reader.GetString(definition.Namespace);
                return ns.Length == 0 ? name : ns + "." + name;
            }
            result.Add(path, reader.TypeDefinitions.Select(FullName).Where(name => name != "<Module>")
                .OrderBy(name => name, StringComparer.Ordinal).ToArray());
        }
        return result;
    }
}

internal static class Program
{
    internal static readonly string[] Phases = { "Awake", "OnEnable", "OnDisable", "OnDestroy" };
    internal const string Lifecycle = "global::Dungeon.CLIPlay.Raw.RawLifecycle";
    internal const string PrefabTransforms = "global::Dungeon.CLIPlay.Raw.RawPrefabTransforms";

    public static int Main(string[] args)
    {
        try
        {
            if (args.Length >= 2 && args[0] == "--declaration-hashes")
            {
                Console.WriteLine(JsonSerializer.Serialize(DeclarationHash.Read(args.Skip(1).ToArray())));
                return 0;
            }
            if (args.Length >= 2 && args[0] == "--type-definitions")
            {
                Console.WriteLine(JsonSerializer.Serialize(DeclarationHash.TypeDefinitions(args.Skip(1).ToArray())));
                return 0;
            }
            if (args.Length != 1) throw new InvalidOperationException("Expected one request JSON path");
            var request = JsonSerializer.Deserialize<Request>(File.ReadAllText(args[0]))
                ?? throw new InvalidOperationException("Empty transformation request");
            var options = new CSharpParseOptions(LanguageVersion.CSharp9, preprocessorSymbols: request.Defines);
            var trees = request.Sources.Select(source => CSharpSyntaxTree.ParseText(
                File.ReadAllText(source.Path), options, source.OriginalPath)).ToArray();
            var compilation = CSharpCompilation.Create(request.AssemblyName, trees,
                request.References.Distinct().Select(path => MetadataReference.CreateFromFile(path)),
                new CSharpCompilationOptions(OutputKind.DynamicallyLinkedLibrary, allowUnsafe: request.AllowUnsafe)
                    .WithMetadataImportOptions(MetadataImportOptions.All));
            var errors = compilation.GetDiagnostics().Where(d => d.Severity == DiagnosticSeverity.Error).ToArray();
            if (errors.Length != 0) throw new InvalidOperationException(string.Join("\n", errors.Select(d => d.ToString())));
            var mono = compilation.GetTypeByMetadataName("UnityEngine.MonoBehaviour");
            var callbacks = new List<CallbackInfo>();
            var runtimeInitializers = new List<RuntimeInitializerInfo>();
            var activeAndEnabledReads = 0;
            var setActiveCalls = 0;
            var transformRootReads = 0;
            var getComponentInChildrenCalls = 0;
            var scriptableObjectCallbacks = 0;
            var outputs = new List<(SourceInput Source, string Text)>();
            for (int index = 0; index < trees.Length; index++)
            {
                var model = compilation.GetSemanticModel(trees[index]);
                var rewriter = new LifecycleRewriter(model, mono, callbacks, runtimeInitializers, request);
                var rewritten = rewriter.Visit(trees[index].GetRoot())!.ToFullString();
                activeAndEnabledReads += rewriter.ActiveAndEnabledReads;
                setActiveCalls += rewriter.SetActiveCalls;
                transformRootReads += rewriter.TransformRootReads;
                getComponentInChildrenCalls += rewriter.GetComponentInChildrenCalls;
                scriptableObjectCallbacks += rewriter.ScriptableObjectCallbacks;
                outputs.Add((request.Sources[index], rewritten));
            }
            // Every source assembly is marked after all its lifecycle methods
            // have passed validation. The native loader verifies this attribute.
            var marker = compilation.GetTypeByMetadataName("Dungeon.CLIPlay.Raw.RawLifecycleInstrumentedAttribute");
            var api = compilation.GetTypeByMetadataName("Dungeon.CLIPlay.Raw.RawLifecycle");
            if (marker is null || api is null || !api.GetMembers("ShouldCollect").Any() || !api.GetMembers("Enqueue").Any()
                || (activeAndEnabledReads != 0 && !api.GetMembers("IsActiveAndEnabled").Any()))
                throw new InvalidOperationException("Host lacks lifecycle barrier API/marker; prepare a matching universal host");
            if (setActiveCalls != 0 && !api!.GetMembers("SetActive").Any())
                throw new InvalidOperationException("Host lacks lifecycle SetActive bridge; prepare a matching universal host");
            var prefabTransforms = compilation.GetTypeByMetadataName("Dungeon.CLIPlay.Raw.RawPrefabTransforms");
            if (transformRootReads != 0 && prefabTransforms?.GetMembers("Root").OfType<IMethodSymbol>().Any(method =>
                method.IsStatic && method.DeclaredAccessibility == Accessibility.Public && method.Parameters.Length == 1
                && method.Parameters[0].Type.ToDisplayString() == "UnityEngine.Transform"
                && method.ReturnType.ToDisplayString() == "UnityEngine.Transform") != true)
                throw new InvalidOperationException("Host lacks prefab Transform.root bridge; prepare a matching universal host");
            if (getComponentInChildrenCalls != 0)
            {
                foreach (var receiver in new[] { "UnityEngine.Component", "UnityEngine.GameObject" })
                foreach (bool generic in new[] { false, true })
                foreach (bool includeInactive in new[] { false, true })
                {
                    int count = 1 + (generic ? 0 : 1) + (includeInactive ? 1 : 0);
                    if (prefabTransforms?.GetMembers("GetComponentInChildren").OfType<IMethodSymbol>().Any(method =>
                        method.IsStatic && method.DeclaredAccessibility == Accessibility.Public && method.Parameters.Length == count
                        && method.TypeParameters.Length == (generic ? 1 : 0) && method.Parameters[0].Type.ToDisplayString() == receiver
                        && (generic || method.Parameters[1].Type.ToDisplayString() == "System.Type")
                        && (!includeInactive || method.Parameters[^1].Type.SpecialType == SpecialType.System_Boolean)
                        && (generic ? SymbolEqualityComparer.Default.Equals(method.ReturnType, method.TypeParameters[0])
                            : method.ReturnType.ToDisplayString() == "UnityEngine.Component")) != true)
                        throw new InvalidOperationException("Host lacks prefab GetComponentInChildren bridge overloads; prepare a matching universal host");
                }
            }
            var scriptableMarker = compilation.GetTypeByMetadataName("Dungeon.CLIPlay.Raw.RawScriptableObjectInstrumentedAttribute");
            bool scriptableObjectBarrier = scriptableMarker is not null && api!.GetMembers("ShouldCollect").OfType<IMethodSymbol>().Any(m =>
                m.Parameters.Length == 2 && m.Parameters[0].Type.ToDisplayString() == "UnityEngine.ScriptableObject");
            if (scriptableObjectCallbacks != 0 && !scriptableObjectBarrier)
                throw new InvalidOperationException("Host lacks ScriptableObject lifecycle barrier API/marker; prepare a matching universal host");
            File.WriteAllText(request.MarkerPath,
                "[assembly: global::Dungeon.CLIPlay.Raw.RawLifecycleInstrumentedAttribute(1)]\n" +
                (scriptableObjectBarrier ? "[assembly: global::Dungeon.CLIPlay.Raw.RawScriptableObjectInstrumentedAttribute(1)]\n" : ""));
            foreach (var output in outputs)
            {
                Directory.CreateDirectory(System.IO.Path.GetDirectoryName(output.Source.OutputPath)!);
                File.WriteAllText(output.Source.OutputPath, output.Text);
            }
            Console.WriteLine(JsonSerializer.Serialize(new { SchemaVersion = 1, Verified = true,
                Callbacks = callbacks, Phases, ScriptableObjectBarrier = scriptableObjectBarrier, RuntimeInitializers = runtimeInitializers,
                ActiveAndEnabledReads = activeAndEnabledReads, SetActiveCalls = setActiveCalls,
                TransformRootReads = transformRootReads, GetComponentInChildrenCalls = getComponentInChildrenCalls }));
            return 0;
        }
        catch (Exception error)
        {
            Console.Error.WriteLine(error.Message);
            return 1;
        }
    }

    private sealed class LifecycleRewriter : CSharpSyntaxRewriter
    {
        private readonly SemanticModel model;
        private readonly INamedTypeSymbol? mono;
        private readonly List<CallbackInfo> callbacks;
        private readonly List<RuntimeInitializerInfo> runtimeInitializers;
        private readonly Request request;
        private readonly Dictionary<int, IdentifierNameSyntax> conditionalReceivers = new();
        private readonly HashSet<string> usedIdentifiers;
        private int nextReceiver;
        public int ActiveAndEnabledReads { get; private set; }
        public int SetActiveCalls { get; private set; }
        public int TransformRootReads { get; private set; }
        public int GetComponentInChildrenCalls { get; private set; }
        public int ScriptableObjectCallbacks { get; private set; }

        public LifecycleRewriter(SemanticModel model, INamedTypeSymbol? mono, List<CallbackInfo> callbacks,
            List<RuntimeInitializerInfo> runtimeInitializers, Request request)
        { this.model = model; this.mono = mono; this.callbacks = callbacks;
          this.runtimeInitializers = runtimeInitializers; this.request = request;
          usedIdentifiers = model.SyntaxTree.GetRoot().DescendantTokens()
              .Where(token => token.IsKind(SyntaxKind.IdentifierToken)).Select(token => token.ValueText).ToHashSet(); }

        private bool IsMonoBehaviour(INamedTypeSymbol? type)
        {
            for (var current = type; current is not null; current = current.BaseType)
                if (SymbolEqualityComparer.Default.Equals(current, mono) && mono is not null) return true;
            return false;
        }

        private bool IsActiveAndEnabledRead(SyntaxNode node)
        {
            var property = model.GetSymbolInfo(node).Symbol as IPropertySymbol;
            return property?.Name == "isActiveAndEnabled"
                && property.ContainingType.ToDisplayString() == "UnityEngine.Behaviour";
        }

        private bool IsSetActive(SyntaxNode node)
        {
            var method = model.GetSymbolInfo(node).Symbol as IMethodSymbol;
            return method?.Name == "SetActive" && method.ContainingType.ToDisplayString() == "UnityEngine.GameObject";
        }

        private bool IsTransformRootRead(SyntaxNode node)
        {
            var property = model.GetSymbolInfo(node).Symbol as IPropertySymbol;
            return property?.Name == "root" && property.ContainingType.ToDisplayString() == "UnityEngine.Transform"
                && property.ContainingAssembly.Identity.Name == "UnityEngine.CoreModule";
        }

        private bool IsGetComponentInChildren(SyntaxNode node)
        {
            var method = model.GetSymbolInfo(node).Symbol as IMethodSymbol;
            return method?.Name == "GetComponentInChildren"
                && method.ContainingType.ToDisplayString() is "UnityEngine.Component" or "UnityEngine.GameObject"
                && method.ContainingAssembly.Identity.Name == "UnityEngine.CoreModule";
        }

        private static ConditionalAccessExpressionSyntax? ConditionalOwner(MemberBindingExpressionSyntax node) =>
            node.Ancestors().OfType<ConditionalAccessExpressionSyntax>()
                .FirstOrDefault(conditional => conditional.WhenNotNull.Span.Contains(node.Span));

        private ExpressionSyntax RootCall(ExpressionSyntax receiver, SyntaxNode original)
        {
            TransformRootReads++;
            return SyntaxFactory.InvocationExpression(SyntaxFactory.ParseExpression(PrefabTransforms + ".Root"),
                SyntaxFactory.ArgumentList(SyntaxFactory.SingletonSeparatedList(SyntaxFactory.Argument(receiver))))
                .WithTriviaFrom(original);
        }

        public override SyntaxNode? VisitConditionalAccessExpression(ConditionalAccessExpressionSyntax node)
        {
            // A static bridge needs the receiver hidden by ?. . Lift only the
            // conditional that owns that binding, preserving single evaluation
            // and CLR null checks (Unity's overloaded == is deliberately absent).
            bool needsReceiver = node.WhenNotNull.DescendantNodesAndSelf().Any(candidate =>
                (IsTransformRootRead(candidate) || candidate is InvocationExpressionSyntax && IsGetComponentInChildren(candidate))
                && ((candidate is InvocationExpressionSyntax invocation ? invocation.Expression : candidate) is MemberBindingExpressionSyntax binding
                    ? ConditionalOwner(binding)?.SpanStart == node.SpanStart
                    : (candidate is InvocationExpressionSyntax call ? call.Expression : candidate) is MemberAccessExpressionSyntax member && member.Expression.DescendantNodesAndSelf()
                        .OfType<MemberBindingExpressionSyntax>().Any(binding => ConditionalOwner(binding)?.SpanStart == node.SpanStart)));
            if (!needsReceiver) return base.VisitConditionalAccessExpression(node);
            var resultType = model.GetTypeInfo(node).Type;
            if (resultType is null || resultType.SpecialType == SpecialType.System_Void)
                Reject(node, "Void null-conditional chains containing Transform.root or GetComponentInChildren are unsupported; use an explicit null check");
            string name;
            do { name = "__UnityPlay_RootReceiver_" + nextReceiver++; } while (!usedIdentifiers.Add(name));
            var receiver = (ExpressionSyntax)Visit(node.Expression)!;
            conditionalReceivers.Add(node.SpanStart, SyntaxFactory.IdentifierName(name));
            ExpressionSyntax whenNotNull;
            try { whenNotNull = (ExpressionSyntax)Visit(node.WhenNotNull)!; }
            finally { conditionalReceivers.Remove(node.SpanStart); }
            return SyntaxFactory.ParseExpression("(" + receiver.WithoutTrivia().ToFullString() + " is { } " + name
                + " ? " + whenNotNull.WithoutTrivia().ToFullString() + " : ("
                + resultType!.ToDisplayString(SymbolDisplayFormat.FullyQualifiedFormat) + ")null)").WithTriviaFrom(node);
        }

        private void GuardLifecycleCall(SyntaxNode node)
        {
            var symbol = model.GetSymbolInfo(node).Symbol as IMethodSymbol;
            if (symbol is null || !Phases.Contains(symbol.Name) || !(IsMonoBehaviour(symbol.ContainingType) || IsScriptableObject(symbol.ContainingType))) return;
            var invocation = node as InvocationExpressionSyntax ?? node.Parent as InvocationExpressionSyntax;
            var method = node.Ancestors().OfType<MethodDeclarationSyntax>().FirstOrDefault();
            if (invocation?.Expression is MemberAccessExpressionSyntax { Expression: BaseExpressionSyntax }
                && method?.Identifier.ValueText == symbol.Name) return;
            Reject(node, "Direct calls/method groups of authored lifecycle callbacks are unsupported during raw initialization; only explicit base calls in the same lifecycle phase are supported");
        }

        private int ExecutionOrder(INamedTypeSymbol type)
        {
            foreach (var declaration in type.DeclaringSyntaxReferences)
            {
                var path = declaration.SyntaxTree.FilePath;
                if (Path.GetFileNameWithoutExtension(path) == type.Name
                    && request.ExecutionOrders?.TryGetValue(path, out int order) == true && order != 0) return order;
            }
            for (var current = type; current is not null; current = current.BaseType)
            {
                var attribute = current.GetAttributes().FirstOrDefault(a => a.AttributeClass?.ToDisplayString() == "UnityEngine.DefaultExecutionOrder");
                if (attribute is not null && attribute.ConstructorArguments.Length == 1)
                    return (int)attribute.ConstructorArguments[0].Value!;
            }
            return 0;
        }

        public override SyntaxNode? VisitInvocationExpression(InvocationExpressionSyntax node)
        {
            // nameof observes the property name and never invokes its getter.
            if (model.GetOperation(node)?.Kind == OperationKind.NameOf) return node;
            GuardLifecycleCall(node);
            if (IsGetComponentInChildren(node))
            {
                ExpressionSyntax receiver;
                SimpleNameSyntax name;
                if (node.Expression is MemberAccessExpressionSyntax member)
                {
                    receiver = member.Expression is BaseExpressionSyntax ? SyntaxFactory.ThisExpression() : (ExpressionSyntax)Visit(member.Expression)!;
                    name = member.Name;
                }
                else if (node.Expression is MemberBindingExpressionSyntax binding
                    && ConditionalOwner(binding) is { } owner && conditionalReceivers.TryGetValue(owner.SpanStart, out var conditionalReceiver))
                { receiver = conditionalReceiver; name = binding.Name; }
                else if (node.Expression is SimpleNameSyntax simple)
                { receiver = SyntaxFactory.ThisExpression(); name = simple; }
                else
                { Reject(node, "Unresolved null-conditional GetComponentInChildren receiver; use an explicit null check"); return node; }
                var arguments = (ArgumentListSyntax)Visit(node.ArgumentList)!;
                var originalMethod = (IMethodSymbol)model.GetSymbolInfo(node).Symbol!;
                arguments = arguments.WithArguments(SyntaxFactory.SeparatedList(arguments.Arguments.Select(argument =>
                {
                    if (argument.NameColon is null) return argument;
                    var parameter = originalMethod.Parameters.Single(parameter => parameter.Name == argument.NameColon.Name.Identifier.ValueText);
                    string bridgeName = parameter.Type.SpecialType == SpecialType.System_Boolean ? "includeInactive" : "type";
                    return argument.WithNameColon(argument.NameColon.WithName(SyntaxFactory.IdentifierName(bridgeName)
                        .WithTriviaFrom(argument.NameColon.Name)));
                })));
                GetComponentInChildrenCalls++;
                // Preserve argument order and evaluation. Unity's Component uses
                // "t" where GameObject uses "type"; the bridge normalizes names.
                return SyntaxFactory.InvocationExpression(SyntaxFactory.MemberAccessExpression(SyntaxKind.SimpleMemberAccessExpression,
                    SyntaxFactory.ParseExpression(PrefabTransforms), (SimpleNameSyntax)Visit(name)!),
                    arguments.WithArguments(arguments.Arguments.Insert(0, SyntaxFactory.Argument(receiver)))).WithTriviaFrom(node);
            }
            if (IsSetActive(node))
            {
                if (node.Expression is not MemberAccessExpressionSyntax member)
                    Reject(node, "Null-conditional GameObject.SetActive calls are unsupported by the initialization barrier; use an explicit null check");
                var receiver = (ExpressionSyntax)Visit(((MemberAccessExpressionSyntax)node.Expression).Expression)!;
                var arguments = (ArgumentListSyntax)Visit(node.ArgumentList)!;
                SetActiveCalls++;
                return SyntaxFactory.InvocationExpression(SyntaxFactory.ParseExpression(Lifecycle + ".SetActive"),
                    arguments.WithArguments(SyntaxFactory.SeparatedList(arguments.Arguments
                        .Select(argument => argument.WithNameColon(null)))
                        .Insert(0, SyntaxFactory.Argument(receiver))))
                    .WithTriviaFrom(node);
            }
            return base.VisitInvocationExpression(node);
        }

        public override SyntaxNode? VisitMemberBindingExpression(MemberBindingExpressionSyntax node)
        {
            if (IsActiveAndEnabledRead(node))
                Reject(node, "Null-conditional Behaviour.isActiveAndEnabled reads are unsupported by the initialization barrier; use an explicit null check");
            if (IsSetActive(node)) Reject(node, "Null-conditional GameObject.SetActive calls are unsupported by the initialization barrier");
            GuardLifecycleCall(node);
            var owner = ConditionalOwner(node);
            if (owner is not null && conditionalReceivers.TryGetValue(owner.SpanStart, out var receiver))
            {
                if (IsTransformRootRead(node)) return RootCall(receiver, node);
                return SyntaxFactory.MemberAccessExpression(SyntaxKind.SimpleMemberAccessExpression, receiver,
                    (SimpleNameSyntax)Visit(node.Name)!).WithTriviaFrom(node);
            }
            return base.VisitMemberBindingExpression(node);
        }

        public override SyntaxNode? VisitMemberAccessExpression(MemberAccessExpressionSyntax node)
        {
            GuardLifecycleCall(node);
            if (IsSetActive(node))
                Reject(node, "GameObject.SetActive method groups are unsupported by the initialization barrier");
            if (IsGetComponentInChildren(node))
                Reject(node, "GetComponentInChildren method groups are unsupported for raw prefab templates");
            if (IsTransformRootRead(node)) return RootCall((ExpressionSyntax)Visit(node.Expression)!, node);
            if (!IsActiveAndEnabledRead(node)) return base.VisitMemberAccessExpression(node);
            ActiveAndEnabledReads++;
            var receiver = node.Expression is BaseExpressionSyntax ? SyntaxFactory.ThisExpression()
                : (ExpressionSyntax)Visit(node.Expression)!;
            return SyntaxFactory.InvocationExpression(SyntaxFactory.ParseExpression(Lifecycle + ".IsActiveAndEnabled"),
                SyntaxFactory.ArgumentList(SyntaxFactory.SingletonSeparatedList(SyntaxFactory.Argument(receiver))))
                .WithTriviaFrom(node);
        }

        public override SyntaxNode? VisitIdentifierName(IdentifierNameSyntax node)
        {
            if (node.Parent is not MemberAccessExpressionSyntax && node.Parent is not MemberBindingExpressionSyntax)
                GuardLifecycleCall(node);
            if (node.Parent is MemberAccessExpressionSyntax || node.Parent is MemberBindingExpressionSyntax
                || !IsActiveAndEnabledRead(node)) return base.VisitIdentifierName(node);
            ActiveAndEnabledReads++;
            return SyntaxFactory.ParseExpression(Lifecycle + ".IsActiveAndEnabled(this)").WithTriviaFrom(node);
        }

        private bool IsScriptableObject(INamedTypeSymbol? type)
        {
            for (var current = type; current is not null; current = current.BaseType)
                if (current.ToDisplayString() == "UnityEngine.ScriptableObject") return true;
            return false;
        }

        private static bool IsCallback(IMethodSymbol method) => Phases.Contains(method.Name)
            && !method.IsStatic && method.ReturnsVoid && method.Parameters.Length == 0;

        private bool VerifiedAssembly(IAssemblySymbol assembly, bool scriptable) => assembly.GetAttributes().Any(a =>
            a.AttributeClass?.ToDisplayString() == "Dungeon.CLIPlay.Raw." + (scriptable ? "RawScriptableObjectInstrumentedAttribute" : "RawLifecycleInstrumentedAttribute")
            && a.ConstructorArguments.Length == 1 && a.ConstructorArguments[0].Value is int value && value == 1);

        private bool IsNativeBase(IMethodSymbol method) => request.NativeBaseAssemblies?.Contains(
            method.ContainingAssembly.Identity.Name, StringComparer.Ordinal) == true;

        private static string MetadataTypeName(INamedTypeSymbol type)
        {
            var names = new Stack<string>();
            for (var current = type; current is not null; current = current.ContainingType) names.Push(current.MetadataName);
            var prefix = type.ContainingNamespace.IsGlobalNamespace ? "" : type.ContainingNamespace.ToDisplayString() + ".";
            return prefix + string.Join("+", names);
        }

        private void AddRuntimeInitializers(ClassDeclarationSyntax node, INamedTypeSymbol type, List<MemberDeclarationSyntax> members)
        {
            foreach (var method in node.Members.OfType<MethodDeclarationSyntax>())
            {
                var symbol = model.GetDeclaredSymbol(method)!;
                var attribute = symbol.GetAttributes().FirstOrDefault(a =>
                    a.AttributeClass?.ToDisplayString() == "UnityEngine.RuntimeInitializeOnLoadMethodAttribute");
                if (attribute is null) continue;
                if (!symbol.IsStatic || !symbol.ReturnsVoid || symbol.Parameters.Length != 0 || symbol.TypeParameters.Length != 0
                    || symbol.IsAbstract || symbol.IsExtern || method.Modifiers.Any(SyntaxKind.AsyncKeyword)
                    || method.Modifiers.Any(SyntaxKind.PartialKeyword) || method.Body is null && method.ExpressionBody is null)
                    Reject(method, "Runtime initializers require a nonasync static void parameterless body");
                for (var containing = type; containing is not null; containing = containing.ContainingType)
                    if (containing.TypeParameters.Length != 0 || containing.DeclaredAccessibility == Accessibility.Private)
                        Reject(method, "Runtime initializers on generic types or private nested types are unsupported");
                var phase = attribute.ConstructorArguments.Length == 0 ? 0 : (int)attribute.ConstructorArguments[0].Value!;
                if (phase < 0 || phase > 4) Reject(method, "Unknown RuntimeInitializeLoadType value");
                var bridge = "__UnityPlay_RuntimeInitialize_" + runtimeInitializers.Count;
                while (type.GetMembers(bridge).Length != 0) bridge += "_";
                members.Add(SyntaxFactory.ParseMemberDeclaration($"public static void {bridge}() {{ {method.Identifier.Text}(); }}")!);
                runtimeInitializers.Add(new RuntimeInitializerInfo(request.AssemblyName, MetadataTypeName(type), bridge, phase));
            }
        }

        public override SyntaxNode? VisitStructDeclaration(StructDeclarationSyntax node)
        {
            foreach (var method in node.Members.OfType<MethodDeclarationSyntax>())
                if (model.GetDeclaredSymbol(method)!.GetAttributes().Any(attribute =>
                    attribute.AttributeClass?.ToDisplayString() == "UnityEngine.RuntimeInitializeOnLoadMethodAttribute"))
                    Reject(method, "Runtime initializers on struct types are unsupported; use a static class");
            return base.VisitStructDeclaration(node);
        }

        private string HelperName(INamedTypeSymbol type, string phase)
        {
            var helper = "__UnityPlay_" + phase;
            while (type.GetMembers(helper).Length != 0) helper += "_";
            return helper;
        }

        private BlockSyntax Wrapper(string phase, string helper, int order) => (BlockSyntax)SyntaxFactory.ParseStatement("{" +
            $"if (!{Lifecycle}.ShouldCollect(this, {Lifecycle}.Phase.{phase})) {{ {helper}(); return; }}" +
            $"{Lifecycle}.Enqueue(this, {Lifecycle}.Phase.{phase}, {helper}, {order});" + "}");

        public override SyntaxNode? VisitClassDeclaration(ClassDeclarationSyntax node)
        {
            var type = model.GetDeclaredSymbol(node)!;
            var lifecycleType = IsMonoBehaviour(type) || IsScriptableObject(type);
            if (!lifecycleType)
            {
                var ordinary = (ClassDeclarationSyntax)base.VisitClassDeclaration(node)!;
                var ordinaryMembers = ordinary.Members.ToList();
                AddRuntimeInitializers(node, type, ordinaryMembers);
                return ordinary.WithMembers(SyntaxFactory.List(ordinaryMembers));
            }
            var nativeBaseCallbacks = new Dictionary<string, IMethodSymbol>();
            var seenBasePhases = new HashSet<string>();
            for (var parent = type.BaseType; parent is not null && parent.ToDisplayString() != "UnityEngine.MonoBehaviour"
                 && parent.ToDisplayString() != "UnityEngine.ScriptableObject"; parent = parent.BaseType)
            {
                foreach (var callback in parent.GetMembers().OfType<IMethodSymbol>().Where(IsCallback))
                {
                    if (!seenBasePhases.Add(callback.Name)) continue;
                    if (parent.DeclaringSyntaxReferences.Length != 0 || VerifiedAssembly(parent.ContainingAssembly, IsScriptableObject(parent))) continue;
                    if (!IsNativeBase(callback)) Reject(node, "Inherited lifecycle callbacks come from an unverified external assembly");
                    if (!nativeBaseCallbacks.ContainsKey(callback.Name)) nativeBaseCallbacks.Add(callback.Name, callback);
                }
            }
            var members = new List<MemberDeclarationSyntax>();
            foreach (var member in node.Members)
            {
                if (member is not MethodDeclarationSyntax method || !Phases.Contains(method.Identifier.ValueText))
                { members.Add((MemberDeclarationSyntax)Visit(member)!); continue; }
                var symbol = model.GetDeclaredSymbol(method)!;
                if (!symbol.ReturnsVoid || symbol.Parameters.Length != 0 || symbol.TypeParameters.Length != 0
                    || symbol.IsStatic || symbol.IsExtern || method.Modifiers.Any(SyntaxKind.AsyncKeyword)
                    || method.Modifiers.Any(SyntaxKind.PartialKeyword))
                    Reject(method, "Lifecycle callbacks require a nonstatic, nonasync void parameterless body; overloads/generic/partial methods are unsupported");
                for (var containing = type; containing is not null; containing = containing.ContainingType)
                    if (containing.TypeParameters.Length != 0) Reject(method, "Lifecycle callbacks on generic engine object classes are unsupported");
                if (symbol.IsAbstract) { members.Add(method); continue; }
                if (method.Body is null && method.ExpressionBody is null)
                    Reject(method, "Lifecycle callbacks require a body or an abstract declaration");
                var order = ExecutionOrder(type);
                var phase = method.Identifier.ValueText;
                var helperName = HelperName(type, phase);
                var visitedMethod = (MethodDeclarationSyntax)base.VisitMethodDeclaration(method)!;
                var helper = visitedMethod.WithIdentifier(SyntaxFactory.Identifier(helperName))
                    .WithAttributeLists(default)
                    .WithModifiers(SyntaxFactory.TokenList(SyntaxFactory.Token(SyntaxKind.PrivateKeyword)
                        .WithTrailingTrivia(SyntaxFactory.Space)))
                    .WithLeadingTrivia(default(SyntaxTriviaList));
                var location = method.GetLocation().GetLineSpan();
                string escapedPath = location.Path.Replace("\\", "\\\\").Replace("\"", "\\\"");
                if (helper.Body is not null)
                    helper = helper.WithBody(helper.Body.WithOpenBraceToken(helper.Body.OpenBraceToken.WithTrailingTrivia(
                        SyntaxFactory.ParseTrailingTrivia($"\n#line {helper.Body.OpenBraceToken.GetLocation().GetLineSpan().StartLinePosition.Line + 1} \"{escapedPath}\"\n"))));
                helper = helper.WithTrailingTrivia(SyntaxFactory.ParseTrailingTrivia("\n#line default\n"));
                members.Add(method.WithBody(Wrapper(phase, helperName, order)).WithExpressionBody(null).WithSemicolonToken(default));
                members.Add(helper);
                callbacks.Add(new CallbackInfo(type.ToDisplayString(), phase, location.Path, order));
                if (IsScriptableObject(type)) ScriptableObjectCallbacks++;
            }
            // An explicitly approved host package base cannot be transformed.
            // Wrap an accessible inherited message in the source subclass and
            // call its original implementation only after graph hydration.
            if (type.DeclaringSyntaxReferences[0].GetSyntax() == node)
            {
                foreach (var pair in nativeBaseCallbacks)
                {
                    var phase = pair.Key;
                    if (type.GetMembers(phase).OfType<IMethodSymbol>().Any(IsCallback)) continue;
                    var inherited = pair.Value;
                    if (inherited.IsAbstract) continue;
                    if (!model.Compilation.IsSymbolAccessibleWithin(inherited, type))
                        Reject(node, $"Native base callback {inherited.ToDisplayString()} is inaccessible; a public/protected bridge is required");
                    var helper = HelperName(type, phase);
                    var order = ExecutionOrder(type);
                    var access = inherited.DeclaredAccessibility == Accessibility.Public ? "public" : "protected";
                    var modifier = (inherited.IsVirtual || inherited.IsOverride) && !inherited.IsSealed ? "override" : "new";
                    var source = $"{access} {modifier} void {phase}() {Wrapper(phase, helper, order)} private void {helper}() {{ base.{phase}(); }}";
                    members.AddRange(SyntaxFactory.ParseCompilationUnit("class Generated { " + source + " }")
                        .Members.OfType<ClassDeclarationSyntax>().Single().Members);
                    callbacks.Add(new CallbackInfo(type.ToDisplayString(), phase, node.SyntaxTree.FilePath, order, true));
                    if (IsScriptableObject(type)) ScriptableObjectCallbacks++;
                }
            }
            var hasAwake = false;
            for (var current = type; current is not null && current.ToDisplayString() != "UnityEngine.MonoBehaviour"
                 && current.ToDisplayString() != "UnityEngine.ScriptableObject"; current = current.BaseType)
                if (current.GetMembers("Awake").OfType<IMethodSymbol>().Any(IsCallback))
                { hasAwake = true; break; }
            if (IsMonoBehaviour(type) && !hasAwake && type.DeclaringSyntaxReferences[0].GetSyntax() == node)
            {
                if (type.ContainingType is not null || type.TypeParameters.Length != 0)
                    Reject(node, "Synthetic Awake on nested/generic MonoBehaviour classes is unsupported");
                var helper = HelperName(type, "Awake");
                var order = ExecutionOrder(type);
                var source = $"private void Awake() {Wrapper("Awake", helper, order)} private void {helper}() {{ }}";
                members.AddRange(SyntaxFactory.ParseCompilationUnit("class Generated { " + source + " }")
                    .Members.OfType<ClassDeclarationSyntax>().Single().Members);
                callbacks.Add(new CallbackInfo(type.ToDisplayString(), "Awake", node.SyntaxTree.FilePath, order, true));
            }
            AddRuntimeInitializers(node, type, members);
            return node.WithMembers(SyntaxFactory.List(members));
        }

        private static void Reject(SyntaxNode node, string message)
        {
            var location = node.GetLocation().GetLineSpan();
            throw new InvalidOperationException($"{location.Path}({location.StartLinePosition.Line + 1}): {message}");
        }
    }
}
