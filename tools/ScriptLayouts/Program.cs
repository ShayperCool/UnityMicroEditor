// Describes what Unity's serializer stores of the script classes of compiled
// assemblies: for every class derived from MonoBehaviour or ScriptableObject,
// its serialized fields in order (those of its base classes first), and for
// every [Serializable] class or struct they hold, the same. The tool's
// playerbake lays the description out as the Player's type tree.
//
// A field is serialized when it is an instance field that is neither constant
// nor read-only nor [NonSerialized], public or [SerializeField] or
// [SerializeReference], of a type Unity serializes: a primitive, an enum (as
// its underlying type), a string, an engine object (by reference), an engine
// value type with a native layout (Vector3, AnimationCurve, ...), a
// [Serializable] class or struct of the game, or an array or List<T> of one
// of those. [SerializeReference] holds any class by managed reference: every
// [Serializable] class of the assemblies a field of that kind can hold is
// described as well, since the asset names the one it holds.
//
// A field's default is what the parameterless constructor of its class
// assigns it, as far as constants, empty collections, engine values and new
// serializable objects show it; Unity takes it for a field an asset lacks.
//
// Usage: ScriptLayouts <directory or assembly>...; prints the description as JSON.
using System.Text.Json;
using System.Text.Json.Nodes;
using Mono.Cecil;
using Mono.Cecil.Cil;

static class Program
{
    static int Main(string[] arguments)
    {
        if (arguments.Length == 0)
        {
            Console.Error.WriteLine("usage: ScriptLayouts <directory or assembly>...");
            return 2;
        }
        var resolver = new DefaultAssemblyResolver();
        var paths = new List<string>();
        foreach (var argument in arguments)
        {
            if (Directory.Exists(argument))
            {
                resolver.AddSearchDirectory(argument);
                paths.AddRange(Directory.GetFiles(argument, "*.dll").Order(StringComparer.Ordinal));
            }
            else
            {
                resolver.AddSearchDirectory(Path.GetDirectoryName(Path.GetFullPath(argument))!);
                paths.Add(argument);
            }
        }
        var layouts = new Layouts();
        var types = new List<TypeDefinition>();
        foreach (var path in paths)
        {
            AssemblyDefinition assembly;
            try
            {
                assembly = AssemblyDefinition.ReadAssembly(path, new ReaderParameters { AssemblyResolver = resolver });
            }
            catch (BadImageFormatException)
            {
                continue;
            }
            foreach (var type in assembly.MainModule.GetTypes())
            {
                layouts.AddScript(type, assembly.Name.Name);
                types.Add(type);
            }
        }
        layouts.AddManaged(types);
        Console.WriteLine(JsonSerializer.Serialize(layouts.Output, new JsonSerializerOptions
        {
            PropertyNamingPolicy = JsonNamingPolicy.CamelCase,
            DefaultIgnoreCondition = System.Text.Json.Serialization.JsonIgnoreCondition.WhenWritingNull,
        }));
        return 0;
    }
}

sealed class Output
{
    public int SchemaVersion { get; set; } = 1;
    /// <summary>Script classes by full name (<c>Namespace.Outer/Inner</c> as Unity names them).</summary>
    public SortedDictionary<string, Script> Scripts { get; } = new(StringComparer.Ordinal);
    /// <summary>The [Serializable] classes and structs scripts hold, by their key.</summary>
    public SortedDictionary<string, Composite> Types { get; } = new(StringComparer.Ordinal);
    /// <summary>The classes fields held by managed reference can hold, by assembly, namespace and name.</summary>
    public List<Managed> Managed { get; } = new();
}

sealed class Managed
{
    public string Assembly { get; set; } = "";
    public string Namespace { get; set; } = "";
    /// <summary>The class's name as an asset names it, <c>Outer/Inner</c> for a nested one.</summary>
    public string Name { get; set; } = "";
    /// <summary>The name its type tree gives it.</summary>
    public string TreeName { get; set; } = "";
    public List<Field> Fields { get; set; } = new();
}

sealed class Script
{
    public string Assembly { get; set; } = "";
    public string Namespace { get; set; } = "";
    public string Name { get; set; } = "";
    public bool Abstract { get; set; }
    public bool Generic { get; set; }
    public List<Field> Fields { get; set; } = new();
}

sealed class Composite
{
    /// <summary>The name a type tree gives the type.</summary>
    public string Name { get; set; } = "";
    public bool Struct { get; set; }
    public List<Field> Fields { get; set; } = new();
}

