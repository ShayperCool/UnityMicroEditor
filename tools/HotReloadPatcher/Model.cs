// Read-side model of one compiled assembly: names for types, members and signatures that do not
// depend on metadata tokens, so two compilations of the same source can be compared by meaning.
using System.Buffers.Binary;
using System.Collections.Concurrent;
using System.Collections.Immutable;
using System.Reflection;
using System.Reflection.Emit;
using System.Reflection.Metadata;
using System.Reflection.Metadata.Ecma335;
using System.Reflection.PortableExecutable;
using System.Text;

/// How a TypeDef takes part in the comparison.
internal enum GenKind
{
    /// Written by the user, or compiler-named but stable (fixed buffers): compared and referenced by name.
    User,
    /// Compiler-generated and owned by the methods that reach it (closure class, iterator/async state
    /// machine, anonymous type, dynamic call-site container): compared by structure.
    Exclusive,
    /// The per-type `<>c` class that pools every non-capturing lambda of a user type: only the
    /// members a method references belong to that method.
    Shared,
    /// `<PrivateImplementationDetails>`: content-addressed names, referenced member by member.
    Pid,
}

internal sealed class Asm : IDisposable
{
    public readonly string Path;
    public readonly PEReader PE;
    public readonly MetadataReader MD;
    public readonly string Name;
    public readonly Guid Mvid;
    public readonly Dictionary<string, TypeDefinitionHandle> Types = new(StringComparer.Ordinal);
    /// `Plain` scopes foreign types with their assembly (`[mscorlib]System.Action`); `Bare` does not,
    /// for member ids and for comparing a signature across two assemblies.
    public readonly SymbolProvider Plain, Bare;

    readonly string?[] typeName;
    readonly GenKind[] typeKind;
    readonly string?[] typeRefName, typeRefBare;
    readonly Dictionary<string, HashSet<string>> members = new(StringComparer.Ordinal);
    MetadataReaderProvider? pdb;
    bool pdbOpened;

    public Asm(string path, bool wholeImage = true)
        : this(path, File.OpenRead(path), wholeImage ? PEStreamOptions.PrefetchEntireImage : PEStreamOptions.PrefetchMetadata) { }

    public Asm(string path, Stream image, PEStreamOptions options = PEStreamOptions.PrefetchEntireImage)
    {
        Path = path;
        PE = new PEReader(image, options);
        if (!PE.HasMetadata) throw new InvalidDataException($"{path} has no managed metadata.");
        MD = PE.GetMetadataReader();
        if (!MD.IsAssembly) throw new InvalidDataException($"{path} is not an assembly.");
        Name = MD.GetString(MD.GetAssemblyDefinition().Name);
        Mvid = MD.GetGuid(MD.GetModuleDefinition().Mvid);
        int count = MD.TypeDefinitions.Count;
        typeName = new string[count + 1];
        typeKind = new GenKind[count + 1];
        typeRefName = new string[MD.TypeReferences.Count + 1];
        typeRefBare = new string[MD.TypeReferences.Count + 1];
        Plain = new SymbolProvider(this, bare: false);
        Bare = new SymbolProvider(this, bare: true);
        // Filled eagerly: the comparison workers read these tables concurrently.
        foreach (var h in MD.TypeDefinitions) { typeKind[MetadataTokens.GetRowNumber(h)] = ComputeKind(h); Types[TypeName(h)] = h; }
    }

    public void Dispose() { pdb?.Dispose(); PE.Dispose(); }

    /// The portable PDB next to the assembly, or null.
    public MetadataReader? Pdb
    {
        get
        {
            if (!pdbOpened)
            {
                pdbOpened = true;
                string file = System.IO.Path.ChangeExtension(Path, ".pdb");
                try { if (File.Exists(file)) pdb = MetadataReaderProvider.FromPortablePdbImage(ImmutableArray.Create(File.ReadAllBytes(file))); }
                catch (Exception e) when (e is BadImageFormatException or IOException) { pdb = null; }
            }
            try { return pdb?.GetMetadataReader(); }
            catch (BadImageFormatException) { return null; }
        }
    }

    public string TypeName(TypeDefinitionHandle h)
    {
        int row = MetadataTokens.GetRowNumber(h);
        if (typeName[row] is { } cached) return cached;
        var td = MD.GetTypeDefinition(h);
        string name = MD.GetString(td.Name);
        var outer = td.GetDeclaringType();
        if (!outer.IsNil) name = TypeName(outer) + "+" + name;
        else if (!td.Namespace.IsNil) name = MD.GetString(td.Namespace) + "." + name;
        return typeName[row] = name;
    }

