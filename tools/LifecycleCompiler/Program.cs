using System.Text.Json;
using System.Buffers.Binary;
using System.Collections.Immutable;
using System.Reflection.Metadata;
using System.Reflection.Metadata.Ecma335;
using System.Reflection.PortableExecutable;
using System.Security.Cryptography;
using System.Text;
using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.CSharp;
using Microsoft.CodeAnalysis.CSharp.Syntax;
using FieldAttributes = System.Reflection.FieldAttributes;
using MethodAttributes = System.Reflection.MethodAttributes;
using TypeAttributes = System.Reflection.TypeAttributes;

internal sealed record SourceInput(string Path, string OriginalPath, string OutputPath);
internal sealed record Request(string AssemblyName, string[] References, string[] Defines,
    SourceInput[] Sources, string MarkerPath, bool AllowUnsafe = false,
    Dictionary<string, int>? ExecutionOrders = null, string[]? NativeBaseAssemblies = null);
internal sealed record CallbackInfo(string Type, string Phase, string Source, int Order, bool Synthetic = false);
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

internal sealed record LayoutScript(string Guid, string Assembly, string Type);
internal sealed record LayoutRequest(string[] Assemblies, LayoutScript[] Scripts);

// Reads, from assembly metadata alone, which fields of a ScriptableObject
// class, a StateMachineBehaviour among them, the engine serializes, in the
// engine's order. Whatever the reader
// cannot decide the way the engine does makes the class unsupported; it never
// guesses a field in or out.
internal static class BehaviourLayouts
{
    // The engine cuts a type off once this many serialized types enclose it.
    // What it keeps of such a type is not reproduced here.
    private const int DepthLimit = 10;
    // The metadata bits of [NonSerialized] and [Serializable], whose names the framework marks obsolete.
    private const FieldAttributes NotSerialized = (FieldAttributes)0x0080;
    private const TypeAttributes Serializable = (TypeAttributes)0x2000;

    private sealed class Unsupported : Exception
    {
        public Unsupported(string reason) : base(reason) { }
    }

    private sealed class Module
    {
        public string Name = "";
        public string Path = "";
        public MetadataReader Reader = null!;
        // Method bodies are read on demand: only a few classes have constructors that matter.
        private PEReader? image;
        public MethodBodyBlock? Body(int address)
        {
            try
            {
                image ??= new PEReader(ImmutableArray.Create(File.ReadAllBytes(Path)));
                return image.GetMethodBody(address);
            }
            catch (Exception error) when (error is BadImageFormatException or IOException or InvalidOperationException) { return null; }
        }
        public readonly Dictionary<(string, string), TypeDefinitionHandle> Types = new();
        public readonly Dictionary<(string, string), string> Forwarded = new();
        public bool Engine => Name == "UnityEngine" || Name.StartsWith("UnityEngine.", StringComparison.Ordinal);
    }

    private readonly record struct TypeId(Module Module, TypeDefinitionHandle Handle)
    {
        public TypeDefinition Definition => Module.Reader.GetTypeDefinition(Handle);
        public string Name => Module.Reader.GetString(Definition.Name);
        public bool Nested => !Definition.GetDeclaringType().IsNil;
        public string Namespace
        {
            get
            {
                var definition = Definition;
                while (!definition.GetDeclaringType().IsNil) definition = Module.Reader.GetTypeDefinition(definition.GetDeclaringType());
                return Module.Reader.GetString(definition.Namespace);
            }
        }
    }

    private abstract record Sig;
    private sealed record PrimitiveSig(PrimitiveTypeCode Code) : Sig;
    // A class or struct: its full name, and its definition if an assembly of the request holds it.
    private sealed record NamedSig(string FullName, TypeId? Type) : Sig;
    private sealed record InstanceSig(NamedSig Generic, ImmutableArray<Sig> Arguments) : Sig;
    private sealed record ArraySig(Sig Element, bool Vector) : Sig;
    private sealed record ParameterSig(int Index) : Sig;
    private sealed record OtherSig : Sig;

    private sealed class Universe : ISignatureTypeProvider<Sig, object?>
    {
        private readonly Dictionary<string, Module> modules = new(StringComparer.Ordinal);
        // A metadata reader points into the memory of its file's reader.
        private readonly List<PEReader> files = new();
        // Signatures are decoded for one module at a time.
        private Module current = null!;

        public Universe(string[] paths)
        {
            foreach (var path in paths)
            {
                using var stream = File.OpenRead(path);
                if (stream.Length > 1024L * 1024 * 1024) throw new InvalidOperationException("Layout input exceeds the DLL bound");
                PEReader pe;
                // A native library beside the managed ones is not an input.
                try { pe = new PEReader(stream, PEStreamOptions.PrefetchMetadata); }
                catch (BadImageFormatException) { continue; }
                if (!pe.HasMetadata) continue;
                files.Add(pe);
                var reader = pe.GetMetadataReader();
                if (!reader.IsAssembly) continue;
                var module = new Module { Name = reader.GetString(reader.GetAssemblyDefinition().Name), Path = path, Reader = reader };
                // The first file of a name wins, as the caller lists the project's own assemblies first.
                if (!modules.TryAdd(module.Name, module)) continue;
                foreach (var handle in reader.TypeDefinitions)
                {
                    var definition = reader.GetTypeDefinition(handle);
                    if (definition.GetDeclaringType().IsNil)
                        module.Types.TryAdd((reader.GetString(definition.Namespace), reader.GetString(definition.Name)), handle);
                }
                foreach (var handle in reader.ExportedTypes)
                {
                    var exported = reader.GetExportedType(handle);
                    if (exported.IsForwarder && exported.Implementation.Kind == HandleKind.AssemblyReference)
                        module.Forwarded.TryAdd((reader.GetString(exported.Namespace), reader.GetString(exported.Name)),
                            reader.GetString(reader.GetAssemblyReference((AssemblyReferenceHandle)exported.Implementation).Name));
                }
            }
        }

        public Module? Find(string assembly) => modules.GetValueOrDefault(assembly);

        public TypeId? Find(string assembly, string ns, string name)
        {
            for (int hop = 0; hop < 8 && modules.TryGetValue(assembly, out var module); hop++)
            {
                if (module.Types.TryGetValue((ns, name), out var handle)) return new TypeId(module, handle);
                if (!module.Forwarded.TryGetValue((ns, name), out var next)) return null;
                assembly = next;
            }
            return null;
        }

