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
using MethodAttributes = System.Reflection.MethodAttributes;

internal sealed record SourceInput(string Path, string OriginalPath, string OutputPath);
internal sealed record Request(string AssemblyName, string[] References, string[] Defines,
    SourceInput[] Sources, bool AllowUnsafe = false, Dictionary<string, string>? ReferenceHashes = null);

/// <summary>The errors of the sources themselves: the one failure of a served request that is
/// reported as it stands (see <see cref="Program.Serve"/>).</summary>
internal sealed class CompilationErrors : InvalidOperationException
{
    public CompilationErrors(string message) : base(message) { }
}

/// <summary>
/// The references of the requests a served helper ran, by path and by the SHA-256 the driver checked
/// before it sent the request. A reference read for the first time is hashed here and must have that
/// content; one that a later request names with other content is read again. Entries no request of the
/// last four used are dropped. Nothing else outlives a request.
/// </summary>
internal sealed class References
{
    readonly Dictionary<(string Path, string Hash), (MetadataReference Reference, long Used)> entries = new();
    long request;

    public void Begin() => request++;

    public MetadataReference Get(string path, IReadOnlyDictionary<string, string>? hashes)
    {
        if (hashes is null || !hashes.TryGetValue(path, out var hash) || hash.Length != 64)
            return MetadataReference.CreateFromFile(path);
        if (entries.TryGetValue((path, hash), out var entry))
        {
            entries[(path, hash)] = (entry.Reference, request);
            return entry.Reference;
        }
        var bytes = File.ReadAllBytes(path);
        var actual = Convert.ToHexString(SHA256.HashData(bytes)).ToLowerInvariant();
        if (actual != hash)
            throw new InvalidOperationException($"The reference {path} does not have the content the request names.");
        var reference = MetadataReference.CreateFromImage(bytes, filePath: path);
        entries[(path, hash)] = (reference, request);
        return reference;
    }

    public void Trim()
    {
        foreach (var key in entries.Where(entry => entry.Value.Used < request - 3).Select(entry => entry.Key).ToList())
            entries.Remove(key);
    }
}
internal sealed record RuntimeInitializerInfo(string Assembly, string TypeName, string MethodName, int Phase);
internal sealed record SourceDeclarationRequest(string AssemblyName, string[] Defines, string[] EditorDefines, SourceInput[] Sources);

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

    internal static Dictionary<string, string[]> TypeDefinitions(string[] paths, bool publicOnly = false)
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
            bool IsPublic(TypeDefinitionHandle handle)
            {
                var definition = reader.GetTypeDefinition(handle);
                var visibility = definition.Attributes & System.Reflection.TypeAttributes.VisibilityMask;
                var parent = definition.GetDeclaringType();
                return parent.IsNil ? visibility == System.Reflection.TypeAttributes.Public
                    : visibility == System.Reflection.TypeAttributes.NestedPublic && IsPublic(parent);
            }
            result.Add(path, reader.TypeDefinitions.Where(handle => !publicOnly || IsPublic(handle)).Select(FullName).Where(name => name != "<Module>")
                .OrderBy(name => name, StringComparer.Ordinal).ToArray());
        }
        return result;
    }
}

/// <summary>
/// The rows a build writes into <c>RuntimeInitializeOnLoads.json</c> for an assembly the engine
/// runs as it is: one per static method marked <c>[RuntimeInitializeOnLoadMethod]</c>, with the
/// load type the attribute names (<c>AfterSceneLoad</c> when it names none). A nested type is
/// named after the type that declares it, separated by '/'.
/// </summary>
internal static class RuntimeInitializerTable
{
    const string AttributeNamespace = "UnityEngine", AttributeName = "RuntimeInitializeOnLoadMethodAttribute";

    internal sealed record Row(string assemblyName, string nameSpace, string className, string methodName,
        int loadTypes, bool isUnityClass);

