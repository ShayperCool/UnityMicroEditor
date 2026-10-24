// Links the code of a Player compiled as the Editor's Play mode compiles it
// (with UNITY_EDITOR and against the Editor's assemblies) into a Player.
//
// `link` takes one compiled assembly:
// - a call of an engine member that only the Editor's engine has
//   (CanvasRenderer.onRequestRebuild), and of one whose answer Play mode
//   gives differently (Application.isEditor), becomes a call of a static
//   stand-in of the same signature in UnityMicroEditor.PlayModeEngine; a
//   type only the Editor's engine has moves to that assembly;
// - a serialized field that only one kind of build declares is aligned with
//   the host's build of the same assembly, whose data the Player reads
//   without type trees (Resources, the managers' assets): a field only the
//   Editor declares is not serialized, one only the Player declares is added.
//
// `stand-ins` takes a Player's managed directory and writes the assemblies
// its code names that the Player lacks (UnityEditor.CoreModule and the like,
// and UnityMicroEditor.PlayModeEngine) with exactly the types and members it
// names. A member returns its type's default value, or what Play mode
// answers where the runtime bridge has it (UnityMicroEditor.PlayModeEditor).
//
// Usage: PlayModeLinker <request.json>; prints a report as JSON.
using System.Text.Json;
using Mono.Cecil;
using Mono.Cecil.Cil;

static class Program
{
    static int Main(string[] arguments)
    {
        if (arguments.Length != 1)
        {
            Console.Error.WriteLine("usage: PlayModeLinker <request.json>");
            return 2;
        }
        var options = new JsonSerializerOptions { PropertyNameCaseInsensitive = true };
        var request = JsonSerializer.Deserialize<Request>(File.ReadAllText(arguments[0]), options)
            ?? throw new InvalidDataException("empty request");
        var report = request.Mode switch
        {
            "link" => new AssemblyLink(request).Run(),
            "stand-ins" => new StandIns(request).Run(),
            _ => throw new InvalidDataException($"unknown mode {request.Mode}"),
        };
        Console.WriteLine(JsonSerializer.Serialize(report, new JsonSerializerOptions
        {
            PropertyNamingPolicy = JsonNamingPolicy.CamelCase,
            WriteIndented = true,
        }));
        return 0;
    }
}

sealed class Request
{
    public string Mode { get; set; } = "";
    /// <summary>The Player's own engine modules, as the installation ships them.</summary>
    public string Engine { get; set; } = "";
    /// <summary>The managed directory of the Player the code runs in.</summary>
    public string Managed { get; set; } = "";
    // link
    public string Input { get; set; } = "";
    public string Output { get; set; } = "";
    /// <summary>The host's build of the same assembly, whose serialized layout the Player's data has; empty if none.</summary>
    public string Layout { get; set; } = "";
    // stand-ins
    /// <summary>Where the Editor's own assemblies lie.</summary>
    public List<string> EditorDirectories { get; set; } = new();
    /// <summary>The assembly whose <c>UnityMicroEditor.PlayModeEditor</c> types answer for the Editor.</summary>
    public string Support { get; set; } = "";
}

sealed class Report
{
    public int SchemaVersion { get; set; } = 1;
    public List<string> Shims { get; set; } = new();
    public int EditorTypes { get; set; }
    public int EditorMembers { get; set; }
    public int SupportedMembers { get; set; }
    public List<string> EngineStubs { get; set; } = new();
    public List<string> MovedEngineTypes { get; set; } = new();
    public List<string> NotSerialized { get; set; } = new();
    public List<string> AddedFields { get; set; } = new();
    public List<string> LayoutDifferences { get; set; } = new();
    public List<string> Unsupported { get; set; } = new();
    public List<string> Unresolved { get; set; } = new();
}

static class Names
{
    public const string EngineShim = "UnityMicroEditor.PlayModeEngine";
    public const string EngineNamespace = "UnityMicroEditor.PlayMode.Engine";
    public const string SupportNamespace = "UnityMicroEditor.PlayModeEditor";
    /// <summary>The type every stand-in assembly holds, by which a later run knows it for one.</summary>
    public const string Marker = "UnityMicroEditor.PlayMode.StandIn";

    public static string ScopeName(TypeReference type)
    {
        while (type.DeclaringType != null) type = type.DeclaringType;
        return type.Scope switch
        {
            AssemblyNameReference name => name.Name,
            ModuleDefinition module => module.Assembly.Name.Name,
            _ => type.Scope?.Name ?? "",
        };
    }

    /// <summary>The engine type a moved type or a stand-in holder is named after, flattened.</summary>
    public static string Flat(string fullName)
    {
        var parts = fullName.Split('/');
        if (parts.Length == 1) return fullName;
        var outer = parts[0];
        var dot = outer.LastIndexOf('.');
        var ns = dot >= 0 ? outer[..dot] + "." : "";
        return ns + string.Join("_", parts.Select(part => part[(part.LastIndexOf('.') + 1)..]));
    }

    public static string Holder(string fullName) => fullName.Replace('.', '_').Replace('/', '_').Replace('`', '_');

    public static DefaultAssemblyResolver Resolver(params string[] directories)
    {
        var resolver = new DefaultAssemblyResolver();
        foreach (var directory in resolver.GetSearchDirectories()) resolver.RemoveSearchDirectory(directory);
        foreach (var directory in directories.Where(directory => directory.Length > 0)) resolver.AddSearchDirectory(directory);
        return resolver;
    }

    public static TypeDefinition? Lookup(IAssemblyResolver resolver, string scope, string fullName, out string definedIn)
    {
        definedIn = scope;
        AssemblyDefinition? assembly;
        try { assembly = resolver.Resolve(new AssemblyNameReference(scope, null)); }
        catch (AssemblyResolutionException) { return null; }
        if (assembly == null) return null;
        var found = assembly.MainModule.GetType(fullName);
        if (found != null) return found;
        var outer = fullName.Split('/')[0];
        foreach (var exported in assembly.MainModule.ExportedTypes)
        {
            if (!exported.IsForwarder || exported.FullName != outer) continue;
            return Lookup(resolver, exported.Scope.Name, fullName, out definedIn);
        }
        return null;
    }

    public static IEnumerable<MethodDefinition> Bodies(ModuleDefinition module)
    {
        foreach (var type in module.GetTypes())
            foreach (var method in type.Methods)
                if (method.HasBody) yield return method;
    }

    public static ModuleDefinition Read(string path, IAssemblyResolver resolver, out bool symbols)
    {
        var pdb = System.IO.Path.ChangeExtension(path, ".pdb");
        symbols = File.Exists(pdb);
        try
        {
            return ModuleDefinition.ReadModule(path, new ReaderParameters { AssemblyResolver = resolver, InMemory = true, ReadSymbols = symbols });
        }
        catch (Exception error) when (error is SymbolsNotMatchingException or SymbolsNotFoundException)
        {
            symbols = false;
            return ModuleDefinition.ReadModule(path, new ReaderParameters { AssemblyResolver = resolver, InMemory = true });
        }
    }

    public static void Write(ModuleDefinition module, string path, bool symbols)
    {
        var temporary = path + ".linking";
        if (symbols)
        {
            module.Write(temporary, new WriterParameters { WriteSymbols = true });
            File.Move(System.IO.Path.ChangeExtension(temporary, ".pdb"), System.IO.Path.ChangeExtension(path, ".pdb"), true);
        }
        else
        {
            module.Write(temporary);
        }
        File.Move(temporary, path, true);
    }
}