        // The definition a type reference of a module leads to and its full name.
        public NamedSig Named(Module module, EntityHandle handle)
        {
            var reader = module.Reader;
            if (handle.Kind == HandleKind.TypeDefinition)
            {
                var type = new TypeId(module, (TypeDefinitionHandle)handle);
                return new NamedSig(FullName(type), type);
            }
            if (handle.Kind != HandleKind.TypeReference) return new NamedSig("", null);
            var reference = reader.GetTypeReference((TypeReferenceHandle)handle);
            string name = reader.GetString(reference.Name), ns = reader.GetString(reference.Namespace);
            switch (reference.ResolutionScope.Kind)
            {
                case HandleKind.AssemblyReference:
                    string assembly = reader.GetString(reader.GetAssemblyReference((AssemblyReferenceHandle)reference.ResolutionScope).Name);
                    return new NamedSig(ns.Length == 0 ? name : ns + "." + name, Find(assembly, ns, name));
                case HandleKind.ModuleDefinition:
                    return new NamedSig(ns.Length == 0 ? name : ns + "." + name, Find(module.Name, ns, name));
                case HandleKind.TypeReference:
                    var outer = Named(module, (EntityHandle)reference.ResolutionScope);
                    TypeId? nested = null;
                    if (outer.Type is { } parent)
                        foreach (var candidate in parent.Definition.GetNestedTypes())
                            if (parent.Module.Reader.GetString(parent.Module.Reader.GetTypeDefinition(candidate).Name) == name)
                                nested = new TypeId(parent.Module, candidate);
                    return new NamedSig(outer.FullName + "+" + name, nested);
                default:
                    return new NamedSig(ns.Length == 0 ? name : ns + "." + name, null);
            }
        }

        public static string FullName(TypeId type)
        {
            var definition = type.Definition;
            string name = type.Name;
            if (!definition.GetDeclaringType().IsNil)
                return FullName(new TypeId(type.Module, definition.GetDeclaringType())) + "+" + name;
            string ns = type.Module.Reader.GetString(definition.Namespace);
            return ns.Length == 0 ? name : ns + "." + name;
        }

        // A type as a signature of `module` names it: a field's type or a base type.
        public Sig Decode(Module module, EntityHandle handle)
        {
            current = module;
            return handle.Kind == HandleKind.TypeSpecification
                ? module.Reader.GetTypeSpecification((TypeSpecificationHandle)handle).DecodeSignature(this, null)
                : Named(module, handle);
        }

        // The provider, set to decode signatures of `module`.
        public Universe Enter(Module module)
        {
            current = module;
            return this;
        }

        public Sig Decode(Module module, FieldDefinition field)
        {
            current = module;
            return field.DecodeSignature(this, null);
        }

        public Sig GetPrimitiveType(PrimitiveTypeCode typeCode) => new PrimitiveSig(typeCode);
        public Sig GetTypeFromDefinition(MetadataReader reader, TypeDefinitionHandle handle, byte rawTypeKind) => Named(current, handle);
        public Sig GetTypeFromReference(MetadataReader reader, TypeReferenceHandle handle, byte rawTypeKind) => Named(current, handle);
        public Sig GetTypeFromSpecification(MetadataReader reader, object? genericContext, TypeSpecificationHandle handle, byte rawTypeKind) =>
            reader.GetTypeSpecification(handle).DecodeSignature(this, genericContext);
        public Sig GetSZArrayType(Sig elementType) => new ArraySig(elementType, true);
        public Sig GetArrayType(Sig elementType, ArrayShape shape) => new ArraySig(elementType, false);
        public Sig GetGenericInstantiation(Sig genericType, ImmutableArray<Sig> typeArguments) =>
            genericType is NamedSig named ? new InstanceSig(named, typeArguments) : new OtherSig();
        public Sig GetGenericTypeParameter(object? genericContext, int index) => new ParameterSig(index);
        public Sig GetGenericMethodParameter(object? genericContext, int index) => new OtherSig();
        public Sig GetByReferenceType(Sig elementType) => new OtherSig();
        public Sig GetPointerType(Sig elementType) => new OtherSig();
        public Sig GetPinnedType(Sig elementType) => new OtherSig();
        public Sig GetFunctionPointerType(MethodSignature<Sig> signature) => new OtherSig();
        // `volatile` is a required modifier; it does not change what is stored.
        public Sig GetModifiedType(Sig modifier, Sig unmodifiedType, bool isRequired) => unmodifiedType;
    }

    private static readonly Dictionary<string, string> EngineStructs = new(StringComparer.Ordinal)
    {
        ["UnityEngine.Vector2"] = "Vector2", ["UnityEngine.Vector3"] = "Vector3", ["UnityEngine.Vector4"] = "Vector4",
        ["UnityEngine.Quaternion"] = "Quaternion", ["UnityEngine.Color"] = "Color", ["UnityEngine.Color32"] = "Color32",
        ["UnityEngine.Rect"] = "Rect", ["UnityEngine.Bounds"] = "Bounds", ["UnityEngine.LayerMask"] = "LayerMask",
        ["UnityEngine.AnimationCurve"] = "AnimationCurve", ["UnityEngine.Gradient"] = "Gradient",
        ["UnityEngine.Vector2Int"] = "Vector2Int", ["UnityEngine.Vector3Int"] = "Vector3Int", ["UnityEngine.RectInt"] = "RectInt",
        ["UnityEngine.BoundsInt"] = "BoundsInt", ["UnityEngine.Matrix4x4"] = "Matrix4x4", ["UnityEngine.Hash128"] = "Hash128",
        ["UnityEngine.RectOffset"] = "RectOffset", ["UnityEngine.RenderingLayerMask"] = "RenderingLayerMask",
        ["UnityEngine.Rendering.SphericalHarmonicsL2"] = "SphericalHarmonicsL2", ["UnityEngine.LazyLoadReference`1"] = "LazyLoadReference",
    };

    // Engine structs a build was seen to leave out of a script's data.
    private static readonly HashSet<string> EngineStructsNotSerialized = new(StringComparer.Ordinal)
    {
        "UnityEngine.RangeInt", "UnityEngine.Ray", "UnityEngine.Plane", "UnityEngine.Keyframe", "UnityEngine.GradientColorKey",
    };

    private static string? Keyword(PrimitiveTypeCode code) => code switch
    {
        PrimitiveTypeCode.Boolean => "bool", PrimitiveTypeCode.Byte => "byte", PrimitiveTypeCode.SByte => "sbyte",
        PrimitiveTypeCode.Int16 => "short", PrimitiveTypeCode.UInt16 => "ushort", PrimitiveTypeCode.Int32 => "int",
        PrimitiveTypeCode.UInt32 => "uint", PrimitiveTypeCode.Int64 => "long", PrimitiveTypeCode.UInt64 => "ulong",
        PrimitiveTypeCode.Single => "float", PrimitiveTypeCode.Double => "double", PrimitiveTypeCode.Char => "char",
        _ => null,
    };