sealed class Field
{
    public string Name { get; set; } = "";
    public Kind Type { get; set; } = new();
    public bool? Hidden { get; set; }
    public List<string>? Formerly { get; set; }
    /// <summary>What the constructor assigns the field: a number, a string, an empty array, or the fields of a structure.</summary>
    public JsonNode? Default { get; set; }
}

sealed class Kind
{
    /// <summary>primitive, string, object, array, composite, engine, reference.</summary>
    public string Of { get; set; } = "";
    /// <summary>The primitive's C# name, the engine object's class, the engine value type, the composite's key or the referenced type.</summary>
    public string? Name { get; set; }
    /// <summary>An array's element; List&lt;T&gt; is marked.</summary>
    public Kind? Element { get; set; }
    public bool? List { get; set; }
}

sealed class Layouts
{
    public Output Output { get; } = new();

    /// <summary>Engine value types whose layout the engine defines natively.</summary>
    static readonly HashSet<string> EngineValues = new(StringComparer.Ordinal)
    {
        "UnityEngine.Vector2", "UnityEngine.Vector3", "UnityEngine.Vector4", "UnityEngine.Quaternion",
        "UnityEngine.Matrix4x4", "UnityEngine.Color", "UnityEngine.Color32", "UnityEngine.Rect",
        "UnityEngine.LayerMask", "UnityEngine.AnimationCurve", "UnityEngine.Gradient",
        "UnityEngine.RectOffset", "UnityEngine.GUIStyle", "UnityEngine.Bounds", "UnityEngine.Vector2Int",
        "UnityEngine.Vector3Int", "UnityEngine.RectInt", "UnityEngine.BoundsInt", "UnityEngine.Hash128",
        "UnityEngine.Rendering.SphericalHarmonicsL2", "UnityEngine.PropertyName",
    };

    static readonly Dictionary<string, string> Primitives = new(StringComparer.Ordinal)
    {
        ["System.Boolean"] = "bool", ["System.Byte"] = "byte", ["System.SByte"] = "sbyte",
        ["System.Char"] = "char", ["System.Int16"] = "short", ["System.UInt16"] = "ushort",
        ["System.Int32"] = "int", ["System.UInt32"] = "uint", ["System.Int64"] = "long",
        ["System.UInt64"] = "ulong", ["System.Single"] = "float", ["System.Double"] = "double",
    };

    /// <summary>The declared types of the fields held by managed reference, by full name.</summary>
    readonly HashSet<string> referenced = new(StringComparer.Ordinal);
    readonly HashSet<TypeDefinition> managed = new();

    /// <summary>Describes the classes of <paramref name="types"/> that a field held by managed
    /// reference can hold, and then those their own such fields can.</summary>
    public void AddManaged(List<TypeDefinition> types)
    {
        for (var known = -1; known != referenced.Count;)
        {
            known = referenced.Count;
            foreach (var type in types)
            {
                if (!type.IsClass || type.IsValueType || type.IsAbstract || type.IsInterface || type.HasGenericParameters) continue;
                if (!type.IsSerializable || managed.Contains(type) || DerivesFrom(type, "UnityEngine.Object")) continue;
                if (!Assignable(type)) continue;
                managed.Add(type);
                var outer = Outermost(type);
                Output.Managed.Add(new Managed
                {
                    Assembly = type.Module.Assembly.Name.Name,
                    Namespace = outer.Namespace,
                    Name = UnityName(type)[(outer.Namespace.Length > 0 ? outer.Namespace.Length + 1 : 0)..],
                    TreeName = type.Name,
                    Fields = FieldsOf(type, null),
                });
            }
        }
        Output.Managed.Sort((a, b) => string.CompareOrdinal($"{a.Assembly}|{a.Namespace}|{a.Name}", $"{b.Assembly}|{b.Namespace}|{b.Name}"));
    }

    /// <summary>Whether a field held by managed reference can hold an object of <paramref name="type"/>.</summary>
    bool Assignable(TypeDefinition type)
    {
        for (var current = type; current != null; current = current.BaseType is { } next ? Resolve(next) : null)
        {
            if (referenced.Contains(current.FullName)) return true;
            if (current.Interfaces.Any(implemented => Implements(implemented.InterfaceType))) return true;
        }
        return referenced.Contains("System.Object");
    }