    public string SimpleName(TypeDefinitionHandle h) => MD.GetString(MD.GetTypeDefinition(h).Name);

    public GenKind Kind(TypeDefinitionHandle h) => typeKind[MetadataTokens.GetRowNumber(h)];

    GenKind ComputeKind(TypeDefinitionHandle h)
    {
        var td = MD.GetTypeDefinition(h);
        string simple = MD.GetString(td.Name);
        var outer = td.GetDeclaringType();
        GenKind outerKind = outer.IsNil ? GenKind.User : ComputeKind(outer);
        if (outerKind == GenKind.Pid) return GenKind.Pid;
        if (outerKind != GenKind.User) return GenKind.Exclusive;
        if (simple == "<PrivateImplementationDetails>") return GenKind.Pid;
        if (simple.Length == 0 || simple[0] != '<' || simple == "<Module>") return GenKind.User;
        // `fixed int buffer[4]` declares the nested struct <buffer>e__FixedBuffer. It is the declared
        // type of a user field and carries no ordinal, so it keeps its identity like a user type.
        if (simple.EndsWith(">e__FixedBuffer", StringComparison.Ordinal)) return GenKind.User;
        if (simple == "<>c" || (simple.StartsWith("<>c__", StringComparison.Ordinal) && !simple.StartsWith("<>c__DisplayClass", StringComparison.Ordinal))) return GenKind.Shared;
        return GenKind.Exclusive;
    }

    public string TypeRefName(TypeReferenceHandle h, bool bare = false)
    {
        int row = MetadataTokens.GetRowNumber(h);
        var cache = bare ? typeRefBare : typeRefName;
        if (cache[row] is { } cached) return cached;
        var tr = MD.GetTypeReference(h);
        string name = MD.GetString(tr.Name);
        string full = tr.Namespace.IsNil ? name : MD.GetString(tr.Namespace) + "." + name;
        var scope = tr.ResolutionScope;
        full = scope.Kind switch
        {
            HandleKind.TypeReference => TypeRefName((TypeReferenceHandle)scope, bare) + "+" + name,
            HandleKind.AssemblyReference => bare ? full : "[" + MD.GetString(MD.GetAssemblyReference((AssemblyReferenceHandle)scope).Name) + "]" + full,
            HandleKind.ModuleReference => "[.module " + MD.GetString(MD.GetModuleReference((ModuleReferenceHandle)scope).Name) + "]" + full,
            _ => full, // ModuleDefinition: a TypeRef to a type of this very module
        };
        return cache[row] = full;
    }

    /// Name of the assembly a TypeRef resolves in; "" for a type of this module.
    public string TypeRefAssembly(TypeReferenceHandle h)
    {
        var scope = MD.GetTypeReference(h).ResolutionScope;
        return scope.Kind switch
        {
            HandleKind.TypeReference => TypeRefAssembly((TypeReferenceHandle)scope),
            HandleKind.AssemblyReference => MD.GetString(MD.GetAssemblyReference((AssemblyReferenceHandle)scope).Name),
            _ => "",
        };
    }

    public bool IsValueType(TypeDefinitionHandle h)
    {
        var baseType = MD.GetTypeDefinition(h).BaseType;
        if (baseType.IsNil) return false;
        string name = baseType.Kind switch
        {
            HandleKind.TypeReference => TypeRefName((TypeReferenceHandle)baseType, bare: true),
            HandleKind.TypeDefinition => TypeName((TypeDefinitionHandle)baseType),
            _ => "",
        };
        return name is "System.ValueType" or "System.Enum";
    }

    public bool IsEnum(TypeDefinitionHandle h)
    {
        var baseType = MD.GetTypeDefinition(h).BaseType;
        return baseType.Kind == HandleKind.TypeReference && TypeRefName((TypeReferenceHandle)baseType, bare: true) == "System.Enum";
    }

    public bool IsInterface(TypeDefinitionHandle h) => (MD.GetTypeDefinition(h).Attributes & TypeAttributes.Interface) != 0;

    /// The state machine of an iterator or an async method, by what it implements: the name differs
    /// between a method (`<M>d__5`), a lambda (`<<M>b__5_0>d`) and a local function.
    public bool IsStateMachine(TypeDefinitionHandle h)
    {
        foreach (var i in MD.GetTypeDefinition(h).GetInterfaceImplementations())
            if (MD.GetInterfaceImplementation(i).Interface is { Kind: HandleKind.TypeReference } reference &&
                TypeRefName((TypeReferenceHandle)reference, bare: true) is "System.Runtime.CompilerServices.IAsyncStateMachine" or "System.Collections.IEnumerator") return true;
        return false;
    }