/// <summary>One compiled assembly, linked for the Player.</summary>
sealed class AssemblyLink
{
    /// <summary>What Play mode answers for members the Player has as well, by holder and name.</summary>
    public static readonly HashSet<string> Facts = new(StringComparer.Ordinal)
    {
        "System.Boolean UnityEngine.Application::get_isEditor()",
        "UnityEngine.RuntimePlatform UnityEngine.Application::get_platform()",
        "System.Void UnityEngine.Application::Quit()",
        "System.Void UnityEngine.Application::Quit(System.Int32)",
        // The legacy Input, which also gives what a remote viewer or an agent sends.
        "UnityEngine.Vector3 UnityEngine.Input::get_mousePosition()",
        "UnityEngine.Vector2 UnityEngine.Input::get_mouseScrollDelta()",
        "System.Boolean UnityEngine.Input::GetMouseButton(System.Int32)",
        "System.Boolean UnityEngine.Input::GetMouseButtonDown(System.Int32)",
        "System.Boolean UnityEngine.Input::GetMouseButtonUp(System.Int32)",
        "System.Boolean UnityEngine.Input::GetKey(UnityEngine.KeyCode)",
        "System.Boolean UnityEngine.Input::GetKeyDown(UnityEngine.KeyCode)",
        "System.Boolean UnityEngine.Input::GetKeyUp(UnityEngine.KeyCode)",
        "System.Boolean UnityEngine.Input::GetKey(System.String)",
        "System.Boolean UnityEngine.Input::GetKeyDown(System.String)",
        "System.Boolean UnityEngine.Input::GetKeyUp(System.String)",
        "System.Boolean UnityEngine.Input::get_anyKey()",
        "System.Boolean UnityEngine.Input::get_anyKeyDown()",
    };

    readonly Request request;
    readonly Report report = new();
    readonly DefaultAssemblyResolver resolver;
    readonly DefaultAssemblyResolver engine;
    readonly HashSet<string> engineNames = new(StringComparer.Ordinal);

    public AssemblyLink(Request request)
    {
        this.request = request;
        // The engine's own modules come first: the managed directory may
        // hold a game assembly whose name starts as an engine module's does.
        // The Editor's own assemblies come before the managed directory's
        // stand-ins of them: writing the assembly reads the constants of
        // Editor enums. A plugin's own directory comes last, for its
        // siblings'.
        resolver = Names.Resolver(new[] { request.Engine }.Concat(request.EditorDirectories)
            .Append(request.Managed).Append(System.IO.Path.GetDirectoryName(request.Input) ?? "").ToArray());
        engine = Names.Resolver(request.Engine);
        foreach (var file in Directory.GetFiles(request.Engine, "UnityEngine*.dll"))
            engineNames.Add(System.IO.Path.GetFileNameWithoutExtension(file));
    }

    public Report Run()
    {
        var module = Names.Read(request.Input, resolver, out var symbols);
        var name = module.Assembly.Name.Name;
        var moved = MoveEngineTypes(module);
        RedirectEngineMembers(module, moved);
        if (request.Layout.Length > 0 && File.Exists(request.Layout)) Align(module, request.Layout);
        Names.Write(module, request.Output, symbols);
        report.Shims.Add(name);
        return report;
    }

    AssemblyNameReference EngineShim(ModuleDefinition module)
    {
        var reference = module.AssemblyReferences.FirstOrDefault(candidate => candidate.Name == Names.EngineShim);
        if (reference != null) return reference;
        reference = new AssemblyNameReference(Names.EngineShim, new Version(0, 0, 0, 0));
        module.AssemblyReferences.Add(reference);
        return reference;
    }

    /// <summary>The engine types the code names that only the Editor's engine has move to the engine stand-in.</summary>
    HashSet<TypeReference> MoveEngineTypes(ModuleDefinition module)
    {
        var moved = new HashSet<TypeReference>();
        foreach (var type in module.GetTypeReferences().ToList())
        {
            var scope = Names.ScopeName(type);
            if (!engineNames.Contains(scope) || Names.Lookup(engine, scope, type.FullName, out _) != null) continue;
            var flat = Names.Flat(type.FullName);
            report.MovedEngineTypes.Add($"{scope}: {type.FullName}");
            var dot = flat.LastIndexOf('.');
            type.DeclaringType = null;
            type.Namespace = dot >= 0 ? flat[..dot] : "";
            type.Name = dot >= 0 ? flat[(dot + 1)..] : flat;
            type.Scope = EngineShim(module);
            moved.Add(type);
        }
        return moved;
    }

    void RedirectEngineMembers(ModuleDefinition module, HashSet<TypeReference> moved)
    {
        var replacements = new Dictionary<string, MemberReference>(StringComparer.Ordinal);
        foreach (var member in module.GetMemberReferences())
        {
            // The methods of an array type are the runtime's own.
            if (member.DeclaringType is ArrayType) continue;
            var element = member.DeclaringType.GetElementType();
            var scope = Names.ScopeName(element);
            if (!engineNames.Contains(scope) || moved.Contains(element)) continue;
            if (!Facts.Contains(member.FullName))
            {
                IMemberDefinition? resolved = null;
                try { resolved = member.Resolve(); } catch (AssemblyResolutionException) { }
                if (resolved != null) continue;
            }
            if (member.DeclaringType is GenericInstanceType)
            {
                report.Unsupported.Add($"{member.FullName} (a member of a generic engine type)");
                continue;
            }
            var holder = new TypeReference(Names.EngineNamespace, Names.Holder(element.FullName), module, EngineShim(module));
            switch (member)
            {
                case MethodReference method:
                    var stub = new MethodReference(method.Name, method.ReturnType, holder) { HasThis = false };
                    foreach (var parameter in method.GenericParameters) stub.GenericParameters.Add(new GenericParameter(parameter.Name, stub));
                    if (method.HasThis)
                    {
                        TypeReference self = method.DeclaringType;
                        if (self.IsValueType) self = new ByReferenceType(self);
                        stub.Parameters.Add(new ParameterDefinition("self", ParameterAttributes.None, self));
                    }
                    foreach (var parameter in method.Parameters)
                        stub.Parameters.Add(new ParameterDefinition(parameter.Name, parameter.Attributes, parameter.ParameterType));
                    stub.ReturnType = Relocate(method.ReturnType, method, stub);
                    foreach (var parameter in stub.Parameters) parameter.ParameterType = Relocate(parameter.ParameterType, method, stub);
                    replacements[method.FullName] = stub;
                    report.EngineStubs.Add(method.FullName);
                    break;
                case FieldReference field:
                    replacements[field.FullName] = new FieldReference(field.Name, field.FieldType, holder);
                    report.EngineStubs.Add(field.FullName);
                    break;
            }
        }
        if (replacements.Count == 0) return;
        foreach (var method in Names.Bodies(module))
        {
            var il = method.Body.GetILProcessor();
            foreach (var instruction in method.Body.Instructions.ToList())
            {
                switch (instruction.Operand)
                {
                    case MethodReference called:
                        var element = called is GenericInstanceMethod generic ? generic.ElementMethod : called;
                        if (!replacements.TryGetValue(element.FullName, out var stub)) break;
                        if (instruction.OpCode == OpCodes.Ldftn || instruction.OpCode == OpCodes.Ldvirtftn || instruction.OpCode == OpCodes.Newobj)
                        {
                            report.Unsupported.Add($"{method.FullName}: {instruction.OpCode} {element.FullName}");
                            break;
                        }
                        MethodReference target = (MethodReference)stub;
                        if (called is GenericInstanceMethod instance)
                        {
                            var specific = new GenericInstanceMethod(target);
                            foreach (var argument in instance.GenericArguments) specific.GenericArguments.Add(argument);
                            target = specific;
                        }
                        if (instruction.Previous?.OpCode == OpCodes.Constrained) instruction.Previous.OpCode = OpCodes.Nop;
                        instruction.OpCode = OpCodes.Call;
                        instruction.Operand = target;
                        break;
                    case FieldReference field:
                        if (!replacements.TryGetValue(field.FullName, out var holder)) break;
                        var storage = (FieldReference)holder;
                        if (instruction.OpCode == OpCodes.Ldsfld || instruction.OpCode == OpCodes.Stsfld || instruction.OpCode == OpCodes.Ldsflda)
                        {
                            instruction.Operand = storage;
                        }
                        else if (instruction.OpCode == OpCodes.Ldfld || instruction.OpCode == OpCodes.Ldflda)
                        {
                            // Every instance shares one value. The instruction itself
                            // becomes the first one, so that branches to it hold.
                            var load = il.Create(instruction.OpCode == OpCodes.Ldfld ? OpCodes.Ldsfld : OpCodes.Ldsflda, storage);
                            instruction.OpCode = OpCodes.Pop;
                            instruction.Operand = null;
                            il.InsertAfter(instruction, load);
                        }
                        else if (instruction.OpCode == OpCodes.Stfld)
                        {
                            var value = new VariableDefinition(storage.FieldType);
                            method.Body.Variables.Add(value);
                            var drop = il.Create(OpCodes.Pop);
                            var reload = il.Create(OpCodes.Ldloc, value);
                            var store = il.Create(OpCodes.Stsfld, storage);
                            instruction.OpCode = OpCodes.Stloc;
                            instruction.Operand = value;
                            il.InsertAfter(instruction, drop);
                            il.InsertAfter(drop, reload);
                            il.InsertAfter(reload, store);
                        }
                        break;
                }
            }
        }
    }