    public static List<Row> Read(string path)
    {
        using var stream = File.OpenRead(path);
        using var pe = new PEReader(stream);
        var reader = pe.GetMetadataReader();
        string assembly = reader.GetString(reader.GetAssemblyDefinition().Name);
        var rows = new List<Row>();
        foreach (var typeHandle in reader.TypeDefinitions)
        {
            var type = reader.GetTypeDefinition(typeHandle);
            foreach (var methodHandle in type.GetMethods())
            {
                var method = reader.GetMethodDefinition(methodHandle);
                foreach (var attributeHandle in method.GetCustomAttributes())
                {
                    var attribute = reader.GetCustomAttribute(attributeHandle);
                    var (ns, name, parameters) = Constructor(reader, attribute.Constructor);
                    if (ns != AttributeNamespace || name != AttributeName) continue;
                    if ((method.Attributes & MethodAttributes.Static) == 0)
                        throw new InvalidOperationException($"runtime initializer is not static: {reader.GetString(method.Name)}");
                    int load = 0;
                    if (parameters == 1)
                    {
                        var value = reader.GetBlobReader(attribute.Value);
                        if (value.ReadUInt16() != 1) throw new InvalidOperationException("custom attribute blob has no prolog");
                        load = value.ReadInt32();
                    }
                    var (typeNamespace, typeName) = Names(reader, typeHandle);
                    rows.Add(new Row(assembly, typeNamespace, typeName, reader.GetString(method.Name), load,
                        assembly.StartsWith("Unity", StringComparison.Ordinal)));
                }
            }
        }
        return rows;
    }

    static (string Namespace, string Name) Names(MetadataReader reader, TypeDefinitionHandle handle)
    {
        var type = reader.GetTypeDefinition(handle);
        var declaring = type.GetDeclaringType();
        if (declaring.IsNil) return (reader.GetString(type.Namespace), reader.GetString(type.Name));
        var (ns, outer) = Names(reader, declaring);
        return (ns, outer + "/" + reader.GetString(type.Name));
    }

    static (string Namespace, string Name, int Parameters) Constructor(MetadataReader reader, EntityHandle constructor)
    {
        BlobHandle signature;
        EntityHandle owner;
        if (constructor.Kind == HandleKind.MemberReference)
        {
            var member = reader.GetMemberReference((MemberReferenceHandle)constructor);
            signature = member.Signature;
            owner = member.Parent;
        }
        else if (constructor.Kind == HandleKind.MethodDefinition)
        {
            var definition = reader.GetMethodDefinition((MethodDefinitionHandle)constructor);
            signature = definition.Signature;
            owner = definition.GetDeclaringType();
        }
        else return ("", "", 0);
        var blob = reader.GetBlobReader(signature);
        blob.ReadSignatureHeader();
        int parameters = blob.ReadCompressedInteger();
        switch (owner.Kind)
        {
            case HandleKind.TypeReference:
                var reference = reader.GetTypeReference((TypeReferenceHandle)owner);
                return (reader.GetString(reference.Namespace), reader.GetString(reference.Name), parameters);
            case HandleKind.TypeDefinition:
                var definition = reader.GetTypeDefinition((TypeDefinitionHandle)owner);
                return (reader.GetString(definition.Namespace), reader.GetString(definition.Name), parameters);
            default:
                return ("", "", parameters);
        }
    }
}

internal static class Program
{
    public static int Main(string[] args)
    {
        if (args.Length == 1 && args[0] == "--serve") return Serve();
        return Run(args, Console.Out, Console.Error, null, out _);
    }

    /// <summary>
    /// The helper for the life of a watched session: one JSON request per line on standard input, one
    /// answer per line on standard output, each what the one-shot run of its arguments would exit with
    /// and print. Only the rewrite of an assembly, <c>--type-definitions</c> and
    /// <c>--declaration-hashes</c> are served; everything else is answered with exit 64 and never run. The answer's
    /// <c>kind</c> is <c>diagnostics</c> for errors of the sources, the one failure the driver reports as
    /// it stands. The end of standard input ends the process.
    /// </summary>
    internal static int Serve()
    {
        using var frames = Console.OpenStandardOutput();
        using var input = new StreamReader(Console.OpenStandardInput(), new UTF8Encoding(false));
        TextWriter console = Console.Out, consoleError = Console.Error;
        var references = new References();
        void Frame(object value)
        {
            frames.Write(JsonSerializer.SerializeToUtf8Bytes(value));
            frames.WriteByte((byte)'\n');
            frames.Flush();
        }
        Frame(new Dictionary<string, object> { ["ready"] = true, ["protocol"] = 1 });
        string? line;
        while ((line = input.ReadLine()) != null)
        {
            if (line.Length == 0) continue;
            ServeRequest? request = null;
            try { request = JsonSerializer.Deserialize<ServeRequest>(line); } catch (JsonException) { }
            var output = new StringWriter();
            var error = new StringWriter();
            int exit;
            string kind = "";
            string[] arguments = request?.arguments ?? Array.Empty<string>();
            bool served = arguments.Length == 1 && !arguments[0].StartsWith("--", StringComparison.Ordinal)
                || arguments.Length >= 2 && (arguments[0] == "--type-definitions" || arguments[0] == "--declaration-hashes");
            if (request is null || !served)
            {
                exit = 64;
                kind = "refused";
                error.WriteLine(request is null ? "Unreadable request" : "Not served: " + string.Join(" ", arguments));
            }
            else
            {
                // A stray write of any code goes into the answer, never between two frames.
                Console.SetOut(output);
                Console.SetError(error);
                try
                {
                    if (!string.IsNullOrEmpty(request.directory)) Directory.SetCurrentDirectory(request.directory);
                    references.Begin();
                    exit = Run(arguments, output, error, references, out kind);
                    references.Trim();
                }
                finally
                {
                    Console.SetOut(console);
                    Console.SetError(consoleError);
                }
            }
            Frame(new Dictionary<string, object>
            {
                ["id"] = request?.id ?? 0, ["exit"] = exit, ["output"] = output.ToString(),
                ["error"] = error.ToString(), ["kind"] = kind,
            });
        }
        return 0;
    }