    private static Sig Substitute(Sig sig, ImmutableArray<Sig> arguments) => sig switch
    {
        ParameterSig parameter => parameter.Index < arguments.Length ? arguments[parameter.Index] : new OtherSig(),
        ArraySig array => new ArraySig(Substitute(array.Element, arguments), array.Vector),
        InstanceSig instance => new InstanceSig(instance.Generic, instance.Arguments.Select(argument => Substitute(argument, arguments)).ToImmutableArray()),
        _ => sig,
    };

    private static IEnumerable<(string Name, CustomAttribute Attribute)> Attributes(Module module, CustomAttributeHandleCollection handles)
    {
        var reader = module.Reader;
        foreach (var handle in handles)
        {
            var attribute = reader.GetCustomAttribute(handle);
            EntityHandle type = attribute.Constructor.Kind == HandleKind.MemberReference
                ? reader.GetMemberReference((MemberReferenceHandle)attribute.Constructor).Parent
                : reader.GetMethodDefinition((MethodDefinitionHandle)attribute.Constructor).GetDeclaringType();
            string name = "";
            if (type.Kind == HandleKind.TypeReference)
            {
                var reference = reader.GetTypeReference((TypeReferenceHandle)type);
                name = reader.GetString(reference.Namespace) + "." + reader.GetString(reference.Name);
            }
            else if (type.Kind == HandleKind.TypeDefinition)
            {
                var definition = reader.GetTypeDefinition((TypeDefinitionHandle)type);
                name = reader.GetString(definition.Namespace) + "." + reader.GetString(definition.Name);
            }
            yield return (name, attribute);
        }
    }

    // The base type of a class with the class's own type arguments applied.
    // The definition is absent for the roots of the base library.
    private static (TypeId? Type, ImmutableArray<Sig> Arguments, string FullName)? Base(Universe universe, TypeId type, ImmutableArray<Sig> arguments)
    {
        var handle = type.Definition.BaseType;
        if (handle.IsNil) return null;
        var sig = Substitute(universe.Decode(type.Module, handle), arguments);
        var (named, baseArguments) = sig switch
        {
            NamedSig plain => (plain, ImmutableArray<Sig>.Empty),
            InstanceSig instance => (instance.Generic, instance.Arguments),
            _ => throw new Unsupported("a base type that is not a class"),
        };
        if (named.FullName is "System.Object" or "System.ValueType" or "System.Enum" or "System.MulticastDelegate" or "System.Delegate")
            return (null, baseArguments, named.FullName);
        if (named.Type is not { } resolved) throw new Unsupported($"type {named.FullName} is in none of the given assemblies");
        return (resolved, baseArguments, named.FullName);
    }

    private static bool DerivesFrom(Universe universe, TypeId type, string ns, string name)
    {
        TypeId current = type;
        for (int depth = 0; depth < 64; depth++)
        {
            if (current.Module.Engine && !current.Nested && current.Name == name && current.Namespace == ns) return true;
            if (Base(universe, current, ImmutableArray<Sig>.Empty) is not { Type: { } parent }) return false;
            current = parent;
        }
        throw new Unsupported("a class hierarchy deeper than the reader follows");
    }


    // What a field holds once the constructors of its class have run, as far as the code tells.
    private abstract record Initial;
    private sealed record ConstantInitial(object? Value) : Initial;
    // A new object of this type, made by its constructor without arguments, or an empty array.
    private sealed record FreshInitial(Sig? Type) : Initial;
    private sealed record UnknownInitial : Initial;
    private sealed record SelfInitial : Initial;

    private static bool SameSig(Sig a, Sig b) => (a, b) switch
    {
        (PrimitiveSig x, PrimitiveSig y) => x.Code == y.Code,
        (NamedSig x, NamedSig y) => x.FullName == y.FullName,
        (InstanceSig x, InstanceSig y) => x.Generic.FullName == y.Generic.FullName && x.Arguments.Length == y.Arguments.Length
            && x.Arguments.Zip(y.Arguments).All(pair => SameSig(pair.First, pair.Second)),
        (ArraySig x, ArraySig y) => x.Vector == y.Vector && SameSig(x.Element, y.Element),
        (ParameterSig x, ParameterSig y) => x.Index == y.Index,
        _ => false,
    };

    // Follows the constructor the engine runs for a new object, the one without parameters, and
    // records what it stores in the object's fields. The engine reads an authored object over
    // such an instance, so a field the text lacks keeps what the constructor left in it. Code
    // the reader cannot follow makes the whole object unknown rather than guessed at.
    private sealed class Construction
    {
        private static readonly Dictionary<ushort, System.Reflection.Emit.OpCode> Codes = typeof(System.Reflection.Emit.OpCodes)
            .GetFields(System.Reflection.BindingFlags.Public | System.Reflection.BindingFlags.Static)
            .Select(field => (System.Reflection.Emit.OpCode)field.GetValue(null)!).ToDictionary(code => (ushort)code.Value);

        private readonly Universe universe;
        private readonly Dictionary<TypeId, Dictionary<(TypeId, string), Initial>?> known = new();
        public Construction(Universe universe) { this.universe = universe; }

        // The fields the constructors of a type and of its base types set; null if that is unknown.
        public Dictionary<(TypeId, string), Initial>? Of(TypeId type)
        {
            if (known.TryGetValue(type, out var found)) return found;
            var result = new Dictionary<(TypeId, string), Initial>();
            if (!Run(type, result, 0)) result = null;
            known[type] = result;
            return result;
        }