    bool Implements(TypeReference implemented)
    {
        var definition = Resolve(implemented);
        if (referenced.Contains(definition?.FullName ?? implemented.FullName)) return true;
        return definition != null && definition.Interfaces.Any(inner => Implements(inner.InterfaceType));
    }

    public void AddScript(TypeDefinition type, string assembly)
    {
        if (!type.IsClass || type.IsInterface) return;
        if (!DerivesFrom(type, "UnityEngine.MonoBehaviour") && !DerivesFrom(type, "UnityEngine.ScriptableObject")) return;
        var script = new Script
        {
            Assembly = assembly,
            Namespace = Outermost(type).Namespace,
            Name = type.Name,
            Abstract = type.IsAbstract,
            Generic = type.HasGenericParameters,
        };
        if (!script.Generic) script.Fields = FieldsOf(type, null);
        Output.Scripts[UnityName(type)] = script;
    }

    static TypeDefinition Outermost(TypeDefinition type)
    {
        while (type.DeclaringType != null) type = type.DeclaringType;
        return type;
    }

    static string UnityName(TypeDefinition type)
    {
        var outer = Outermost(type);
        var nested = type.FullName[(outer.FullName.Length)..];
        return outer.FullName + nested;
    }

    static bool DerivesFrom(TypeDefinition type, string baseName)
    {
        for (var current = type.BaseType; current != null;)
        {
            if (current.FullName == baseName) return true;
            var resolved = Resolve(current);
            if (resolved == null) return false;
            current = resolved.BaseType;
        }
        return false;
    }

    static TypeDefinition? Resolve(TypeReference type)
    {
        try { return type.Resolve(); }
        catch (AssemblyResolutionException) { return null; }
    }

    static bool IsEngineBase(string name) => name is "UnityEngine.MonoBehaviour" or "UnityEngine.ScriptableObject"
        or "UnityEngine.Behaviour" or "UnityEngine.Component" or "UnityEngine.Object" or "System.Object"
        or "System.ValueType";

    /// <summary>The serialized fields of a type and its bases, the bases' first.
    /// <paramref name="arguments"/> binds the generic parameters of a generic instance.</summary>
    List<Field> FieldsOf(TypeDefinition type, Dictionary<string, TypeReference>? arguments)
    {
        var fields = new List<Field>();
        var defaults = Defaults.Of(type);
        if (type.BaseType != null && !IsEngineBase(type.BaseType.FullName) && Resolve(type.BaseType) is { } baseType)
        {
            fields.AddRange(FieldsOf(baseType, Bind(type.BaseType, baseType, arguments)));
        }
        foreach (var field in type.Fields)
        {
            if (field.IsStatic || field.IsLiteral || field.IsInitOnly || field.IsNotSerialized) continue;
            var reference = Has(field, "UnityEngine.SerializeReference");
            if (!field.IsPublic && !reference && !Has(field, "UnityEngine.SerializeField")) continue;
            var fieldType = Substitute(field.FieldType, arguments);
            var kind = reference ? ReferenceKind(fieldType) : KindOf(fieldType, true);
            if (kind == null) continue;
            var formerly = field.CustomAttributes
                .Where(attribute => attribute.AttributeType.FullName == "UnityEngine.Serialization.FormerlySerializedAsAttribute")
                .Select(attribute => attribute.ConstructorArguments[0].Value as string ?? "")
                .ToList();
            fields.Add(new Field
            {
                Name = field.Name,
                Type = kind,
                Hidden = Has(field, "UnityEngine.HideInInspector") ? true : null,
                Formerly = formerly.Count > 0 ? formerly : null,
                Default = defaults.TryGetValue(field.Name, out var value) ? value?.DeepClone() : null,
            });
        }
        return fields;
    }

    static bool Has(FieldDefinition field, string attribute) =>
        field.CustomAttributes.Any(candidate => candidate.AttributeType.FullName == attribute);

    static Dictionary<string, TypeReference>? Bind(TypeReference reference, TypeDefinition definition,
        Dictionary<string, TypeReference>? arguments)
    {
        if (reference is not GenericInstanceType instance) return null;
        var bound = new Dictionary<string, TypeReference>(StringComparer.Ordinal);
        for (var index = 0; index < instance.GenericArguments.Count && index < definition.GenericParameters.Count; index++)
        {
            bound[definition.GenericParameters[index].Name] = Substitute(instance.GenericArguments[index], arguments);
        }
        return bound;
    }