    /// The class or struct that holds the captured variables of one scope.
    public bool IsClosure(TypeDefinitionHandle h) => SimpleName(h).StartsWith("<>c__DisplayClass", StringComparison.Ordinal);

    /// A type with generic parameters of its own or of an enclosing type.
    public bool IsGenericType(TypeDefinitionHandle h)
    {
        for (var t = h; !t.IsNil; t = MD.GetTypeDefinition(t).GetDeclaringType())
            if (MD.GetTypeDefinition(t).GetGenericParameters().Count > 0) return true;
        return false;
    }

    public static string SigText(MethodSignature<string> s)
    {
        var sb = new StringBuilder();
        if (s.Header.CallingConvention != SignatureCallingConvention.Default) sb.Append(' ').Append(s.Header.CallingConvention).Append(' ');
        if (s.GenericParameterCount > 0) sb.Append('`').Append(s.GenericParameterCount);
        sb.Append('(');
        for (int i = 0; i < s.ParameterTypes.Length; i++)
        {
            if (i > 0) sb.Append(',');
            if (i == s.RequiredParameterCount) sb.Append("...,");
            sb.Append(s.ParameterTypes[i]);
        }
        sb.Append("):").Append(s.ReturnType);
        // `static` is part of the identity: a static toggle is a removed and an added method.
        if (!s.Header.IsInstance) sb.Append(" static");
        return sb.ToString();
    }

    /// Identity of a method inside its type across compilations: name, static or instance,
    /// generic arity, parameter types and return type (conversion operators overload on it).
    public string MethodKey(MethodDefinitionHandle h)
    {
        var d = MD.GetMethodDefinition(h);
        return MD.GetString(d.Name) + SigText(d.DecodeSignature(Plain, null));
    }

    /// The same identity without assembly scopes, comparable between two assemblies.
    public string BareMethodKey(MethodDefinitionHandle h)
    {
        var d = MD.GetMethodDefinition(h);
        return MD.GetString(d.Name) + SigText(d.DecodeSignature(Bare, null));
    }

    /// Member id of the result document: `Namespace.Type+Nested::Method(ParamType,...)`.
    public string MemberId(MethodDefinitionHandle h)
    {
        var d = MD.GetMethodDefinition(h);
        string name = MD.GetString(d.Name);
        var s = d.DecodeSignature(Bare, null);
        string id = TypeName(d.GetDeclaringType()) + "::" + name + (s.GenericParameterCount > 0 ? "`" + s.GenericParameterCount : "") + "(" + string.Join(",", s.ParameterTypes) + ")";
        return name is "op_Implicit" or "op_Explicit" ? id + ":" + s.ReturnType : id;
    }

    public string FieldId(FieldDefinitionHandle h)
    {
        var d = MD.GetFieldDefinition(h);
        return TypeName(d.GetDeclaringType()) + "::" + MD.GetString(d.Name);
    }

    public string HandleText(EntityHandle h, bool bare = false) => h.IsNil ? "" : h.Kind switch
    {
        HandleKind.TypeDefinition => TypeName((TypeDefinitionHandle)h),
        HandleKind.TypeReference => TypeRefName((TypeReferenceHandle)h, bare),
        HandleKind.TypeSpecification => MD.GetTypeSpecification((TypeSpecificationHandle)h).DecodeSignature(bare ? Bare : Plain, null),
        _ => h.Kind.ToString(),
    };

    public string AttributeTypeName(CustomAttribute ca) => ca.Constructor.Kind switch
    {
        HandleKind.MemberReference => HandleText(MD.GetMemberReference((MemberReferenceHandle)ca.Constructor).Parent, bare: true),
        HandleKind.MethodDefinition => TypeName(MD.GetMethodDefinition((MethodDefinitionHandle)ca.Constructor).GetDeclaringType()),
        _ => "?",
    };