        private bool Run(TypeId type, Dictionary<(TypeId, string), Initial> fields, int depth)
        {
            if (depth > 64) return false;
            var reader = type.Module.Reader;
            var definition = type.Definition;
            bool valueType = Base(universe, type, ImmutableArray<Sig>.Empty)?.FullName == "System.ValueType";
            MethodDefinition? constructor = null;
            foreach (var handle in definition.GetMethods())
            {
                var method = reader.GetMethodDefinition(handle);
                if ((method.Attributes & MethodAttributes.Static) != 0 || reader.GetString(method.Name) != ".ctor") continue;
                if (method.DecodeSignature(universe.Enter(type.Module), null).ParameterTypes.Length != 0) continue;
                // Only a public constructor was compared with the engine.
                if ((method.Attributes & MethodAttributes.MemberAccessMask) != MethodAttributes.Public) return false;
                constructor = method;
            }
            // The engine makes an object of a class without such a constructor, and a struct, without running code.
            if (constructor is not { } run) return true;
            if (valueType || run.RelativeVirtualAddress == 0) return false;
            var body = type.Module.Body(run.RelativeVirtualAddress);
            if (body is null || body.ExceptionRegions.Length != 0) return false;
            var il = body.GetILReader();
            var stack = new List<Initial>();
            Initial Pop() { var top = stack[^1]; stack.RemoveAt(stack.Count - 1); return top; }
            bool ended = false;
            while (il.RemainingBytes > 0)
            {
                if (ended) return false;
                ushort value = il.ReadByte();
                if (value == 0xfe) value = (ushort)(0xfe00 | il.ReadByte());
                if (!Codes.TryGetValue(value, out var code)) return false;
                long operand = 0;
                switch (code.OperandType)
                {
                    case System.Reflection.Emit.OperandType.InlineNone: break;
                    case System.Reflection.Emit.OperandType.ShortInlineBrTarget:
                    case System.Reflection.Emit.OperandType.ShortInlineI: operand = il.ReadSByte(); break;
                    case System.Reflection.Emit.OperandType.ShortInlineVar: operand = il.ReadByte(); break;
                    case System.Reflection.Emit.OperandType.InlineVar: operand = il.ReadUInt16(); break;
                    case System.Reflection.Emit.OperandType.InlineI8:
                    case System.Reflection.Emit.OperandType.InlineR: operand = il.ReadInt64(); break;
                    case System.Reflection.Emit.OperandType.InlineSwitch: return false;
                    default: operand = il.ReadInt32(); break;
                }
                if (code.FlowControl is System.Reflection.Emit.FlowControl.Branch or System.Reflection.Emit.FlowControl.Cond_Branch or System.Reflection.Emit.FlowControl.Throw)
                    return false;
                string name = code.Name ?? "";
                int pops, pushes;
                switch (name)
                {
                    case "nop": continue;
                    case "ret": if (stack.Count != 0) return false; ended = true; continue;
                    case "ldarg.0": stack.Add(new SelfInitial()); continue;
                    case "ldnull": stack.Add(new ConstantInitial(null)); continue;
                    case "ldstr": stack.Add(new ConstantInitial(reader.GetUserString(MetadataTokens.UserStringHandle((int)operand & 0xffffff)))); continue;
                    case "ldc.i4.m1": stack.Add(new ConstantInitial(-1L)); continue;
                    case "ldc.i4.0": case "ldc.i4.1": case "ldc.i4.2": case "ldc.i4.3": case "ldc.i4.4": case "ldc.i4.5": case "ldc.i4.6": case "ldc.i4.7": case "ldc.i4.8":
                        stack.Add(new ConstantInitial((long)(name[^1] - '0'))); continue;
                    case "ldc.i4.s": case "ldc.i4": stack.Add(new ConstantInitial((long)(int)operand)); continue;
                    case "ldc.i8": stack.Add(new ConstantInitial(operand)); continue;
                    case "ldc.r4": stack.Add(new ConstantInitial((double)BitConverter.Int32BitsToSingle((int)operand))); continue;
                    case "ldc.r8": stack.Add(new ConstantInitial(BitConverter.Int64BitsToDouble(operand))); continue;
                    case "conv.i8": case "conv.u8": case "conv.r4": case "conv.r8":
                    {
                        if (stack.Count == 0) return false;
                        var top = Pop();
                        if (top is SelfInitial) return false;
                        stack.Add(top is ConstantInitial { Value: long integer } ? new ConstantInitial(name switch
                        {
                            "conv.i8" => integer,
                            // An unsigned 32-bit constant is loaded as its signed bits.
                            "conv.u8" => (long)(uint)integer,
                            "conv.r4" => (object)(double)(float)integer,
                            _ => (double)integer,
                        }) : top is ConstantInitial { Value: double real } && name is "conv.r4" or "conv.r8"
                            ? new ConstantInitial(name == "conv.r4" ? (double)(float)real : real) : new UnknownInitial());
                        continue;
                    }
                    case "dup":
                    {
                        if (stack.Count == 0) return false;
                        var top = Pop();
                        if (top is SelfInitial) return false;
                        // An object under construction is no longer the plain new object.
                        if (top is FreshInitial) top = new UnknownInitial();
                        stack.Add(top); stack.Add(top);
                        continue;
                    }
                    case "newarr":
                    {
                        if (stack.Count == 0) return false;
                        var length = Pop();
                        if (length is SelfInitial) return false;
                        stack.Add(length is ConstantInitial { Value: 0L } ? new FreshInitial(null) : new UnknownInitial());
                        continue;
                    }
                    case "ldfld": case "ldflda":
                    {
                        if (stack.Count == 0) return false;
                        var owner = Pop();
                        if (owner is SelfInitial)
                        {
                            // Whatever takes the address may write the field, and whatever
                            // loads a new object may change it.
                            if (Field(type.Module, (int)operand) is not { } taken) return false;
                            if (name == "ldflda" || fields.GetValueOrDefault(taken) is FreshInitial) fields[taken] = new UnknownInitial();
                        }
                        stack.Add(new UnknownInitial());
                        continue;
                    }
                    case "stfld":
                    {
                        if (stack.Count < 2) return false;
                        var stored = Pop(); var owner = Pop();
                        if (stored is SelfInitial) return false;
                        if (owner is not SelfInitial) continue;
                        if (Field(type.Module, (int)operand) is not { } target) return false;
                        fields[target] = stored;
                        continue;
                    }
                    case "call": case "callvirt": case "newobj":
                    {
                        if (Method(type.Module, (int)operand) is not { } method) return false;
                        pops = method.Parameters + (method.Instance && name != "newobj" ? 1 : 0);
                        if (stack.Count < pops) return false;
                        var arguments = stack.GetRange(stack.Count - pops, pops);
                        stack.RemoveRange(stack.Count - pops, pops);
                        bool chained = name == "call" && method.Name == ".ctor" && arguments.Count > 0 && arguments[0] is SelfInitial;
                        if (arguments.Skip(chained ? 1 : 0).Any(argument => argument is SelfInitial)) return false;
                        if (chained)
                        {
                            // The constructor of the base class: of the project, followed in turn; of the engine or the base library, without fields of ours.
                            if (method.Owner is not NamedSig owner || owner.Type == type) return false;
                            if (owner.Type is { } parent && !parent.Module.Engine)
                            {
                                if (method.Parameters != 0 || !Run(parent, fields, depth + 1)) return false;
                            }
                            else if (owner.Type is null && owner.FullName != "System.Object") return false;
                            continue;
                        }
                        if (name == "newobj") stack.Add(method.Parameters == 0 ? new FreshInitial(method.Owner) : new UnknownInitial());
                        else if (method.Returns) stack.Add(new UnknownInitial());
                        continue;
                    }
                }
                pops = code.StackBehaviourPop switch
                {
                    System.Reflection.Emit.StackBehaviour.Pop0 => 0,
                    System.Reflection.Emit.StackBehaviour.Pop1 or System.Reflection.Emit.StackBehaviour.Popi or System.Reflection.Emit.StackBehaviour.Popref => 1,
                    System.Reflection.Emit.StackBehaviour.Pop1_pop1 or System.Reflection.Emit.StackBehaviour.Popi_pop1 or System.Reflection.Emit.StackBehaviour.Popi_popi
                        or System.Reflection.Emit.StackBehaviour.Popi_popi8 or System.Reflection.Emit.StackBehaviour.Popi_popr4 or System.Reflection.Emit.StackBehaviour.Popi_popr8
                        or System.Reflection.Emit.StackBehaviour.Popref_pop1 or System.Reflection.Emit.StackBehaviour.Popref_popi => 2,
                    System.Reflection.Emit.StackBehaviour.Popi_popi_popi or System.Reflection.Emit.StackBehaviour.Popref_popi_popi or System.Reflection.Emit.StackBehaviour.Popref_popi_popi8
                        or System.Reflection.Emit.StackBehaviour.Popref_popi_popr4 or System.Reflection.Emit.StackBehaviour.Popref_popi_popr8
                        or System.Reflection.Emit.StackBehaviour.Popref_popi_popref or System.Reflection.Emit.StackBehaviour.Popref_popi_pop1 => 3,
                    _ => -1,
                };
                pushes = code.StackBehaviourPush switch
                {
                    System.Reflection.Emit.StackBehaviour.Push0 => 0,
                    System.Reflection.Emit.StackBehaviour.Push1 or System.Reflection.Emit.StackBehaviour.Pushi or System.Reflection.Emit.StackBehaviour.Pushi8
                        or System.Reflection.Emit.StackBehaviour.Pushr4 or System.Reflection.Emit.StackBehaviour.Pushr8 or System.Reflection.Emit.StackBehaviour.Pushref => 1,
                    System.Reflection.Emit.StackBehaviour.Push1_push1 => 2,
                    _ => -1,
                };
                if (pops < 0 || pushes < 0 || stack.Count < pops) return false;
                // The object itself may only be stored into and passed to its base constructor.
                for (int index = 0; index < pops; index++) if (Pop() is SelfInitial) return false;
                for (int index = 0; index < pushes; index++) stack.Add(new UnknownInitial());
            }
            return ended;
        }