    /// <summary>A method's own generic parameters, as the stand-in method's.</summary>
    static TypeReference Relocate(TypeReference type, MethodReference from, MethodReference to) => type switch
    {
        GenericParameter { Type: GenericParameterType.Method } parameter when parameter.Position < to.GenericParameters.Count
            => to.GenericParameters[parameter.Position],
        ByReferenceType reference => new ByReferenceType(Relocate(reference.ElementType, from, to)),
        ArrayType array => new ArrayType(Relocate(array.ElementType, from, to), array.Rank),
        _ => type,
    };

    // ----- serialized layout -----------------------------------------------------

    void Align(ModuleDefinition module, string layoutPath)
    {
        ModuleDefinition layout;
        try { layout = ModuleDefinition.ReadModule(layoutPath, new ReaderParameters { AssemblyResolver = resolver, InMemory = true }); }
        catch (BadImageFormatException) { return; }
        var types = layout.GetTypes().ToDictionary(type => type.FullName, StringComparer.Ordinal);
        foreach (var type in module.GetTypes())
        {
            if (!types.TryGetValue(type.FullName, out var counterpart) || !UnitySerializable(type)) continue;
            var ours = type.Fields.Where(Serialized).ToList();
            var theirs = counterpart.Fields.Where(Serialized).ToList();
            var theirNames = new HashSet<string>(theirs.Select(field => field.Name), StringComparer.Ordinal);
            var ourNames = new HashSet<string>(ours.Select(field => field.Name), StringComparer.Ordinal);
            foreach (var field in ours.Where(field => !theirNames.Contains(field.Name)))
            {
                field.IsNotSerialized = true;
                report.NotSerialized.Add(type.FullName + "::" + field.Name);
            }
            FieldDefinition? previous = null;
            foreach (var field in theirs)
            {
                if (ourNames.Contains(field.Name))
                {
                    previous = ours.First(candidate => candidate.Name == field.Name);
                    continue;
                }
                var added = new FieldDefinition(field.Name, field.Attributes, ImportLayoutType(field.FieldType, module, layout, type));
                foreach (var attribute in field.CustomAttributes)
                {
                    if (!attribute.AttributeType.Namespace.StartsWith("UnityEngine", StringComparison.Ordinal)) continue;
                    added.CustomAttributes.Add(new CustomAttribute(module.ImportReference(attribute.Constructor), attribute.GetBlob()));
                }
                type.Fields.Insert(previous == null ? 0 : type.Fields.IndexOf(previous) + 1, added);
                previous = added;
                report.AddedFields.Add(type.FullName + "::" + field.Name);
            }
            if (!type.Fields.Where(Serialized).Select(field => field.Name).SequenceEqual(theirs.Select(field => field.Name)))
                report.LayoutDifferences.Add($"{type.FullName} orders its serialized fields differently");
            if (type.BaseType?.FullName != counterpart.BaseType?.FullName)
                report.LayoutDifferences.Add($"{type.FullName} derives from {type.BaseType?.FullName} instead of {counterpart.BaseType?.FullName}");
        }
    }

    /// <summary>A type whose fields Unity serializes: an engine object's script, or one marked serializable.</summary>
    static bool UnitySerializable(TypeDefinition type)
    {
        if (type.Name.Contains('<') || type.IsInterface || type.IsEnum) return false;
        if (type.IsSerializable) return true;
        for (var current = type.BaseType; current != null;)
        {
            if (current.FullName is "UnityEngine.Object" or "UnityEngine.MonoBehaviour" or "UnityEngine.ScriptableObject") return true;
            TypeDefinition? resolved = null;
            try { resolved = current.Resolve(); } catch (AssemblyResolutionException) { }
            current = resolved?.BaseType;
        }
        return false;
    }

    /// <summary>A field Unity serializes: an instance field neither constant nor read-only, public or marked to be serialized.</summary>
    static bool Serialized(FieldDefinition field) =>
        !field.IsStatic && !field.IsLiteral && !field.IsInitOnly && !field.IsNotSerialized
        && (field.IsPublic || field.CustomAttributes.Any(attribute =>
            attribute.AttributeType.FullName is "UnityEngine.SerializeField" or "UnityEngine.SerializeReference"));

    static TypeReference ImportLayoutType(TypeReference type, ModuleDefinition module, ModuleDefinition layout, TypeDefinition owner)
    {
        switch (type)
        {
            case GenericParameter parameter when parameter.Type == GenericParameterType.Type && parameter.Position < owner.GenericParameters.Count:
                return owner.GenericParameters[parameter.Position];
            case TypeDefinition own when own.Module == layout && module.GetType(own.FullName) is { } match:
                return match;
            case GenericInstanceType instance:
                var generic = new GenericInstanceType(ImportLayoutType(instance.ElementType, module, layout, owner));
                foreach (var argument in instance.GenericArguments) generic.GenericArguments.Add(ImportLayoutType(argument, module, layout, owner));
                return generic;
            case ArrayType array:
                return new ArrayType(ImportLayoutType(array.ElementType, module, layout, owner), array.Rank);
        }
        return module.ImportReference(type, owner);
    }
}

/// <summary>The assemblies a Player's code names that the Player lacks, written as stand-ins.</summary>
sealed class StandIns
{
    readonly Request request;
    readonly Report report = new();
    readonly DefaultAssemblyResolver player;
    readonly DefaultAssemblyResolver editor;
    readonly HashSet<string> playerNames = new(StringComparer.Ordinal);
    readonly HashSet<string> engineNames = new(StringComparer.Ordinal);
    readonly List<ModuleDefinition> code = new();
    readonly Dictionary<string, AssemblyDefinition> shims = new(StringComparer.Ordinal);
    // "assembly|full name" of a type as the code names it, and as the Editor defines it.
    readonly Dictionary<string, TypeDefinition> generated = new(StringComparer.Ordinal);
    readonly Dictionary<TypeDefinition, TypeDefinition?> definitions = new();
    readonly HashSet<string> generatedMembers = new(StringComparer.Ordinal);
    Dictionary<string, (string scope, string fullName)>? flatEngineTypes;
    ModuleDefinition? support;
    TypeDefinition? refHolder;