    /// Whether the type `typeName` of this assembly declares the member. `key` is a name followed by
    /// a bare method signature, or by `:` and a bare field type.
    public bool HasMember(string typeName, string key)
    {
        lock (members)
        {
            if (!members.TryGetValue(typeName, out var set))
            {
                members[typeName] = set = new HashSet<string>(StringComparer.Ordinal);
                if (Types.TryGetValue(typeName, out var t))
                {
                    var td = MD.GetTypeDefinition(t);
                    foreach (var m in td.GetMethods()) set.Add(BareMethodKey(m));
                    foreach (var f in td.GetFields()) set.Add(BareFieldKey(f));
                }
            }
            return set.Contains(key);
        }
    }

    public string BareFieldKey(FieldDefinitionHandle f)
    {
        var d = MD.GetFieldDefinition(f);
        return MD.GetString(d.Name) + ":" + d.DecodeSignature(Bare, null) + ((d.Attributes & FieldAttributes.Static) != 0 ? " static" : "");
    }
}

/// Renders signature types as text. `DefHook` lets a caller substitute compiler-generated TypeDefs.
internal sealed class SymbolProvider : ISignatureTypeProvider<string, object?>
{
    readonly Asm a;
    readonly bool bare;
    public Func<TypeDefinitionHandle, string>? DefHook;
    public SymbolProvider(Asm a, bool bare) { this.a = a; this.bare = bare; }

    public string GetPrimitiveType(PrimitiveTypeCode c) => c switch
    {
        PrimitiveTypeCode.Void => "System.Void",
        PrimitiveTypeCode.Boolean => "System.Boolean",
        PrimitiveTypeCode.Char => "System.Char",
        PrimitiveTypeCode.SByte => "System.SByte",
        PrimitiveTypeCode.Byte => "System.Byte",
        PrimitiveTypeCode.Int16 => "System.Int16",
        PrimitiveTypeCode.UInt16 => "System.UInt16",
        PrimitiveTypeCode.Int32 => "System.Int32",
        PrimitiveTypeCode.UInt32 => "System.UInt32",
        PrimitiveTypeCode.Int64 => "System.Int64",
        PrimitiveTypeCode.UInt64 => "System.UInt64",
        PrimitiveTypeCode.Single => "System.Single",
        PrimitiveTypeCode.Double => "System.Double",
        PrimitiveTypeCode.String => "System.String",
        PrimitiveTypeCode.TypedReference => "System.TypedReference",
        PrimitiveTypeCode.IntPtr => "System.IntPtr",
        PrimitiveTypeCode.UIntPtr => "System.UIntPtr",
        PrimitiveTypeCode.Object => "System.Object",
        _ => c.ToString(),
    };
    public string GetTypeFromDefinition(MetadataReader r, TypeDefinitionHandle h, byte rawTypeKind) => DefHook != null ? DefHook(h) : a.TypeName(h);
    public string GetTypeFromReference(MetadataReader r, TypeReferenceHandle h, byte rawTypeKind) => a.TypeRefName(h, bare);
    public string GetTypeFromSpecification(MetadataReader r, object? ctx, TypeSpecificationHandle h, byte rawTypeKind) => r.GetTypeSpecification(h).DecodeSignature(this, ctx);
    public string GetSZArrayType(string e) => e + "[]";
    public string GetArrayType(string e, ArrayShape s) => e + "[" + new string(',', s.Rank - 1) + (s.LowerBounds.Length + s.Sizes.Length > 0 ? ";" + string.Join(" ", s.LowerBounds) + ";" + string.Join(" ", s.Sizes) : "") + "]";
    public string GetByReferenceType(string e) => e + "&";
    public string GetPointerType(string e) => e + "*";
    public string GetPinnedType(string e) => e + " pinned";
    public string GetGenericInstantiation(string g, ImmutableArray<string> args) => g + "<" + string.Join(",", args) + ">";
    public string GetGenericTypeParameter(object? ctx, int i) => "!" + i;
    public string GetGenericMethodParameter(object? ctx, int i) => "!!" + i;
    public string GetFunctionPointerType(MethodSignature<string> s) => "fnptr" + Asm.SigText(s);
    public string GetModifiedType(string modifier, string unmodified, bool isRequired) => unmodified + (isRequired ? " modreq(" : " modopt(") + modifier + ")";
}

/// IL instruction walker. Operand kinds come from System.Reflection.Emit.OpCodes, so no opcode table is hand-written.
internal static class IL
{
    static readonly byte[] one = new byte[256], two = new byte[256]; // OperandType + 1; 0 = unknown opcode

    static IL()
    {
        foreach (var f in typeof(OpCodes).GetFields(BindingFlags.Public | BindingFlags.Static))
        {
            if (f.GetValue(null) is not OpCode op) continue;
            ushort v = unchecked((ushort)op.Value);
            if (op.Size == 1) one[v] = (byte)((int)op.OperandType + 1);
            else two[v & 0xFF] = (byte)((int)op.OperandType + 1);
        }
    }