        // The field a token of a module names, if a type of the request declares it.
        private (TypeId, string)? Field(Module module, int token)
        {
            var handle = MetadataTokens.EntityHandle(token);
            var reader = module.Reader;
            if (handle.Kind == HandleKind.FieldDefinition)
            {
                var field = reader.GetFieldDefinition((FieldDefinitionHandle)handle);
                return (new TypeId(module, field.GetDeclaringType()), reader.GetString(field.Name));
            }
            if (handle.Kind != HandleKind.MemberReference) return null;
            var reference = reader.GetMemberReference((MemberReferenceHandle)handle);
            var owner = universe.Decode(module, reference.Parent) switch { NamedSig named => named, InstanceSig instance => instance.Generic, _ => null };
            return owner?.Type is { } type ? (type, reader.GetString(reference.Name)) : null;
        }

        private (string Name, Sig Owner, int Parameters, bool Instance, bool Returns)? Method(Module module, int token)
        {
            var handle = MetadataTokens.EntityHandle(token);
            var reader = module.Reader;
            if (handle.Kind == HandleKind.MethodSpecification) handle = reader.GetMethodSpecification((MethodSpecificationHandle)handle).Method;
            MethodSignature<Sig> signature; string name; Sig owner;
            if (handle.Kind == HandleKind.MethodDefinition)
            {
                var method = reader.GetMethodDefinition((MethodDefinitionHandle)handle);
                name = reader.GetString(method.Name); owner = universe.Decode(module, method.GetDeclaringType());
                signature = method.DecodeSignature(universe.Enter(module), null);
            }
            else if (handle.Kind == HandleKind.MemberReference)
            {
                var reference = reader.GetMemberReference((MemberReferenceHandle)handle);
                if (reference.GetKind() != MemberReferenceKind.Method || reference.Parent.Kind is not (HandleKind.TypeReference or HandleKind.TypeDefinition or HandleKind.TypeSpecification)) return null;
                name = reader.GetString(reference.Name); owner = universe.Decode(module, reference.Parent);
                signature = reference.DecodeMethodSignature(universe.Enter(module), null);
            }
            else return null;
            return (name, owner, signature.ParameterTypes.Length, signature.Header.IsInstance, signature.ReturnType is not PrimitiveSig { Code: PrimitiveTypeCode.Void });
        }
    }

    // A constant as the engine's text would hold it, for a field of a simple type.
    private static string? InitialText(object? constant, Dictionary<string, object> kind)
    {
        string keyword = kind["kind"] switch { "primitive" => (string)kind["name"], "enum" => (string)kind["underlying"], "string" => "string", _ => "" };
        var culture = System.Globalization.CultureInfo.InvariantCulture;
        switch (constant)
        {
            case string text when keyword == "string": return text;
            case long integer:
                return keyword switch
                {
                    "bool" => integer != 0 ? "1" : "0",
                    "byte" => ((byte)integer).ToString(culture), "sbyte" => ((sbyte)integer).ToString(culture),
                    "short" => ((short)integer).ToString(culture), "ushort" or "char" => ((ushort)integer).ToString(culture),
                    "int" => ((int)integer).ToString(culture), "uint" => ((uint)integer).ToString(culture),
                    "long" => integer.ToString(culture), "ulong" => ((ulong)integer).ToString(culture),
                    _ => null,
                };
            case double real:
                // The text has one name for every value that is not a number; only the one a
                // compiler writes for `NaN` is told by it.
                if (double.IsNaN(real) && BitConverter.DoubleToInt64Bits(real) != BitConverter.DoubleToInt64Bits(double.NaN)) return null;
                return keyword switch { "float" => ((float)real).ToString("R", culture), "double" => real.ToString("R", culture), _ => null };
            default: return null;
        }
    }