    internal sealed record ServeRequest(long id, string? directory, string[]? arguments);

    /// <summary>One run of the helper. <paramref name="references"/> keeps reference metadata between
    /// the requests of a served helper; a one-shot run has none and reads every reference.</summary>
    internal static int Run(string[] args, TextWriter stdout, TextWriter stderr, References? references, out string failure)
    {
        failure = "";
        try
        {
            if (args.Length >= 2 && args[0] == "--declaration-hashes")
            {
                stdout.WriteLine(JsonSerializer.Serialize(DeclarationHash.Read(args.Skip(1).ToArray())));
                return 0;
            }
            if (args.Length >= 2 && (args[0] == "--type-definitions" || args[0] == "--public-type-definitions"))
            {
                stdout.WriteLine(JsonSerializer.Serialize(DeclarationHash.TypeDefinitions(args.Skip(1).ToArray(), args[0] == "--public-type-definitions")));
                return 0;
            }
            if (args.Length == 2 && args[0] == "--runtime-initializers")
            {
                stdout.WriteLine(JsonSerializer.Serialize(RuntimeInitializerTable.Read(args[1])));
                return 0;
            }
            if (args.Length == 2 && args[0] == "--source-type-declarations")
            {
                stdout.WriteLine(JsonSerializer.Serialize(ReadSourceDeclarations(args[1])));
                return 0;
            }
            if (args.Length != 1) throw new InvalidOperationException("Expected one request JSON path");
            var request = JsonSerializer.Deserialize<Request>(File.ReadAllText(args[0]))
                ?? throw new InvalidOperationException("Empty transformation request");
            var options = new CSharpParseOptions(LanguageVersion.CSharp9, preprocessorSymbols: request.Defines);
            var trees = request.Sources.Select(source => CSharpSyntaxTree.ParseText(
                File.ReadAllText(source.Path), options, source.OriginalPath)).ToArray();
            var compilation = CSharpCompilation.Create(request.AssemblyName, trees,
                request.References.Distinct().Select(path => references is null
                    ? MetadataReference.CreateFromFile(path) : references.Get(path, request.ReferenceHashes)),
                new CSharpCompilationOptions(OutputKind.DynamicallyLinkedLibrary, allowUnsafe: request.AllowUnsafe)
                    .WithMetadataImportOptions(MetadataImportOptions.All));
            var errors = compilation.GetDiagnostics().Where(d => d.Severity == DiagnosticSeverity.Error).ToArray();
            if (errors.Length != 0) throw new CompilationErrors(string.Join("\n", errors.Select(d => d.ToString())));
            var runtimeInitializers = new List<RuntimeInitializerInfo>();
            var outputs = new List<(SourceInput Source, string Text)>();
            var scriptTypes = new Dictionary<string, string>(StringComparer.Ordinal);
            for (int index = 0; index < trees.Length; index++)
            {
                var model = compilation.GetSemanticModel(trees[index]);
                string filenameType = Path.GetFileNameWithoutExtension(request.Sources[index].OriginalPath);
                var primaryTypes = trees[index].GetRoot().DescendantNodes().OfType<ClassDeclarationSyntax>()
                    .Select(node => model.GetDeclaredSymbol(node))
                    .Where(type => type is not null && type.ContainingType is null && type.Arity == 0 && type.Name == filenameType)
                    .Select(type => type!.ToDisplayString(SymbolDisplayFormat.CSharpErrorMessageFormat))
                    .Distinct(StringComparer.Ordinal).ToArray();
                if (primaryTypes.Length > 1)
                    throw new InvalidOperationException("Ambiguous primary MonoScript class in " + request.Sources[index].OriginalPath);
                if (primaryTypes.Length == 1) scriptTypes.Add(request.Sources[index].OriginalPath, primaryTypes[0]);
                int initializerCount = runtimeInitializers.Count;
                var rewriter = new InitializerRewriter(model, runtimeInitializers, request);
                var visited = rewriter.Visit(trees[index].GetRoot())!;
                // Every statement and member must stand on the line it has in the source. A
                // rewrite that moved one would make frames, breakpoints and caller line numbers
                // name the authored file with a wrong line, which is worse than naming the
                // copy: such a file is written again without directives, like a source that
                // sets its own lines.
                if (rewriter.LineDirectives && !rewriter.LinesKept(visited))
                {
                    runtimeInitializers.RemoveRange(initializerCount, runtimeInitializers.Count - initializerCount);
                    rewriter = new InitializerRewriter(model, runtimeInitializers, request, lineDirectives: false);
                    visited = rewriter.Visit(trees[index].GetRoot())!;
                }
                var rewritten = visited.ToFullString();
                // The compiler reads the copy; errors, traces and caller information name the source.
                if (rewriter.LineDirectives) rewritten = $"#line 1 \"{trees[index].FilePath}\"\n" + rewritten;
                outputs.Add((request.Sources[index], rewritten));
            }
            foreach (var output in outputs)
            {
                Directory.CreateDirectory(System.IO.Path.GetDirectoryName(output.Source.OutputPath)!);
                File.WriteAllText(output.Source.OutputPath, output.Text);
            }
            stdout.WriteLine(JsonSerializer.Serialize(new { SchemaVersion = 2, Verified = true,
                RuntimeInitializers = runtimeInitializers, ScriptTypes = scriptTypes }));
            return 0;
        }
        catch (Exception error)
        {
            failure = error is CompilationErrors ? "diagnostics" : "error";
            stderr.WriteLine(error.Message);
            return 1;
        }
    }