    public StandIns(Request request)
    {
        this.request = request;
        player = Names.Resolver(request.Managed);
        editor = Names.Resolver(request.EditorDirectories.ToArray());
        foreach (var file in Directory.GetFiles(request.Engine, "UnityEngine*.dll"))
            engineNames.Add(System.IO.Path.GetFileNameWithoutExtension(file));
        foreach (var file in Directory.GetFiles(request.Managed, "*.dll"))
        {
            var name = System.IO.Path.GetFileNameWithoutExtension(file);
            try
            {
                using var module = ModuleDefinition.ReadModule(file, new ReaderParameters { ReadingMode = ReadingMode.Deferred });
                if (module.GetType(Names.Marker) != null) continue;
            }
            catch (BadImageFormatException)
            {
            }
            playerNames.Add(name);
        }
    }

    public Report Run()
    {
        foreach (var name in playerNames.OrderBy(name => name, StringComparer.Ordinal))
        {
            if (engineNames.Contains(name)) continue;
            var path = System.IO.Path.Combine(request.Managed, name + ".dll");
            var module = ModuleDefinition.ReadModule(path, new ReaderParameters { AssemblyResolver = player, ReadingMode = ReadingMode.Deferred });
            if (module.AssemblyReferences.Any(reference => IsStandIn(reference.Name))) code.Add(module);
            else module.Dispose();
        }
        var supportPath = System.IO.Path.Combine(request.Managed, request.Support + ".dll");
        if (request.Support.Length > 0 && File.Exists(supportPath))
            support = ModuleDefinition.ReadModule(supportPath, new ReaderParameters { AssemblyResolver = player });
        foreach (var module in code) Collect(module);
        AddAnswered();
        ImplementInterfaces();
        Write();
        Verify();
        report.Shims.Sort(StringComparer.Ordinal);
        return report;
    }

    /// <summary>An assembly the Player lacks that the Editor has, or the engine stand-in.</summary>
    bool IsStandIn(string name) =>
        name == Names.EngineShim || shims.ContainsKey(name) || !playerNames.Contains(name) && EditorHas(name);

    bool EditorHas(string name) =>
        request.EditorDirectories.Any(directory => File.Exists(System.IO.Path.Combine(directory, name + ".dll")));

    // ----- stand-in assemblies ------------------------------------------------

    AssemblyDefinition Shim(string name)
    {
        if (shims.TryGetValue(name, out var existing)) return existing;
        var version = new Version(0, 0, 0, 0);
        var original = Names.Lookup(editor, name, "<Module>", out _)?.Module.Assembly;
        if (original != null && name != Names.EngineShim) version = original.Name.Version;
        var shim = AssemblyDefinition.CreateAssembly(
            new AssemblyNameDefinition(name, version),
            name + ".dll",
            new ModuleParameters { Kind = ModuleKind.Dll, AssemblyResolver = player, Runtime = TargetRuntime.Net_4_0 });
        var dot = Names.Marker.LastIndexOf('.');
        shim.MainModule.Types.Add(new TypeDefinition(Names.Marker[..dot], Names.Marker[(dot + 1)..],
            TypeAttributes.NotPublic | TypeAttributes.Abstract | TypeAttributes.Sealed, shim.MainModule.TypeSystem.Object));
        shims[name] = shim;
        report.Shims.Add(name);
        return shim;
    }

    /// <summary>The Editor's definition of a type the code names in a stand-in assembly.</summary>
    TypeDefinition? EditorDefinition(string scope, string fullName, out string definedIn)
    {
        if (scope != Names.EngineShim) return Names.Lookup(editor, scope, fullName, out definedIn);
        definedIn = Names.EngineShim;
        if (fullName.StartsWith(Names.EngineNamespace + ".", StringComparison.Ordinal)) return null;
        flatEngineTypes ??= FlatEngineTypes();
        return flatEngineTypes.TryGetValue(fullName, out var original)
            ? Names.Lookup(editor, original.scope, original.fullName, out _)
            : null;
    }

    /// <summary>The Editor's engine types by the flattened name a moved type has.</summary>
    Dictionary<string, (string, string)> FlatEngineTypes()
    {
        var index = new Dictionary<string, (string, string)>(StringComparer.Ordinal);
        foreach (var name in engineNames)
        {
            AssemblyDefinition? assembly;
            try { assembly = editor.Resolve(new AssemblyNameReference(name, null)); }
            catch (AssemblyResolutionException) { continue; }
            foreach (var type in assembly.MainModule.GetTypes())
                index.TryAdd(Names.Flat(type.FullName), (name, type.FullName));
        }
        return index;
    }

    /// <summary>The stand-in of a type the code names in a stand-in assembly.</summary>
    TypeDefinition Ensure(string scope, string fullName)
    {
        if (generated.TryGetValue(scope + "|" + fullName, out var known)) return known;
        var definition = EditorDefinition(scope, fullName, out var definedIn);
        if (definition != null && generated.TryGetValue(definedIn + "|" + definition.FullName, out known))
        {
            generated[scope + "|" + fullName] = known;
            AddForwarder(scope, definedIn, known);
            return known;
        }
        var shim = Shim(definedIn).MainModule;
        var names = fullName.Split('/');
        string ns = "", name = names[^1];
        if (names.Length == 1)
        {
            var dot = name.LastIndexOf('.');
            if (dot >= 0) (ns, name) = (name[..dot], name[(dot + 1)..]);
        }
        var declaring = names.Length > 1 ? Ensure(scope, string.Join("/", names[..^1])) : null;
        var holder = scope == Names.EngineShim && definition == null;
        var attributes = holder
            ? TypeAttributes.Class | TypeAttributes.Abstract | TypeAttributes.Sealed | TypeAttributes.BeforeFieldInit
            : definition?.Attributes ?? TypeAttributes.Class;
        attributes &= ~(TypeAttributes.VisibilityMask | TypeAttributes.Import | TypeAttributes.HasSecurity);
        attributes |= declaring != null ? TypeAttributes.NestedPublic : TypeAttributes.Public;
        var type = new TypeDefinition(ns, name, attributes);
        if (declaring != null) declaring.NestedTypes.Add(type); else shim.Types.Add(type);
        generated[scope + "|" + fullName] = type;
        definitions[type] = definition;
        if (definition != null) generated[definedIn + "|" + definition.FullName] = type;
        else if (!holder) report.Unresolved.Add($"{scope}: {fullName} (the Editor has no such type)");
        report.EditorTypes++;
        AddForwarder(scope, definedIn, type);
        if (definition != null)
            foreach (var parameter in definition.GenericParameters)
                type.GenericParameters.Add(new GenericParameter(parameter.Name, type) { Attributes = parameter.Attributes });
        if (holder) type.BaseType = shim.TypeSystem.Object;
        else Shape(type, definition, shim);
        return type;
    }

    void AddForwarder(string scope, string definedIn, TypeDefinition type)
    {
        if (scope == definedIn || type.DeclaringType != null) return;
        var from = Shim(scope).MainModule;
        if (from.ExportedTypes.Any(exported => exported.FullName == type.FullName)) return;
        var to = from.AssemblyReferences.FirstOrDefault(reference => reference.Name == definedIn);
        if (to == null)
        {
            to = new AssemblyNameReference(definedIn, Shim(definedIn).Name.Version);
            from.AssemblyReferences.Add(to);
        }
        from.ExportedTypes.Add(new ExportedType(type.Namespace, type.Name, from, to) { Attributes = TypeAttributes.Forwarder });
    }