    private sealed class Reader
    {
        private readonly Universe universe;
        private readonly Construction construction;
        public Reader(Universe universe) { this.universe = universe; construction = new Construction(universe); }

        // The serialized fields a class declares itself, in declaration order.
        public void Fields(TypeId type, ImmutableArray<Sig> arguments, int depth, List<object> fields, HashSet<string> names, Dictionary<(TypeId, string), Initial>? initials)
        {
            var reader = type.Module.Reader;
            foreach (var handle in type.Definition.GetFields())
            {
                var field = reader.GetFieldDefinition(handle);
                const FieldAttributes never = FieldAttributes.Static | FieldAttributes.Literal | FieldAttributes.InitOnly | NotSerialized;
                if ((field.Attributes & never) != 0) continue;
                string name = reader.GetString(field.Name);
                bool attributed = false, hidden = false;
                var former = new List<string>();
                (string Element, int Length)? buffer = null;
                foreach (var (attribute, value) in Attributes(type.Module, field.GetCustomAttributes()))
                {
                    if (attribute == "UnityEngine.SerializeField") attributed = true;
                    else if (attribute == "UnityEngine.HideInInspector") hidden = true;
                    else if (attribute == "UnityEngine.SerializeReference")
                        throw new Unsupported($"field {name} is a managed reference");
                    else if (attribute == "UnityEngine.Serialization.FormerlySerializedAsAttribute")
                    {
                        var blob = reader.GetBlobReader(value.Value);
                        if (blob.ReadUInt16() != 1 || blob.ReadSerializedString() is not { } old)
                            throw new Unsupported($"field {name} has a former name the reader cannot decode");
                        former.Add(old);
                    }
                    else if (attribute == "System.Runtime.CompilerServices.FixedBufferAttribute")
                    {
                        var blob = reader.GetBlobReader(value.Value);
                        if (blob.ReadUInt16() != 1 || blob.ReadSerializedString() is not { } element)
                            throw new Unsupported($"fixed buffer {name} the reader cannot decode");
                        buffer = (element.Split(',')[0].Trim(), blob.ReadInt32());
                    }
                }
                if ((field.Attributes & FieldAttributes.FieldAccessMask) != FieldAttributes.Public && !attributed) continue;
                Dictionary<string, object>? kind;
                Sig? declared = null;
                if (buffer is { } fixedBuffer)
                {
                    // The element types a build was compared with.
                    string? keyword = fixedBuffer.Element switch
                    {
                        "System.Int32" => "int", "System.Byte" => "byte", "System.Single" => "float", "System.Boolean" => "bool",
                        "System.Char" => "char", "System.Int16" => "short", "System.Int64" => "long", "System.Double" => "double",
                        _ => null,
                    };
                    if (keyword is null || fixedBuffer.Length <= 0) throw new Unsupported($"fixed buffer {name} of {fixedBuffer.Element}");
                    kind = new() { ["kind"] = "fixedBuffer", ["element"] = Primitive(keyword), ["length"] = fixedBuffer.Length };
                }
                else
                {
                    var sig = Substitute(universe.Decode(type.Module, field), arguments);
                    declared = sig;
                    kind = Classify(sig, depth, attributed, type, false, name);
                }
                if (kind is null) continue;
                if (!names.Add(name)) throw new Unsupported($"two serialized fields are named {name}");
                var entry = new Dictionary<string, object> { ["name"] = name };
                if (hidden) entry["hidden"] = true;
                if (former.Count != 0) entry["formerNames"] = former;
                // What a document that lacks the field leaves in it. Nothing is said of a field the
                // constructor leaves alone, sets to null or to a plain new object of the field's own
                // type: the engine fills such a field the same way itself.
                Initial? initial = initials is null ? new UnknownInitial() : initials.GetValueOrDefault((type, name));
                switch (initial)
                {
                    case null: case ConstantInitial { Value: null }: break;
                    case ConstantInitial constant when InitialText(constant.Value, kind) is { } text: entry["initial"] = text; break;
                    case FreshInitial { Type: null } when (string)kind["kind"] == "array": break;
                    case FreshInitial { Type: { } made } when declared is not null && SameSig(made, declared) && (string)kind["kind"] is "array" or "struct": break;
                    default: entry["initialUnknown"] = true; break;
                }
                entry["type"] = kind;
                fields.Add(entry);
            }
        }

        private static Dictionary<string, object> Primitive(string keyword) => new() { ["kind"] = "primitive", ["name"] = keyword };

        // How the engine stores a field of this type; null if it does not store it.
        private Dictionary<string, object>? Classify(Sig sig, int depth, bool attributed, TypeId declaring, bool element, string field)
        {
            switch (sig)
            {
                case PrimitiveSig primitive:
                    if (Keyword(primitive.Code) is { } keyword) return Primitive(keyword);
                    return primitive.Code == PrimitiveTypeCode.String ? new() { ["kind"] = "string" } : null;
                case ArraySig array:
                    // An array of several dimensions is not stored.
                    return array.Vector ? Collection(array.Element, depth, declaring, element, field) : null;
                case InstanceSig { Generic.FullName: "System.Collections.Generic.List`1" } list when list.Arguments.Length == 1:
                    return Collection(list.Arguments[0], depth, declaring, element, field);
                case InstanceSig { Generic.FullName: "System.Collections.Generic.Dictionary`2" } dictionary when dictionary.Arguments.Length == 2:
                    // Only a dictionary marked with [SerializeField] is stored: as an array of its pairs.
                    if (!attributed) return null;
                    if (element || depth != 0) throw new Unsupported($"field {field} is a dictionary below the top level of the class");
                    var pair = new List<object>();
                    foreach (var (name, argument) in new[] { ("key", dictionary.Arguments[0]), ("value", dictionary.Arguments[1]) })
                    {
                        var part = Classify(argument, depth + 1, false, declaring, true, field)
                            ?? throw new Unsupported($"field {field} is a dictionary of a type that is not serialized");
                        pair.Add(new Dictionary<string, object> { ["name"] = name, ["type"] = part });
                    }
                    return new()
                    {
                        ["kind"] = "array",
                        ["element"] = new Dictionary<string, object> { ["kind"] = "struct", ["name"] = "SerializedKeyValue`2", ["fields"] = pair },
                    };
                case NamedSig named:
                    return Named(named, ImmutableArray<Sig>.Empty, depth, declaring, element, field);
                case InstanceSig instance:
                    return Named(instance.Generic, instance.Arguments, depth, declaring, element, field);
                default:
                    return null;
            }
        }