    static TypeReference Substitute(TypeReference type, Dictionary<string, TypeReference>? arguments)
    {
        if (arguments == null) return type;
        switch (type)
        {
            case GenericParameter parameter when arguments.TryGetValue(parameter.Name, out var bound):
                return bound;
            case ArrayType array:
                return new ArrayType(Substitute(array.ElementType, arguments), array.Rank);
            case GenericInstanceType instance:
                var copy = new GenericInstanceType(instance.ElementType);
                foreach (var argument in instance.GenericArguments) copy.GenericArguments.Add(Substitute(argument, arguments));
                return copy;
            default:
                return type;
        }
    }

    Kind? ReferenceKind(TypeReference type)
    {
        if (type is ArrayType array && array.Rank == 1)
        {
            return ReferenceKind(array.ElementType) is { } element ? new Kind { Of = "array", Element = element } : null;
        }
        if (type is GenericInstanceType list && list.ElementType.FullName == "System.Collections.Generic.List`1")
        {
            return ReferenceKind(list.GenericArguments[0]) is { } element
                ? new Kind { Of = "array", Element = element, List = true } : null;
        }
        if (type.IsValueType || type.IsGenericParameter) return null;
        referenced.Add(Resolve(type)?.FullName ?? type.FullName);
        return new Kind { Of = "reference", Name = type.Name };
    }

    /// <summary>How Unity serializes a value of <paramref name="type"/>, or null when it does not.</summary>
    Kind? KindOf(TypeReference type, bool arrays)
    {
        if (type is GenericParameter) return null;
        if (type is ArrayType array)
        {
            if (!arrays || array.Rank != 1) return null;
            return KindOf(array.ElementType, false) is { } element ? new Kind { Of = "array", Element = element } : null;
        }
        if (type is GenericInstanceType generic && generic.ElementType.FullName == "System.Collections.Generic.List`1")
        {
            if (!arrays) return null;
            return KindOf(generic.GenericArguments[0], false) is { } element
                ? new Kind { Of = "array", Element = element, List = true } : null;
        }
        if (Primitives.TryGetValue(type.FullName, out var primitive)) return new Kind { Of = "primitive", Name = primitive };
        if (type.FullName == "System.String") return new Kind { Of = "string" };
        if (EngineValues.Contains(type.FullName)) return new Kind { Of = "engine", Name = type.Name };
        var definition = Resolve(type);
        if (definition == null) return null;
        if (definition.IsEnum)
        {
            var underlying = definition.Fields.First(field => !field.IsStatic).FieldType.FullName;
            return Primitives.TryGetValue(underlying, out var name) ? new Kind { Of = "primitive", Name = name } : null;
        }
        if (definition.IsClass && !definition.IsValueType && DerivesFrom(definition, "UnityEngine.Object") || type.FullName == "UnityEngine.Object")
        {
            return new Kind { Of = "object", Name = definition.Name };
        }
        if (!definition.IsSerializable || definition.IsAbstract || definition.IsInterface) return null;
        var module = definition.Module.Assembly.Name.Name;
        if (module is "mscorlib" or "netstandard" || module.StartsWith("System", StringComparison.Ordinal)) return null;
        if (typeof(MulticastDelegate).FullName == definition.BaseType?.FullName) return null;
        var key = type.FullName;
        if (!Output.Types.ContainsKey(key))
        {
            var composite = new Composite { Name = definition.Name, Struct = definition.IsValueType };
            Output.Types[key] = composite;
            composite.Fields = FieldsOf(definition, Bind(type, definition, null));
        }
        return new Kind { Of = "composite", Name = key };
    }
}

/// <summary>The values a class's parameterless constructor assigns its fields, read from its IL.</summary>
static class Defaults
{
    static readonly Dictionary<TypeDefinition, Dictionary<string, JsonNode?>> Known = new();

    /// <summary>A value the IL shows nothing of.</summary>
    sealed class Unknown { public static readonly Unknown Value = new(); }
    sealed class This { public static readonly This Value = new(); }

    static readonly Dictionary<string, string[]> EngineFields = new(StringComparer.Ordinal)
    {
        ["UnityEngine.Vector2"] = new[] { "x", "y" },
        ["UnityEngine.Vector3"] = new[] { "x", "y", "z" },
        ["UnityEngine.Vector4"] = new[] { "x", "y", "z", "w" },
        ["UnityEngine.Quaternion"] = new[] { "x", "y", "z", "w" },
        ["UnityEngine.Color"] = new[] { "r", "g", "b", "a" },
        ["UnityEngine.Color32"] = new[] { "r", "g", "b", "a" },
        ["UnityEngine.Rect"] = new[] { "x", "y", "width", "height" },
        ["UnityEngine.Vector2Int"] = new[] { "x", "y" },
        ["UnityEngine.Vector3Int"] = new[] { "x", "y", "z" },
    };