    /// <summary>A type of the Player's class library.</summary>
    TypeReference Library(ModuleDefinition module, string fullName) =>
        module.ImportReference(Names.Lookup(player, "mscorlib", fullName, out _)
            ?? throw new InvalidOperationException($"the Player's class library has no {fullName}"));

    void Shape(TypeDefinition type, TypeDefinition? definition, ModuleDefinition module)
    {
        if (definition != null && definition.IsEnum)
        {
            type.BaseType = Library(module, "System.Enum");
            foreach (var field in definition.Fields)
            {
                var copy = new FieldDefinition(field.Name, field.Attributes, field.IsStatic ? type : Map(field.FieldType, module, type, null));
                if (field.HasConstant) copy.Constant = field.Constant;
                type.Fields.Add(copy);
            }
            return;
        }
        if (definition != null && definition.IsInterface)
        {
            MapInterfaces(type, definition, module);
            return;
        }
        if (definition?.BaseType?.FullName == "System.MulticastDelegate")
        {
            type.BaseType = Library(module, "System.MulticastDelegate");
            foreach (var method in definition.Methods)
            {
                var copy = new MethodDefinition(method.Name, method.Attributes, module.TypeSystem.Void) { ImplAttributes = method.ImplAttributes, HasThis = method.HasThis };
                copy.ReturnType = Map(method.ReturnType, module, type, copy);
                foreach (var parameter in method.Parameters)
                    copy.Parameters.Add(new ParameterDefinition(parameter.Name, parameter.Attributes, Map(parameter.ParameterType, module, type, copy)));
                type.Methods.Add(copy);
                generatedMembers.Add(MemberKey(type, copy));
            }
            return;
        }
        if (definition != null && definition.IsValueType)
        {
            type.BaseType = Library(module, "System.ValueType");
            type.ClassSize = definition.ClassSize;
            type.PackingSize = definition.PackingSize;
            foreach (var field in definition.Fields.Where(field => !field.IsStatic))
            {
                if (!Available(field.FieldType)) continue;
                type.Fields.Add(new FieldDefinition(field.Name, field.Attributes & ~FieldAttributes.FieldAccessMask | FieldAttributes.Public, Map(field.FieldType, module, type, null)));
            }
            MapInterfaces(type, definition, module);
            return;
        }
        type.BaseType = definition?.BaseType == null || !Available(definition.BaseType)
            ? module.TypeSystem.Object
            : Map(definition.BaseType, module, type, null);
        // A stand-in implements no abstract member of a Player class it derives
        // from (UIElements' BaseField<T>), which the runtime refuses of a class
        // that can be made; nothing makes an Editor control in a Player.
        if (AbstractPlayerBase(type.BaseType)) type.IsAbstract = true;
        if (definition != null) MapInterfaces(type, definition, module);
        var constructor = new MethodDefinition(".ctor",
            MethodAttributes.Public | MethodAttributes.HideBySig | MethodAttributes.SpecialName | MethodAttributes.RTSpecialName,
            module.TypeSystem.Void);
        EmitConstructor(constructor, type, module);
        type.Methods.Add(constructor);
        generatedMembers.Add(MemberKey(type, constructor));
    }

    /// <summary>Whether a class of the Player in the chain of bases is abstract.</summary>
    bool AbstractPlayerBase(TypeReference? baseType)
    {
        for (var current = baseType; current != null;)
        {
            var element = current.GetElementType();
            var scope = Names.ScopeName(element);
            if (IsStandIn(scope))
            {
                current = generated.TryGetValue(scope + "|" + element.FullName, out var stand) ? stand.BaseType : null;
                continue;
            }
            var resolved = Names.Lookup(player, scope, element.FullName, out _);
            if (resolved == null) return false;
            if (resolved.IsAbstract && !resolved.IsSealed) return true;
            current = resolved.BaseType;
        }
        return false;
    }

    /// <summary>Whether a type exists for the Player: its own, or one a stand-in holds.</summary>
    bool Available(TypeReference type)
    {
        var element = type.GetElementType();
        if (element is GenericParameter) return true;
        var scope = Names.ScopeName(element);
        if (IsStandIn(scope)) return true;
        if (playerNames.Contains(scope)) return Names.Lookup(player, scope, element.FullName, out _) != null;
        return Names.Lookup(player, "mscorlib", element.FullName, out _) != null;
    }

    /// <summary>
    /// The interfaces of the Editor's own a stand-in implements; a stand-in holds only the
    /// members the code names, so one of the class library's or the engine's (IComparable&lt;T&gt;)
    /// would be left unimplemented, and the runtime refuses such a type.
    /// </summary>
    void MapInterfaces(TypeDefinition type, TypeDefinition definition, ModuleDefinition module)
    {
        foreach (var implementation in definition.Interfaces)
        {
            var interfaceType = implementation.InterfaceType;
            if (interfaceType is GenericInstanceType generic)
            {
                // One of the class library's over types the Player has (IEnumerable<PackageInfo>):
                // code may call it, and a type that names it implements all of it.
                if (!LibraryInterface(generic.ElementType) || !generic.GenericArguments.All(Available)) continue;
            }
            else if (!IsStandIn(Names.ScopeName(interfaceType)) && !LibraryInterface(interfaceType)) continue;
            type.Interfaces.Add(new InterfaceImplementation(Map(interfaceType, module, type, null)));
        }
    }

    /// <summary>An interface of the Player's class library whose members a stand-in can implement (none generic).</summary>
    bool LibraryInterface(TypeReference type)
    {
        var scope = Names.ScopeName(type);
        // The Editor names them through netstandard or mscorlib; the Player has them in mscorlib.
        return !IsStandIn(scope)
            && Names.Lookup(player, "mscorlib", type.FullName, out _) is { IsInterface: true } contract
            && !contract.Methods.Any(method => method.HasGenericParameters);
    }

    /// <summary>A type of an interface's member as the instance <paramref name="context"/> of it has it.</summary>
    static TypeReference Substitute(TypeReference type, GenericInstanceType? context)
    {
        if (context == null) return type;
        switch (type)
        {
            case GenericParameter { Type: GenericParameterType.Type } parameter when parameter.Position < context.GenericArguments.Count:
                return context.GenericArguments[parameter.Position];
            case GenericInstanceType instance:
                var copy = new GenericInstanceType(instance.ElementType);
                foreach (var argument in instance.GenericArguments) copy.GenericArguments.Add(Substitute(argument, context));
                return copy;
            case ArrayType array:
                return new ArrayType(Substitute(array.ElementType, context), array.Rank);
            case ByReferenceType reference:
                return new ByReferenceType(Substitute(reference.ElementType, context));
            default:
                return type;
        }
    }

    /// <summary>
    /// Gives every stand-in class the members of the interfaces it implements that it lacks: of a
    /// stand-in interface, and of one of the class library's with the interfaces it extends, each
    /// answered by the runtime bridge where it has a method of the member's name (a collection's
    /// GetEnumerator), else with its type's default.
    /// </summary>
    void ImplementInterfaces()
    {
        foreach (var type in generated.Values.Distinct().ToList())
        {
            if (type.IsInterface) continue;
            ImplementLibraryInterfaces(type);
            foreach (var implementation in type.Interfaces.ToList())
            {
                var reference = implementation.InterfaceType;
                var contract = reference as TypeDefinition
                    ?? generated.Values.FirstOrDefault(candidate => candidate.FullName == reference.FullName && candidate.Module.Assembly.Name.Name == Names.ScopeName(reference));
                if (contract == null) continue;
                foreach (var required in contract.Methods.Where(method => method.IsAbstract))
                {
                    if (type.Methods.Any(method => method.Name == required.Name && method.Parameters.Count == required.Parameters.Count)) continue;
                    var module = type.Module;
                    var implementationMethod = new MethodDefinition(contract.FullName + "." + required.Name,
                        MethodAttributes.Private | MethodAttributes.Virtual | MethodAttributes.Final | MethodAttributes.NewSlot | MethodAttributes.HideBySig,
                        Import(required.ReturnType, module));
                    foreach (var parameter in required.Parameters)
                        implementationMethod.Parameters.Add(new ParameterDefinition(parameter.Name, parameter.Attributes, Import(parameter.ParameterType, module)));
                    implementationMethod.Overrides.Add(required.Module == module ? required : module.ImportReference(required));
                    EmitDefault(implementationMethod, implementationMethod.Body.GetILProcessor(), module);
                    type.Methods.Add(implementationMethod);
                }
            }
        }
    }