    private static Dictionary<string, object> ReadSourceDeclarations(string path)
    {
        var requests = JsonSerializer.Deserialize<SourceDeclarationRequest[]>(File.ReadAllText(path),
            new JsonSerializerOptions { PropertyNameCaseInsensitive = true }) ?? throw new InvalidOperationException("Empty Player declaration request");
        var result = new Dictionary<string, object>(StringComparer.Ordinal);
        foreach (SourceDeclarationRequest request in requests)
        {
            Dictionary<string, string[]> Parse(string[] defines)
            {
                var parse = new CSharpParseOptions(LanguageVersion.Latest, preprocessorSymbols: defines);
                var sources = new Dictionary<string, string[]>(StringComparer.Ordinal);
                foreach (SourceInput source in request.Sources)
                {
                    SyntaxTree tree = CSharpSyntaxTree.ParseText(File.ReadAllText(source.Path), parse, source.OriginalPath);
                    var errors = tree.GetDiagnostics().Where(diagnostic => diagnostic.Severity == DiagnosticSeverity.Error).ToArray();
                    if (errors.Length != 0) throw new InvalidOperationException(string.Join("\n", errors.Select(error => error.ToString())));
                    string FullName(ClassDeclarationSyntax declaration)
                    {
                        string namespaces = string.Join(".", declaration.Ancestors().OfType<BaseNamespaceDeclarationSyntax>().Reverse().Select(value =>
                            string.Join(".", value.Name.DescendantTokens().Where(token => token.IsKind(SyntaxKind.IdentifierToken)).Select(token => token.ValueText))));
                        string types = string.Join("+", declaration.Ancestors().OfType<TypeDeclarationSyntax>().Reverse().Append(declaration).Select(value =>
                            value.Identifier.ValueText + ((value.TypeParameterList?.Parameters.Count ?? 0) == 0 ? "" : "`" + value.TypeParameterList!.Parameters.Count)));
                        return namespaces.Length == 0 ? types : namespaces + "." + types;
                    }
                    sources.Add(source.OriginalPath, tree.GetRoot().DescendantNodes().OfType<ClassDeclarationSyntax>().Select(FullName).Distinct(StringComparer.Ordinal).OrderBy(name => name, StringComparer.Ordinal).ToArray());
                }
                return sources;
            }
            result.Add(request.AssemblyName, new { player = Parse(request.Defines), editor = Parse(request.EditorDefines) });
        }
        return result;
    }

    /// Adds a public bridge to every `[RuntimeInitializeOnLoadMethod]` method, which the Player
    /// runtime calls at the method's load type: the Player loads the game's assemblies itself,
    /// after the engine ran the initializers a build would have listed. The bridges stand on the
    /// line of the class's closing brace, so every line of the source keeps its number.
    private sealed class InitializerRewriter : CSharpSyntaxRewriter
    {
        private readonly SemanticModel model;
        private readonly List<RuntimeInitializerInfo> runtimeInitializers;
        private readonly Request request;
        /// Whether the output names the lines of its source with `#line`. A source that sets
        /// its own lines, or whose path a directive cannot hold, is left to say what it says.
        private readonly bool lineDirectives;
        public bool LineDirectives => lineDirectives;