    public const ushort Call = 0x28, Callvirt = 0x6F, Ldfld = 0x7B, Ldflda = 0x7C, Stfld = 0x7D,
        Ldsfld = 0x7E, Ldsflda = 0x7F, Stsfld = 0x80, Ldftn = 0xFE06,
        Localloc = 0xFE0F, Unaligned = 0xFE12, Volatile = 0xFE13, Constrained = 0xFE16;

    public static bool IsToken(OperandType t) => t is OperandType.InlineField or OperandType.InlineMethod or OperandType.InlineSig
        or OperandType.InlineString or OperandType.InlineTok or OperandType.InlineType;

    public ref struct Reader
    {
        readonly ReadOnlySpan<byte> il;
        int pos;
        public int Offset, OperandOffset, Next;
        public ushort OpCode, Previous;
        public OperandType Operand;

        public Reader(ReadOnlySpan<byte> il) { this.il = il; pos = 0; Offset = 0; OperandOffset = 0; Next = 0; OpCode = 0; Previous = 0; Operand = default; }

        public bool Read()
        {
            if (pos >= il.Length) return false;
            Previous = OpCode;
            Offset = pos;
            byte b = il[pos++], k;
            if (b == 0xFE)
            {
                if (pos >= il.Length) throw new InvalidDataException($"IL ends inside an opcode at IL_{Offset:X4}.");
                byte b2 = il[pos++];
                OpCode = (ushort)(0xFE00 | b2);
                k = two[b2];
            }
            else { OpCode = b; k = one[b]; }
            if (k == 0) throw new InvalidDataException($"Unknown opcode 0x{OpCode:X} at IL_{Offset:X4}.");
            Operand = (OperandType)(k - 1);
            OperandOffset = pos;
            int size = Operand switch
            {
                OperandType.InlineNone => 0,
                OperandType.ShortInlineBrTarget or OperandType.ShortInlineI or OperandType.ShortInlineVar => 1,
                OperandType.InlineVar => 2,
                OperandType.InlineI8 or OperandType.InlineR => 8,
                OperandType.InlineSwitch => pos + 4 <= il.Length ? 4 + 4 * BinaryPrimitives.ReadInt32LittleEndian(il[pos..]) : 4,
                _ => 4,
            };
            if (size < 0 || pos + size > il.Length) throw new InvalidDataException($"IL ends inside the operand at IL_{Offset:X4}.");
            pos += size;
            Next = pos;
            return true;
        }
    }
}

/// One game assembly of a request: what the Player loaded and what was just compiled.
internal sealed class Entry
{
    public readonly string Name;
    public readonly Asm Loaded, Current;
    public readonly bool Netcode;

    public Entry(string name, Asm loaded, Asm current, bool netcode) { Name = name; Loaded = loaded; Current = current; Netcode = netcode; }
}

/// Every assembly of a request, and the assemblies next to them for walking base chains.
internal sealed class Workspace : IDisposable
{
    public readonly List<Entry> Entries = new();
    readonly ConcurrentDictionary<string, Asm?> siblings = new(StringComparer.Ordinal);

    public Entry? Find(string assemblyName)
    {
        foreach (var e in Entries) if (e.Name == assemblyName) return e;
        return null;
    }

    /// Metadata of a referenced assembly: the current DLL of a request entry, else the file of that
    /// name next to a loaded DLL (the Player's Managed directory holds what the Player runs, and an
    /// assembly that is not part of the request did not change). Null when it cannot be read.
    public Asm? Resolve(string assemblyName)
    {
        if (Find(assemblyName) is { } entry) return entry.Current;
        return siblings.GetOrAdd(assemblyName, name =>
        {
            foreach (var e in Entries)
            {
                string file = System.IO.Path.Combine(System.IO.Path.GetDirectoryName(e.Loaded.Path) ?? "", name + ".dll");
                if (!File.Exists(file)) continue;
                try { return new Asm(file, wholeImage: false); }
                catch (Exception error) when (error is IOException or BadImageFormatException or InvalidDataException or UnauthorizedAccessException) { return null; }
            }
            return null;
        });
    }

    public void Dispose()
    {
        foreach (var e in Entries) { e.Loaded.Dispose(); e.Current.Dispose(); }
        foreach (var a in siblings.Values) a?.Dispose();
    }
}