    void ImplementLibraryInterfaces(TypeDefinition type)
    {
        var module = type.Module;
        var pending = new Queue<TypeReference>(type.Interfaces.Select(implementation => implementation.InterfaceType)
            .Where(reference => LibraryInterface(reference.GetElementType())));
        var done = new HashSet<string>(StringComparer.Ordinal);
        while (pending.Count != 0)
        {
            var reference = pending.Dequeue();
            if (!done.Add(reference.FullName)) continue;
            var contract = Names.Lookup(player, "mscorlib", reference.GetElementType().FullName, out _);
            if (contract == null) continue;
            var context = reference as GenericInstanceType;
            if (!type.Interfaces.Any(implementation => implementation.InterfaceType.FullName == reference.FullName))
                type.Interfaces.Add(new InterfaceImplementation(Import(reference, module)));
            foreach (var inherited in contract.Interfaces)
                pending.Enqueue(Substitute(inherited.InterfaceType, context));
            foreach (var required in contract.Methods.Where(method => method.IsAbstract && !method.IsStatic))
            {
                var name = reference.FullName + "." + required.Name;
                if (type.Methods.Any(method => method.Name == name)) continue;
                var implementationMethod = new MethodDefinition(name,
                    MethodAttributes.Private | MethodAttributes.Virtual | MethodAttributes.Final | MethodAttributes.NewSlot | MethodAttributes.HideBySig,
                    Import(Substitute(required.ReturnType, context), module));
                foreach (var parameter in required.Parameters)
                    implementationMethod.Parameters.Add(new ParameterDefinition(parameter.Name, parameter.Attributes, Import(Substitute(parameter.ParameterType, context), module)));
                var overridden = module.ImportReference(required);
                if (context != null) overridden.DeclaringType = Import(reference, module);
                implementationMethod.Overrides.Add(overridden);
                type.Methods.Add(implementationMethod);
                EmitBody(implementationMethod, type, required.Name);
                report.EditorMembers++;
            }
        }
    }

    /// <summary>A type for a member of <paramref name="module"/>; its parts are imported one by one, since a generic parameter of the module's own needs none.</summary>
    static TypeReference Import(TypeReference type, ModuleDefinition module)
    {
        switch (type)
        {
            case GenericParameter:
                return type;
            case GenericInstanceType instance:
                var copy = new GenericInstanceType(Import(instance.ElementType, module));
                foreach (var argument in instance.GenericArguments) copy.GenericArguments.Add(Import(argument, module));
                return copy;
            case ArrayType array:
                return new ArrayType(Import(array.ElementType, module), array.Rank);
            case ByReferenceType reference:
                return new ByReferenceType(Import(reference.ElementType, module));
            default:
                return type.Module == module ? type : module.ImportReference(type);
        }
    }

    /// <summary>A type as a stand-in names it: types of stand-in assemblies are stand-ins, any other is the Player's.</summary>
    TypeReference Map(TypeReference type, ModuleDefinition module, TypeDefinition? owner, MethodDefinition? method)
    {
        switch (type)
        {
            case GenericParameter parameter:
                if (parameter.Type == GenericParameterType.Method && method != null && parameter.Position < method.GenericParameters.Count)
                    return method.GenericParameters[parameter.Position];
                if (parameter.Type == GenericParameterType.Type && owner != null && parameter.Position < owner.GenericParameters.Count)
                    return owner.GenericParameters[parameter.Position];
                return module.TypeSystem.Object;
            case ByReferenceType reference:
                return new ByReferenceType(Map(reference.ElementType, module, owner, method));
            case PointerType pointer:
                return new PointerType(Map(pointer.ElementType, module, owner, method));
            case PinnedType pinned:
                return new PinnedType(Map(pinned.ElementType, module, owner, method));
            case ArrayType array:
                return new ArrayType(Map(array.ElementType, module, owner, method), array.Rank);
            case RequiredModifierType required:
                return new RequiredModifierType(Map(required.ModifierType, module, owner, method), Map(required.ElementType, module, owner, method));
            case OptionalModifierType optional:
                return new OptionalModifierType(Map(optional.ModifierType, module, owner, method), Map(optional.ElementType, module, owner, method));
            case GenericInstanceType instance:
                var generic = new GenericInstanceType(Map(instance.ElementType, module, owner, method));
                foreach (var argument in instance.GenericArguments) generic.GenericArguments.Add(Map(argument, module, owner, method));
                return generic;
            case FunctionPointerType:
            case SentinelType:
                return module.TypeSystem.IntPtr;
        }
        var scope = Names.ScopeName(type);
        if (IsStandIn(scope))
        {
            var definition = Ensure(scope, type.FullName);
            return definition.Module == module ? definition : module.ImportReference(definition);
        }
        if (type.Module == module) return type;
        if (!playerNames.Contains(scope) && Names.Lookup(player, "mscorlib", type.FullName, out _) is { } library)
            return module.ImportReference(library);
        var imported = module.ImportReference(type);
        if (type.IsValueType) imported.IsValueType = true;
        return imported;
    }

    // ----- members ---------------------------------------------------------------

    void Collect(ModuleDefinition module)
    {
        foreach (var type in module.GetTypeReferences())
        {
            var scope = Names.ScopeName(type);
            if (IsStandIn(scope)) Ensure(scope, type.FullName);
        }
        var statics = StaticFieldUse(module);
        foreach (var member in module.GetMemberReferences())
        {
            var element = member.DeclaringType.GetElementType();
            var scope = Names.ScopeName(element);
            if (!IsStandIn(scope)) continue;
            var type = Ensure(scope, element.FullName);
            switch (member)
            {
                case MethodReference method:
                    AddMethod(type, method);
                    break;
                case FieldReference field:
                    AddField(type, field, statics.Contains(field.FullName));
                    break;
            }
        }
    }

    /// <summary>
    /// The methods of the stand-in types that the runtime bridge answers and the code does not
    /// name, with the Editor's visibility: code may reach a member of the Editor by reflection
    /// (Hive takes the Player settings' serialized object from the internal
    /// <c>PlayerSettings.GetSerializedObject</c>).
    /// </summary>
    void AddAnswered()
    {
        if (support == null) return;
        for (var count = -1; count != definitions.Count;)
        {
            count = definitions.Count;
            foreach (var (type, definition) in definitions.ToList())
            {
                if (definition == null || type.Namespace == Names.EngineNamespace) continue;
                var holder = support.Types.FirstOrDefault(candidate => candidate.Namespace == Names.SupportNamespace && candidate.Name == Names.Holder(type.FullName));
                if (holder == null) continue;
                foreach (var answer in holder.Methods.Where(method => method.IsPublic && method.IsStatic))
                {
                    foreach (var method in definition.Methods.Where(method => method.Name == answer.Name
                        && method.GenericParameters.Count == answer.GenericParameters.Count
                        && method.Parameters.Count + (method.HasThis ? 1 : 0) == answer.Parameters.Count).ToList())
                    {
                        if (type.Methods.Any(known => known.Name == method.Name && known.Parameters.Count == method.Parameters.Count)) continue;
                        var before = type.Methods.Count;
                        AddMethod(type, method);
                        if (type.Methods.Count == before) continue;
                        var added = type.Methods[^1];
                        added.Attributes = (added.Attributes & ~MethodAttributes.MemberAccessMask) | (method.Attributes & MethodAttributes.MemberAccessMask);
                    }
                }
            }
        }
    }