        private Dictionary<string, object>? Collection(Sig item, int depth, TypeId declaring, bool element, string field)
        {
            // A collection of collections is not stored.
            if (element) return null;
            var kind = Classify(item, depth, false, declaring, true, field);
            return kind is null ? null : new() { ["kind"] = "array", ["element"] = kind };
        }

        private Dictionary<string, object>? Named(NamedSig named, ImmutableArray<Sig> arguments, int depth, TypeId declaring, bool element, string field)
        {
            // Even a type the engine does not store has to be known: an enum
            // cannot be told from another struct without its definition.
            if (named.Type is not { } type)
                throw new Unsupported($"type {named.FullName} of field {field} is in none of the given assemblies");
            var definition = type.Definition;
            string root = Base(universe, type, arguments)?.FullName ?? "";
            if (root == "System.Enum")
            {
                foreach (var handle in definition.GetFields())
                {
                    var value = type.Module.Reader.GetFieldDefinition(handle);
                    if ((value.Attributes & FieldAttributes.Static) != 0) continue;
                    if (universe.Decode(type.Module, value) is not PrimitiveSig underlying || Keyword(underlying.Code) is not { } keyword)
                        break;
                    // The engine stores no enum of a 64-bit type.
                    if (keyword is "long" or "ulong") return null;
                    if (keyword is "byte" or "sbyte" or "short" or "ushort" or "int" or "uint")
                        return new() { ["kind"] = "enum", ["underlying"] = keyword };
                    break;
                }
                throw new Unsupported($"enum {named.FullName} has an underlying type the reader does not know");
            }
            // Nothing else of the base library is stored.
            if (named.FullName.StartsWith("System.", StringComparison.Ordinal)) return null;
            if ((definition.Attributes & TypeAttributes.Interface) != 0) return null;
            if (root is "System.MulticastDelegate" or "System.Delegate") return null;
            if (DerivesFrom(universe, type, "UnityEngine", "Object"))
            {
                if (arguments.Length != 0 || type.Nested) throw new Unsupported($"field {field} refers to a generic or nested engine object class");
                return new() { ["kind"] = "object", ["className"] = type.Name, ["namespace"] = type.Namespace };
            }
            if (type.Module.Engine)
            {
                if (EngineStructs.TryGetValue(named.FullName, out var known)) return new() { ["kind"] = "unity", ["name"] = known };
                if (EngineStructsNotSerialized.Contains(named.FullName)) return null;
                // An engine type may be stored by native code in a form its C# fields do not show.
                if (!PlainEngineType(named.FullName)) throw new Unsupported($"engine type {named.FullName} of field {field}");
            }
            if ((definition.Attributes & TypeAttributes.Abstract) != 0) return null;
            if ((definition.Attributes & Serializable) == 0) return null;
            if (Attributes(type.Module, definition.GetCustomAttributes()).Any(attribute => attribute.Name == "System.Runtime.CompilerServices.CompilerGeneratedAttribute"))
                return null;
            // A class does not store a field of its own type; a collection of it is stored.
            if (!element && type == declaring)
            {
                if (arguments.Length != 0) throw new Unsupported($"field {field} holds its own generic type");
                return null;
            }
            if (depth >= DepthLimit) throw new Unsupported($"field {field} nests serialized types up to the engine's depth limit");
            var fields = new List<object>();
            Chain(type, arguments, depth + 1, fields, false);
            return new() { ["kind"] = "struct", ["name"] = type.Name, ["fields"] = fields };
        }

        private static bool PlainEngineType(string name) =>
            name == "UnityEngine.Pose" || name.StartsWith("UnityEngine.Events.", StringComparison.Ordinal);

        // The fields of a class and of the classes it derives from, the most basic class first.
        // Returns whether a class of the chain overrides a state machine callback.
        public bool Chain(TypeId type, ImmutableArray<Sig> arguments, int depth, List<object> fields, bool behaviour, bool callbackOwner = true)
        {
            var chain = new List<(TypeId Type, ImmutableArray<Sig> Arguments)>();
            var current = (Type: type, Arguments: arguments);
            while (true)
            {
                if (chain.Count > 64) throw new Unsupported("a class hierarchy deeper than the reader follows");
                if (behaviour && current.Type.Module.Engine)
                {
                    // An engine class between the script and these may serialize fields of its own.
                    string root = Universe.FullName(current.Type);
                    if (root is not ("UnityEngine.StateMachineBehaviour" or "UnityEngine.ScriptableObject"))
                        throw new Unsupported($"type {Universe.FullName(type)} derives from engine type {root}");
                    break;
                }
                chain.Add(current);
                if (Base(universe, current.Type, current.Arguments) is not { } parent) break;
                if (parent.Type is not { } next)
                {
                    if (parent.FullName is "System.Object" or "System.ValueType") break;
                    throw new Unsupported($"type {Universe.FullName(type)} derives from {parent.FullName}");
                }
                if (!behaviour && next.Module.Engine && !PlainEngineType(parent.FullName))
                    throw new Unsupported($"type {Universe.FullName(type)} derives from engine type {parent.FullName}");
                if (parent.FullName.StartsWith("System.", StringComparison.Ordinal))
                    throw new Unsupported($"type {Universe.FullName(type)} derives from {parent.FullName}");
                current = (next, parent.Arguments);
            }
            chain.Reverse();
            var names = new HashSet<string>(StringComparer.Ordinal);
            bool callbacks = false;
            var initials = construction.Of(type);
            foreach (var (member, memberArguments) in chain)
            {
                Fields(member, memberArguments, depth, fields, names, initials);
                if (!behaviour || !callbackOwner) continue;
                var reader = member.Module.Reader;
                foreach (var handle in member.Definition.GetMethods())
                {
                    var method = reader.GetMethodDefinition(handle);
                    if (reader.GetString(method.Name) is not ("OnStateMachineEnter" or "OnStateMachineExit")) continue;
                    const MethodAttributes slot = MethodAttributes.Virtual | MethodAttributes.NewSlot;
                    if ((method.Attributes & slot) != MethodAttributes.Virtual)
                        throw new Unsupported($"method {reader.GetString(method.Name)} hides the engine's callback instead of overriding it");
                    callbacks = true;
                }
            }
            return callbacks;
        }
    }