    static readonly Dictionary<string, double[]> EngineConstants = new(StringComparer.Ordinal)
    {
        ["UnityEngine.Color::get_white"] = new[] { 1.0, 1, 1, 1 },
        ["UnityEngine.Color::get_black"] = new[] { 0.0, 0, 0, 1 },
        ["UnityEngine.Color::get_clear"] = new[] { 0.0, 0, 0, 0 },
        ["UnityEngine.Color::get_red"] = new[] { 1.0, 0, 0, 1 },
        ["UnityEngine.Color::get_green"] = new[] { 0.0, 1, 0, 1 },
        ["UnityEngine.Color::get_blue"] = new[] { 0.0, 0, 1, 1 },
        ["UnityEngine.Color::get_yellow"] = new[] { 1.0, 0.92156863, 0.015686275, 1 },
        ["UnityEngine.Color::get_gray"] = new[] { 0.5, 0.5, 0.5, 1 },
        ["UnityEngine.Color::get_grey"] = new[] { 0.5, 0.5, 0.5, 1 },
        ["UnityEngine.Vector2::get_zero"] = new[] { 0.0, 0 },
        ["UnityEngine.Vector2::get_one"] = new[] { 1.0, 1 },
        ["UnityEngine.Vector2::get_up"] = new[] { 0.0, 1 },
        ["UnityEngine.Vector2::get_right"] = new[] { 1.0, 0 },
        ["UnityEngine.Vector3::get_zero"] = new[] { 0.0, 0, 0 },
        ["UnityEngine.Vector3::get_one"] = new[] { 1.0, 1, 1 },
        ["UnityEngine.Vector3::get_up"] = new[] { 0.0, 1, 0 },
        ["UnityEngine.Vector3::get_right"] = new[] { 1.0, 0, 0 },
        ["UnityEngine.Vector3::get_forward"] = new[] { 0.0, 0, 1 },
        ["UnityEngine.Vector4::get_zero"] = new[] { 0.0, 0, 0, 0 },
        ["UnityEngine.Vector4::get_one"] = new[] { 1.0, 1, 1, 1 },
        ["UnityEngine.Quaternion::get_identity"] = new[] { 0.0, 0, 0, 1 },
        ["UnityEngine.Vector2Int::get_zero"] = new[] { 0.0, 0 },
        ["UnityEngine.Vector2Int::get_one"] = new[] { 1.0, 1 },
        ["UnityEngine.Vector3Int::get_zero"] = new[] { 0.0, 0, 0 },
        ["UnityEngine.Vector3Int::get_one"] = new[] { 1.0, 1, 1 },
    };

    static JsonObject Engine(string type, IReadOnlyList<double> values)
    {
        var result = new JsonObject();
        var names = EngineFields[type];
        for (var index = 0; index < names.Length; index++) result[names[index]] = index < values.Count ? values[index] : 0.0;
        return result;
    }

    /// <summary>The defaults the type's own constructor and its bases' assign, by field name.</summary>
    public static Dictionary<string, JsonNode?> Of(TypeDefinition type)
    {
        if (Known.TryGetValue(type, out var known)) return known;
        var defaults = new Dictionary<string, JsonNode?>(StringComparer.Ordinal);
        Known[type] = defaults;
        // The base's constructor runs first.
        TypeDefinition? baseType = null;
        try { baseType = type.BaseType?.Resolve(); }
        catch (AssemblyResolutionException) { }
        if (baseType != null && baseType.Module.Assembly.Name.Name is not ("mscorlib" or "netstandard"))
        {
            foreach (var (field, value) in Of(baseType)) defaults[field] = value?.DeepClone();
        }
        var constructor = type.Methods.FirstOrDefault(method => method.IsConstructor && !method.IsStatic
            && method.Parameters.Count == 0 && method.HasBody);
        if (constructor != null) Run(constructor, defaults);
        return defaults;
    }