    static HashSet<string> StaticFieldUse(ModuleDefinition module)
    {
        var used = new HashSet<string>(StringComparer.Ordinal);
        foreach (var method in Names.Bodies(module))
        {
            foreach (var instruction in method.Body.Instructions)
            {
                if (instruction.Operand is FieldReference field
                    && (instruction.OpCode == OpCodes.Ldsfld || instruction.OpCode == OpCodes.Stsfld || instruction.OpCode == OpCodes.Ldsflda))
                    used.Add(field.FullName);
            }
        }
        return used;
    }

    static string MemberKey(TypeDefinition type, IMemberDefinition member) => type.Module.Assembly.Name.Name + "|" + type.FullName + "::" + member switch
    {
        MethodDefinition method => method.Name + "`" + method.GenericParameters.Count + "(" + string.Join(",", method.Parameters.Select(parameter => parameter.ParameterType.FullName)) + ")" + method.ReturnType.FullName,
        FieldDefinition field => field.Name,
        _ => member.Name,
    };

    void AddMethod(TypeDefinition type, MethodReference reference)
    {
        var module = type.Module;
        var method = new MethodDefinition(reference.Name, MethodAttributes.Public | MethodAttributes.HideBySig, module.TypeSystem.Void)
        {
            HasThis = reference.HasThis,
            ExplicitThis = reference.ExplicitThis,
            CallingConvention = reference.CallingConvention,
        };
        foreach (var parameter in reference.GenericParameters)
            method.GenericParameters.Add(new GenericParameter(parameter.Name, method));
        method.ReturnType = Map(reference.ReturnType, module, type, method);
        var index = 0;
        foreach (var parameter in reference.Parameters)
            method.Parameters.Add(new ParameterDefinition(parameter.Name.Length > 0 ? parameter.Name : "arg" + index++, parameter.Attributes, Map(parameter.ParameterType, module, type, method)));
        if (!generatedMembers.Add(MemberKey(type, method))) return;
        var definition = FindDefinition(type, reference);
        if (!reference.HasThis) method.IsStatic = true;
        if (definition != null)
        {
            method.IsVirtual = definition.IsVirtual;
            method.IsNewSlot = definition.IsNewSlot;
            method.IsSpecialName = definition.IsSpecialName;
            method.IsFinal = definition.IsFinal;
        }
        if (reference.Name is ".ctor" or ".cctor")
        {
            method.IsSpecialName = method.IsRuntimeSpecialName = true;
            method.IsVirtual = false;
        }
        // A body names `this` through the type that declares the method.
        type.Methods.Add(method);
        if (type.IsInterface)
        {
            method.IsVirtual = method.IsAbstract = method.IsNewSlot = true;
        }
        else if (method.IsConstructor && !method.IsStatic)
        {
            EmitConstructor(method, type, module);
        }
        else
        {
            EmitBody(method, type);
        }
        report.EditorMembers++;
    }

    MethodDefinition? FindDefinition(TypeDefinition type, MethodReference reference)
    {
        if (!definitions.TryGetValue(type, out var definition) || definition == null) return null;
        var candidates = definition.Methods.Where(method =>
            method.Name == reference.Name
            && method.Parameters.Count == reference.Parameters.Count
            && method.GenericParameters.Count == reference.GenericParameters.Count
            && method.HasThis == reference.HasThis).ToList();
        if (candidates.Count <= 1) return candidates.FirstOrDefault();
        return candidates.FirstOrDefault(method => method.Parameters.Select(parameter => parameter.ParameterType.Name)
            .SequenceEqual(reference.Parameters.Select(parameter => parameter.ParameterType.Name))) ?? candidates[0];
    }

    void AddField(TypeDefinition type, FieldReference reference, bool usedStatic)
    {
        if (type.Fields.Any(field => field.Name == reference.Name)) return;
        var module = type.Module;
        definitions.TryGetValue(type, out var owner);
        var definition = owner?.Fields.FirstOrDefault(field => field.Name == reference.Name);
        var attributes = FieldAttributes.Public;
        if (definition?.IsStatic ?? (usedStatic || owner == null)) attributes |= FieldAttributes.Static;
        if (definition != null && definition.IsLiteral && definition.HasConstant)
            attributes |= FieldAttributes.Literal | FieldAttributes.HasDefault;
        var field = new FieldDefinition(reference.Name, attributes, Map(reference.FieldType, module, type, null));
        if (field.IsLiteral) field.Constant = definition!.Constant;
        type.Fields.Add(field);
        report.EditorMembers++;
    }

    // ----- bodies ------------------------------------------------------------

    void EmitConstructor(MethodDefinition constructor, TypeDefinition type, ModuleDefinition module)
    {
        var il = constructor.Body.GetILProcessor();
        var baseConstructor = BaseConstructor(type, module);
        if (baseConstructor != null)
        {
            il.Emit(OpCodes.Ldarg_0);
            il.Emit(OpCodes.Call, baseConstructor);
        }
        // What the bridge keeps of a made object (`new SerializedObject(target)`): the public
        // static `Construct` of the type's class, with the instance first.
        var answer = FindSupport(type, constructor, "Construct");
        if (answer != null)
        {
            for (var index = 0; index <= constructor.Parameters.Count; index++) il.Emit(OpCodes.Ldarg, index);
            il.Emit(OpCodes.Call, module.ImportReference(answer));
            report.SupportedMembers++;
        }
        il.Emit(OpCodes.Ret);
    }

    MethodReference? BaseConstructor(TypeDefinition type, ModuleDefinition module)
    {
        var baseType = type.BaseType;
        if (baseType == null) return null;
        var element = baseType.GetElementType();
        TypeDefinition? resolved = element as TypeDefinition;
        if (resolved == null)
        {
            var scope = Names.ScopeName(element);
            resolved = generated.TryGetValue(scope + "|" + element.FullName, out var stand) ? stand
                : Names.Lookup(player, scope, element.FullName, out _);
        }
        var parameterless = resolved?.Methods.FirstOrDefault(method => method.IsConstructor && !method.IsStatic && method.Parameters.Count == 0 && !method.IsPrivate);
        if (parameterless == null) return null;
        var reference = parameterless.Module == module ? parameterless : module.ImportReference(parameterless);
        if (baseType is GenericInstanceType generic)
        {
            reference = new MethodReference(".ctor", module.TypeSystem.Void, generic) { HasThis = true };
        }
        return reference;
    }

    MethodDefinition? FindSupport(TypeDefinition type, MethodDefinition method, string? name = null)
    {
        if (support == null) return null;
        name ??= method.Name;
        // An engine member's stand-in is answered by the class named after the engine's type.
        var holderName = type.Namespace == Names.EngineNamespace ? type.Name : Names.Holder(type.FullName);
        var holder = support.Types.FirstOrDefault(candidate => candidate.Namespace == Names.SupportNamespace && candidate.Name == holderName);
        if (holder == null) return null;
        foreach (var candidate in holder.Methods)
        {
            if (!candidate.IsStatic || !candidate.IsPublic || candidate.Name != name
                || candidate.GenericParameters.Count != method.GenericParameters.Count) continue;
            var expected = new List<TypeReference>();
            if (method.HasThis) expected.Add(type);
            expected.AddRange(method.Parameters.Select(parameter => parameter.ParameterType));
            if (candidate.Parameters.Count != expected.Count) continue;
            var fits = true;
            for (var index = 0; index < expected.Count && fits; index++)
                fits = Accepts(candidate.Parameters[index].ParameterType, expected[index]);
            if (fits && (method.ReturnType.FullName == "System.Void" ? candidate.ReturnType.FullName == "System.Void" : Accepts(method.ReturnType, candidate.ReturnType)))
                return candidate;
        }
        return null;
    }

