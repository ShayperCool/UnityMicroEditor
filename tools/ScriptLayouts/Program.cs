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
// of those. [SerializeReference] holds any class by managed reference.
//
// Usage: ScriptLayouts <directory or assembly>...; prints the description as JSON.
using System.Text.Json;
using Mono.Cecil;

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
            foreach (var type in assembly.MainModule.GetTypes()) layouts.AddScript(type, assembly.Name.Name);
        }
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