    static void Run(MethodDefinition method, Dictionary<string, JsonNode?> defaults)
    {
        var stack = new Stack<object?>();
        object? Pop() => stack.Count > 0 ? stack.Pop() : Unknown.Value;
        foreach (var instruction in method.Body.Instructions)
        {
            var code = instruction.OpCode.Code;
            switch (code)
            {
                case Code.Nop: break;
                case Code.Ldarg_0: stack.Push(This.Value); break;
                case Code.Ldc_I4_M1: stack.Push(-1.0); break;
                case >= Code.Ldc_I4_0 and <= Code.Ldc_I4_8: stack.Push((double)(code - Code.Ldc_I4_0)); break;
                case Code.Ldc_I4_S: stack.Push((double)(sbyte)instruction.Operand); break;
                case Code.Ldc_I4: stack.Push((double)(int)instruction.Operand); break;
                case Code.Ldc_I8: stack.Push((double)(long)instruction.Operand); break;
                case Code.Ldc_R4: stack.Push((double)(float)instruction.Operand); break;
                case Code.Ldc_R8: stack.Push((double)instruction.Operand); break;
                case Code.Ldstr: stack.Push((string)instruction.Operand); break;
                case Code.Ldnull: stack.Push(null); break;
                case Code.Conv_R4 or Code.Conv_R8 or Code.Conv_I4 or Code.Conv_I8 or Code.Conv_U1 or Code.Conv_U2
                    or Code.Conv_U4 or Code.Conv_U8 or Code.Conv_I1 or Code.Conv_I2:
                    break;
                case Code.Dup: { var top = Pop(); stack.Push(top); stack.Push(top); break; }
                case Code.Pop: Pop(); break;
                case Code.Newarr:
                    stack.Push(Pop() is 0.0 ? new JsonArray() : Unknown.Value);
                    break;
                case Code.Newobj:
                {
                    var called = (MethodReference)instruction.Operand;
                    var arguments = new object?[called.Parameters.Count];
                    for (var index = arguments.Length - 1; index >= 0; index--) arguments[index] = Pop();
                    stack.Push(Construct(called, arguments));
                    break;
                }
                case Code.Call or Code.Callvirt:
                {
                    var called = (MethodReference)instruction.Operand;
                    var arguments = new object?[called.Parameters.Count];
                    for (var index = arguments.Length - 1; index >= 0; index--) arguments[index] = Pop();
                    var target = called.HasThis ? Pop() : null;
                    // A collection initializer.
                    if (target is JsonArray items && called.Name == "Add" && arguments.Length == 1)
                    {
                        if (arguments[0] is Unknown) items.Add(null);
                        else items.Add(Node(arguments[0]));
                    }
                    var name = called.DeclaringType.FullName + "::" + called.Name;
                    if (EngineConstants.TryGetValue(name, out var values))
                        stack.Push(Engine(called.DeclaringType.FullName, values));
                    else if (called.ReturnType.FullName != "System.Void")
                        stack.Push(Unknown.Value);
                    break;
                }
                case Code.Stfld:
                {
                    var field = (FieldReference)instruction.Operand;
                    var value = Pop();
                    var target = Pop();
                    var node = Node(value);
                    if (target is This)
                    {
                        if (value is Unknown) defaults.Remove(field.Name);
                        else defaults[field.Name] = node;
                    }
                    else if (target is JsonObject composite && value is not Unknown)
                    {
                        composite[field.Name] = node;
                    }
                    break;
                }
                case Code.Ret: return;
                default:
                    // What the stack holds is no longer known.
                    stack.Clear();
                    break;
            }
        }
    }

    static object? Construct(MethodReference constructor, object?[] arguments)
    {
        var type = constructor.DeclaringType;
        var name = type is GenericInstanceType generic ? generic.ElementType.FullName : type.FullName;
        if (name is "System.Collections.Generic.List`1" && arguments.All(argument => argument is double)) return new JsonArray();
        if (EngineFields.ContainsKey(name))
        {
            return arguments.All(argument => argument is double)
                ? Engine(name, arguments.Cast<double>().ToList())
                : Unknown.Value;
        }
        if (arguments.Length != 0) return Unknown.Value;
        TypeDefinition? definition;
        try { definition = type.Resolve(); }
        catch (AssemblyResolutionException) { return Unknown.Value; }
        if (definition == null || !definition.IsSerializable) return Unknown.Value;
        var result = new JsonObject();
        foreach (var (field, value) in Of(definition)) result[field] = value?.DeepClone();
        return result;
    }

    static JsonNode? Node(object? value) => value switch
    {
        double number => JsonValue.Create(number),
        string text => JsonValue.Create(text),
        JsonNode node => node,
        _ => null,
    };
}