    /// <summary>Whether a value of type <paramref name="given"/> may stand where <paramref name="wanted"/> is declared.</summary>
    static bool Accepts(TypeReference wanted, TypeReference given)
    {
        if (wanted is GenericParameter wantedParameter && given is GenericParameter givenParameter)
            return wantedParameter.Type == givenParameter.Type && wantedParameter.Position == givenParameter.Position;
        if (wanted.FullName == given.FullName) return true;
        if (wanted is ByReferenceType || given is ByReferenceType) return false;
        var givenDefinition = given as TypeDefinition;
        if (wanted.FullName == "System.Int32" && givenDefinition?.IsEnum == true) return true;
        if (given.FullName == "System.Int32" && wanted is TypeDefinition { IsEnum: true }) return true;
        if (wanted.FullName == "System.Object") return !given.IsValueType && givenDefinition?.IsEnum != true && given is not GenericParameter;
        if (wanted.FullName == "System.Delegate") return Definition(given)?.BaseType?.FullName == "System.MulticastDelegate";
        // An answer of a generic type's member gives its type argument as an object.
        if (given.FullName == "System.Object" && wanted is GenericParameter) return true;
        if (given.FullName == "System.Object") return !wanted.IsValueType && wanted is not GenericParameter && wanted is not TypeDefinition { IsEnum: true };
        return false;
    }

    /// <summary>The definition of a type, also of a generic instance (<c>Action&lt;T&gt;</c>), or none.</summary>
    static TypeDefinition? Definition(TypeReference type)
    {
        try { return type.Resolve(); }
        catch (AssemblyResolutionException) { return null; }
    }

    void EmitBody(MethodDefinition method, TypeDefinition type, string? name = null)
    {
        var module = type.Module;
        var il = method.Body.GetILProcessor();
        if (type.Namespace == Names.EngineNamespace && EngineFact(type, method, il)) return;
        var answer = FindSupport(type, method, name);
        if (answer != null)
        {
            var count = method.Parameters.Count + (method.HasThis ? 1 : 0);
            for (var index = 0; index < count; index++) il.Emit(OpCodes.Ldarg, index);
            MethodReference target = module.ImportReference(answer);
            if (answer.HasGenericParameters)
            {
                var instance = new GenericInstanceMethod(target);
                foreach (var parameter in method.GenericParameters) instance.GenericArguments.Add(parameter);
                target = instance;
            }
            il.Emit(OpCodes.Call, target);
            if (method.ReturnType.FullName != "System.Void" && answer.ReturnType.FullName == "System.Object" && method.ReturnType.FullName != "System.Object")
                il.Emit(method.ReturnType is GenericParameter ? OpCodes.Unbox_Any : OpCodes.Castclass, method.ReturnType);
            il.Emit(OpCodes.Ret);
            report.SupportedMembers++;
            return;
        }
        EmitDefault(method, il, module);
    }

    /// <summary>What Play mode answers for an engine member the Player has as well (<c>AssemblyLink.Facts</c>).</summary>
    static bool EngineFact(TypeDefinition holder, MethodDefinition method, ILProcessor il)
    {
        switch (holder.Name + "::" + method.Name)
        {
            case "UnityEngine_Application::get_isEditor":
                il.Emit(OpCodes.Ldc_I4_1);
                il.Emit(OpCodes.Ret);
                return true;
            case "UnityEngine_Application::get_platform":
                // RuntimePlatform.LinuxEditor
                il.Emit(OpCodes.Ldc_I4, 16);
                il.Emit(OpCodes.Ret);
                return true;
        }
        return false;
    }

    void EmitDefault(MethodDefinition method, ILProcessor il, ModuleDefinition module)
    {
        var returnType = method.ReturnType;
        if (returnType.FullName == "System.Void")
        {
            il.Emit(OpCodes.Ret);
            return;
        }
        if (returnType is ByReferenceType reference)
        {
            il.Emit(OpCodes.Ldsflda, RefHolderValue(reference.ElementType, module));
            il.Emit(OpCodes.Ret);
            return;
        }
        // What a caller can go on with: an empty array or text rather than none,
        // as the Editor gives where it finds nothing (AssetDatabase.FindAssets).
        if (returnType is ArrayType { Rank: 1 } array)
        {
            il.Emit(OpCodes.Ldc_I4_0);
            il.Emit(OpCodes.Newarr, array.ElementType);
            il.Emit(OpCodes.Ret);
            return;
        }
        if (returnType.FullName == "System.String")
        {
            il.Emit(OpCodes.Ldstr, "");
            il.Emit(OpCodes.Ret);
            return;
        }
        var local = new VariableDefinition(returnType);
        method.Body.Variables.Add(local);
        method.Body.InitLocals = true;
        il.Emit(OpCodes.Ldloca, local);
        il.Emit(OpCodes.Initobj, returnType);
        il.Emit(OpCodes.Ldloc, local);
        il.Emit(OpCodes.Ret);
    }

    /// <summary><c>RefHolder&lt;T&gt;.Value</c> of the engine stand-in, the place a by-reference default refers to.</summary>
    FieldReference RefHolderValue(TypeReference element, ModuleDefinition module)
    {
        var engine = Shim(Names.EngineShim).MainModule;
        if (refHolder == null)
        {
            refHolder = new TypeDefinition("UnityMicroEditor.PlayMode", "RefHolder`1",
                TypeAttributes.Public | TypeAttributes.Abstract | TypeAttributes.Sealed | TypeAttributes.BeforeFieldInit, engine.TypeSystem.Object);
            refHolder.GenericParameters.Add(new GenericParameter("T", refHolder));
            refHolder.Fields.Add(new FieldDefinition("Value", FieldAttributes.Public | FieldAttributes.Static, refHolder.GenericParameters[0]));
            engine.Types.Add(refHolder);
        }
        var instance = new GenericInstanceType(module == engine ? refHolder : module.ImportReference(refHolder));
        instance.GenericArguments.Add(element);
        return new FieldReference("Value", instance.ElementType.GenericParameters[0], instance);
    }

    // ----- output ----------------------------------------------------------------

    void Write()
    {
        foreach (var (name, shim) in shims)
        {
            if (playerNames.Contains(name)) throw new InvalidOperationException($"{name} is the Player's own; no stand-in replaces it");
            // The same stand-ins make the same bytes: a running Player's are compared with
            // those of a later compile (hot reload of Play mode code).
            shim.Write(System.IO.Path.Combine(request.Managed, name + ".dll"),
                new WriterParameters { DeterministicMvid = true, Timestamp = 0 });
        }
    }

    void Verify()
    {
        var check = Names.Resolver(request.Managed);
        foreach (var module in code)
        {
            using var written = ModuleDefinition.ReadModule(module.FileName, new ReaderParameters { AssemblyResolver = check });
            foreach (var member in written.GetMemberReferences())
            {
                if (!IsStandIn(Names.ScopeName(member.DeclaringType.GetElementType()))) continue;
                IMemberDefinition? resolved = null;
                try { resolved = member.Resolve(); } catch (AssemblyResolutionException) { }
                if (resolved == null) report.Unresolved.Add($"{written.Assembly.Name.Name}: {member.FullName}");
            }
        }
    }
}