        /// `lineDirectives: false` writes a copy that names itself, whatever the source allows.
        public InitializerRewriter(SemanticModel model, List<RuntimeInitializerInfo> runtimeInitializers, Request request,
            bool lineDirectives = true)
        { this.model = model; this.runtimeInitializers = runtimeInitializers; this.request = request;
          string path = model.SyntaxTree.FilePath;
          // A directive ends at any line terminator of the language, also the three Unicode
          // ones. Only a `#line` the compiler obeys sets lines: one in a branch that is
          // switched off is text.
          this.lineDirectives = lineDirectives && path.Length != 0 && path.IndexOfAny(new[] { '"', '\n', '\r', '\u0085', '\u2028', '\u2029' }) < 0
              && !model.SyntaxTree.GetRoot().DescendantTrivia(descendIntoTrivia: true).Any(trivia =>
                  (trivia.IsKind(SyntaxKind.LineDirectiveTrivia) || trivia.IsKind(SyntaxKind.LineSpanDirectiveTrivia))
                  && trivia.GetStructure() is DirectiveTriviaSyntax { IsActive: true }); }

        private const string LineAnchor = "UnityMicroEditor.Line";

        /// Statements and members carry the line they start on in the source, so that
        /// `LinesKept` can compare it with the line they have after the rewrite.
        public override SyntaxNode? Visit(SyntaxNode? node)
        {
            var visited = base.Visit(node);
            if (!lineDirectives || visited is null || node is not (StatementSyntax or MemberDeclarationSyntax)
                || node is BlockSyntax or BaseTypeDeclarationSyntax or BaseNamespaceDeclarationSyntax) return visited;
            int line = node.SyntaxTree.GetLineSpan(new Microsoft.CodeAnalysis.Text.TextSpan(node.SpanStart, 0)).StartLinePosition.Line + 1;
            return visited.WithAdditionalAnnotations(new SyntaxAnnotation(LineAnchor, line.ToString(System.Globalization.CultureInfo.InvariantCulture)));
        }

        /// Whether every statement and member of the rewritten tree that came from the source
        /// stands on the line the source has it on, as the directives of the tree map it.
        public bool LinesKept(SyntaxNode rewritten)
        {
            var tree = CSharpSyntaxTree.Create((CSharpSyntaxNode)rewritten, (CSharpParseOptions)model.SyntaxTree.Options, model.SyntaxTree.FilePath);
            foreach (var node in tree.GetRoot().GetAnnotatedNodes(LineAnchor))
            {
                int mapped = tree.GetMappedLineSpan(new Microsoft.CodeAnalysis.Text.TextSpan(node.SpanStart, 0)).StartLinePosition.Line + 1;
                if (node.GetAnnotations(LineAnchor).First().Data != mapped.ToString(System.Globalization.CultureInfo.InvariantCulture)) return false;
            }
            return true;
        }

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

        public override SyntaxNode? VisitClassDeclaration(ClassDeclarationSyntax node)
        {
            var type = model.GetDeclaredSymbol(node)!;
            var visited = (ClassDeclarationSyntax)base.VisitClassDeclaration(node)!;
            var members = visited.Members.ToList();
            AddRuntimeInitializers(node, type, members);
            return Appended(visited, members, visited.Members.Count);
        }

        /// The class with the members the rewrite adds behind the authored ones. They stand on
        /// the line of the closing brace, so no line of the file moves. What stood in front of
        /// that brace (`#endregion`, `#endif`, a comment) stays in front of them: a directive
        /// needs a line of its own, and an added member must not fall into a conditional block.
        private static ClassDeclarationSyntax Appended(ClassDeclarationSyntax node, List<MemberDeclarationSyntax> members, int authored)
        {
            if (members.Count > authored)
            {
                members[authored] = members[authored].WithLeadingTrivia(node.CloseBraceToken.LeadingTrivia);
                node = node.WithCloseBraceToken(node.CloseBraceToken.WithLeadingTrivia(default(SyntaxTriviaList)));
            }
            return node.WithMembers(SyntaxFactory.List(members));
        }

        private static void Reject(SyntaxNode node, string message)
        {
            var location = node.GetLocation().GetLineSpan();
            throw new InvalidOperationException($"{location.Path}({location.StartLinePosition.Line + 1}): {message}");
        }
    }
}