    internal static Dictionary<string, object> Read(string requestPath)
    {
        var request = JsonSerializer.Deserialize<LayoutRequest>(File.ReadAllText(requestPath),
            new JsonSerializerOptions { PropertyNameCaseInsensitive = true }) ?? throw new InvalidOperationException("Empty layout request");
        if (request.Assemblies.Length == 0 || request.Assemblies.Length > 4096)
            throw new InvalidOperationException("Expected a bounded list of managed DLLs");
        var universe = new Universe(request.Assemblies);
        var result = new Dictionary<string, object>(StringComparer.Ordinal);
        var definitions = new Dictionary<Module, Dictionary<string, TypeDefinitionHandle>>();
        foreach (var script in request.Scripts)
        {
            if (universe.Find(script.Assembly) is not { } module) continue;
            if (!definitions.TryGetValue(module, out var byName))
            {
                byName = new(StringComparer.Ordinal);
                foreach (var handle in module.Reader.TypeDefinitions)
                    byName.TryAdd(Universe.FullName(new TypeId(module, handle)), handle);
                definitions.Add(module, byName);
            }
            if (!byName.TryGetValue(script.Type, out var found)) continue;
            var type = new TypeId(module, found);
            var definition = type.Definition;
            if (type.Nested || definition.GetGenericParameters().Count != 0 || (definition.Attributes & TypeAttributes.Abstract) != 0) continue;
            var layout = new Dictionary<string, object>
            {
                ["className"] = type.Name, ["namespace"] = type.Namespace, ["assembly"] = module.Name, ["baseKind"] = "StateMachineBehaviour",
            };
            try
            {
                bool behaviour = DerivesFrom(universe, type, "UnityEngine", "StateMachineBehaviour");
                if (!behaviour)
                {
                    if (!DerivesFrom(universe, type, "UnityEngine", "ScriptableObject")) continue;
                    layout["baseKind"] = "ScriptableObject";
                }
                var bases = new List<string>();
                for (var parent = Base(universe, type, ImmutableArray<Sig>.Empty); parent is { Type: { } next }; parent = Base(universe, next, ImmutableArray<Sig>.Empty))
                {
                    if (bases.Count > 64) throw new Unsupported("a class hierarchy deeper than the reader follows");
                    bases.Add(parent.Value.FullName);
                }
                layout["bases"] = bases;
                var fields = new List<object>();
                bool callbacks = new Reader(universe).Chain(type, ImmutableArray<Sig>.Empty, 0, fields, true, behaviour);
                if (behaviour) layout["overridesStateMachineCallbacks"] = callbacks;
                layout["fields"] = fields;
            }
            catch (Unsupported error)
            {
                layout.Remove("overridesStateMachineCallbacks");
                layout.Remove("bases");
                layout["unsupported"] = error.Message;
                layout["fields"] = Array.Empty<object>();
            }
            result[script.Guid] = layout;
        }
        return result;
    }
}

internal static class Program
{
    internal static readonly string[] Phases = { "Awake", "OnEnable", "OnDisable", "OnDestroy" };
    internal const string Lifecycle = "global::UnityMicroEditor.Raw.RawLifecycle";
    internal const string PrefabTransforms = "global::UnityMicroEditor.Raw.RawPrefabTransforms";

    public static int Main(string[] args)
    {
        try
        {
            if (args.Length >= 2 && args[0] == "--declaration-hashes")
            {
                Console.WriteLine(JsonSerializer.Serialize(DeclarationHash.Read(args.Skip(1).ToArray())));
                return 0;
            }
            if (args.Length >= 2 && (args[0] == "--type-definitions" || args[0] == "--public-type-definitions"))
            {
                Console.WriteLine(JsonSerializer.Serialize(DeclarationHash.TypeDefinitions(args.Skip(1).ToArray(), args[0] == "--public-type-definitions")));
                return 0;
            }
            if (args.Length == 2 && args[0] == "--behaviour-layouts")
            {
                Console.WriteLine(JsonSerializer.Serialize(BehaviourLayouts.Read(args[1])));
                return 0;
            }
            if (args.Length == 2 && args[0] == "--source-type-declarations")
            {
                Console.WriteLine(JsonSerializer.Serialize(ReadSourceDeclarations(args[1])));
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
            var marker = compilation.GetTypeByMetadataName("UnityMicroEditor.Raw.RawLifecycleInstrumentedAttribute");
            var api = compilation.GetTypeByMetadataName("UnityMicroEditor.Raw.RawLifecycle");
            if (marker is null || api is null || !api.GetMembers("ShouldCollect").Any() || !api.GetMembers("Enqueue").Any()
                || (activeAndEnabledReads != 0 && !api.GetMembers("IsActiveAndEnabled").Any()))
                throw new InvalidOperationException("Host lacks lifecycle barrier API/marker; prepare a matching universal host");
            if (setActiveCalls != 0 && !api!.GetMembers("SetActive").Any())
                throw new InvalidOperationException("Host lacks lifecycle SetActive bridge; prepare a matching universal host");
            var prefabTransforms = compilation.GetTypeByMetadataName("UnityMicroEditor.Raw.RawPrefabTransforms");
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
            var scriptableMarker = compilation.GetTypeByMetadataName("UnityMicroEditor.Raw.RawScriptableObjectInstrumentedAttribute");
            bool scriptableObjectBarrier = scriptableMarker is not null && api!.GetMembers("ShouldCollect").OfType<IMethodSymbol>().Any(m =>
                m.Parameters.Length == 2 && m.Parameters[0].Type.ToDisplayString() == "UnityEngine.ScriptableObject");
            if (scriptableObjectCallbacks != 0 && !scriptableObjectBarrier)
                throw new InvalidOperationException("Host lacks ScriptableObject lifecycle barrier API/marker; prepare a matching universal host");
            File.WriteAllText(request.MarkerPath,
                "[assembly: global::UnityMicroEditor.Raw.RawLifecycleInstrumentedAttribute(1)]\n" +
                (scriptableObjectBarrier ? "[assembly: global::UnityMicroEditor.Raw.RawScriptableObjectInstrumentedAttribute(1)]\n" : ""));
            foreach (var output in outputs)
            {
                Directory.CreateDirectory(System.IO.Path.GetDirectoryName(output.Source.OutputPath)!);
                File.WriteAllText(output.Source.OutputPath, output.Text);
            }
            Console.WriteLine(JsonSerializer.Serialize(new { SchemaVersion = 1, Verified = true,
                Callbacks = callbacks, Phases, ScriptableObjectBarrier = scriptableObjectBarrier, RuntimeInitializers = runtimeInitializers,
                ScriptTypes = scriptTypes,
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
            a.AttributeClass?.ToDisplayString() == "UnityMicroEditor.Raw." + (scriptable ? "RawScriptableObjectInstrumentedAttribute" : "RawLifecycleInstrumentedAttribute")
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
