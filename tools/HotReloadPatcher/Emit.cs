// Emits the patch assembly with System.Reflection.Metadata.Ecma335.MetadataBuilder.
//
// A method body is copied byte for byte. Only its metadata tokens are rewritten, in place: every
// token operand is 4 bytes wide, so no instruction moves, and branch targets, switch tables,
// exception regions and the sequence points of the PDB stay valid. An instance method of a class
// becomes a static method whose first parameter is the receiver: `ldarg.0` already means "the
// receiver" in both, so that needs no IL change either.
using System.Buffers.Binary;
using System.Reflection;
using System.Reflection.Emit;
using System.Reflection.Metadata;
using System.Reflection.Metadata.Ecma335;
using System.Reflection.PortableExecutable;
using System.Security.Cryptography;

/// Copies an ECMA-335 signature blob, mapping every TypeDefOrRefOrSpec token on the way.
internal sealed class SigCopier
{
    readonly Func<EntityHandle, EntityHandle> map;
    public SigCopier(Func<EntityHandle, EntityHandle> map) => this.map = map;

    void Token(ref BlobReader r, BlobBuilder w) => w.WriteCompressedInteger(CodedIndex.TypeDefOrRefOrSpec(map(r.ReadTypeHandle())));

    public void Type(ref BlobReader r, BlobBuilder w)
    {
        byte b = r.ReadByte();
        w.WriteByte(b);
        switch (b)
        {
            case 0x01: case 0x02: case 0x03: case 0x04: case 0x05: case 0x06: case 0x07: case 0x08: case 0x09: case 0x0A:
            case 0x0B: case 0x0C: case 0x0D: case 0x0E: case 0x16: case 0x18: case 0x19: case 0x1C:
                break;
            case 0x0F: case 0x10: case 0x1D: case 0x45: case 0x41: // PTR, BYREF, SZARRAY, PINNED, SENTINEL
                Type(ref r, w);
                break;
            case 0x11: case 0x12: // VALUETYPE, CLASS
                Token(ref r, w);
                break;
            case 0x13: case 0x1E: // VAR, MVAR
                w.WriteCompressedInteger(r.ReadCompressedInteger());
                break;
            case 0x14: // ARRAY type rank numSizes size* numLoBounds loBound*
            {
                Type(ref r, w);
                w.WriteCompressedInteger(r.ReadCompressedInteger());
                int sizes = r.ReadCompressedInteger();
                w.WriteCompressedInteger(sizes);
                for (int i = 0; i < sizes; i++) w.WriteCompressedInteger(r.ReadCompressedInteger());
                int bounds = r.ReadCompressedInteger();
                w.WriteCompressedInteger(bounds);
                for (int i = 0; i < bounds; i++) w.WriteCompressedSignedInteger(r.ReadCompressedSignedInteger());
                break;
            }
            case 0x15: // GENERICINST (CLASS|VALUETYPE) token count type*
            {
                w.WriteByte(r.ReadByte());
                Token(ref r, w);
                int count = r.ReadCompressedInteger();
                w.WriteCompressedInteger(count);
                for (int i = 0; i < count; i++) Type(ref r, w);
                break;
            }
            case 0x1B: // FNPTR
                Method(ref r, w);
                break;
            case 0x1F: case 0x20: // CMOD_REQD, CMOD_OPT token, then the modified type
                Token(ref r, w);
                Type(ref r, w);
                break;
            default:
                throw new Unsupported(Codes.EmitNotSupported, $"signature element 0x{b:X2} is not handled");
        }
    }

    public void Method(ref BlobReader r, BlobBuilder w)
    {
        byte header = r.ReadByte();
        w.WriteByte(header);
        if ((header & 0x10) != 0) w.WriteCompressedInteger(r.ReadCompressedInteger());
        int count = r.ReadCompressedInteger();
        w.WriteCompressedInteger(count);
        Type(ref r, w);
        for (int i = 0; i < count; i++) Type(ref r, w);
    }

    /// Any blob that starts with a calling-convention byte: method, field, property, locals, method instantiation.
    public BlobBuilder Signature(BlobReader r)
    {
        var w = new BlobBuilder(Math.Max(16, r.Length + 8));
        byte header = r.ReadByte();
        switch (header & 0x0F)
        {
            case 0x06: // FIELD
                w.WriteByte(header);
                Type(ref r, w);
                break;
            case 0x07: // LOCAL_SIG
            case 0x0A: // GENERICINST (MethodSpec)
            {
                w.WriteByte(header);
                int count = r.ReadCompressedInteger();
                w.WriteCompressedInteger(count);
                for (int i = 0; i < count; i++) Type(ref r, w);
                break;
            }
            default:
                r.Offset = 0;
                Method(ref r, w);
                break;
        }
        return w;
    }

    public BlobBuilder TypeSpec(BlobReader r)
    {
        var w = new BlobBuilder(Math.Max(16, r.Length + 8));
        Type(ref r, w);
        return w;
    }
}

internal sealed record Detour(string Member, string Kind, int OriginalToken, int ReplacementToken);
internal sealed record AddedMethod(string Member, int Token);
internal sealed record AddedField(string Member, string Id, bool Static);
/// A method that only the patch has: nothing redirects into it, so code that holds it (a delegate,
/// a running state machine) keeps it when a later patch brings another version.
internal sealed record PatchMember(string Member, string Kind, string BodyHash);

/// The emitted patch of one assembly.
internal sealed class Patch
{
    public string AssemblyName = "";
    public byte[] Image = Array.Empty<byte>();
    public byte[]? Pdb;
    public readonly List<Detour> Detours = new();
    public readonly List<AddedMethod> AddedMethods = new();
    public readonly List<AddedField> AddedFields = new();
    public readonly List<PatchMember> Members = new();
    /// Added methods that patched code makes a delegate of.
    public readonly List<string> DelegatedAdded = new();
}

internal sealed class Emitter
{
    public const string RuntimeNamespace = "UnityMicroEditor.HotReload";

    enum MKind { Copy, Stub, FieldGet, FieldSet, FieldRef }

    sealed class PMethod
    {
        public MKind Kind;
        /// Copy: the method of the new compilation. Stub: nil.
        public MethodDefinitionHandle Source;
        /// Stub: the removed method of the loaded assembly whose shape the stub repeats.
        public MethodDefinitionHandle Removed;
        /// Instance method of a class of the game assembly, emitted as static with the receiver as first parameter.
        public bool Lower;
        /// Instance method of a struct of the game assembly, emitted as an instance method of a stand-in
        /// struct of the patch. See Root().
        public bool StandIn;
        /// Callers inside the patch call this copy. False for a changed method that exists in the
        /// loaded assembly: callers keep calling the original, which the Player redirects.
        public bool Direct;
        public int Row, LocalsRow;
        /// Bytes of the null check in front of the copied instructions.
        public int Lead;
        public string Name = "";
        /// Member id of the user method this one was reached from; its own id for a user method.
        public string Origin = "";
        public SideField? Field;
        public PType Owner = null!;
    }

    sealed class PType
    {
        public string Ns = "", Name = "";
        public TypeDefinitionHandle Source;
        public bool Whole, Holder, ValueHolder;
        public readonly List<FieldDefinitionHandle> Fields = new();
        public readonly List<PMethod> Methods = new();
        public int Row;
        public string FullName => Ns.Length == 0 ? Name : Ns + "." + Name;
    }

    /// A field the loaded type lacks; its storage is the runtime side table.
    sealed class SideField
    {
        public FieldDefinitionHandle Source;
        public string Id = "", Member = "";
        public bool IsStatic;
        public PMethod Get = null!, Set = null!, Ref = null!;
    }

    sealed class Plan
    {
        public readonly Dictionary<int, PType> Types = new();
        public readonly Dictionary<int, PType> Holders = new();
        public readonly Dictionary<int, PMethod> Methods = new();
        public readonly Dictionary<int, PMethod> Stubs = new();
        public readonly Dictionary<int, SideField> Accessors = new();
        public readonly List<PType> Order = new();
        public readonly Queue<(MethodDefinitionHandle Method, string Origin)> Scan = new();
        public readonly SortedSet<string> Delegated = new(StringComparer.Ordinal);
    }

    /// A compiler-generated type of the new compilation that is identical to one the Player loaded.
    sealed class Twin
    {
        public TypeDefinitionHandle Loaded;
        public readonly Dictionary<string, string> Methods = new(StringComparer.Ordinal), Fields = new(StringComparer.Ordinal);
    }

    readonly Workspace workspace;
    readonly Entry entry;
    readonly Asm o, n;
    readonly MetadataReader md;
    readonly Canon co, cn;
    readonly string patchName, patchNamespace, runtimeAssembly;
    readonly HashSet<int> addedMethods = new(), addedFields = new(), checkedRefs = new();
    readonly Dictionary<int, string> refused = new();
    readonly Dictionary<int, Twin?> twins = new();
    readonly Dictionary<int, string> loadedHashes = new();
    readonly SigCopier scanner, copier, loadedCopier;
    Plan plan = new();
    /// The user method whose code is being scanned.
    string origin = "";

    // Emission state.
    MetadataBuilder mb = null!;
    BlobBuilder ilStream = null!, mapped = null!;
    MethodBodyStreamEncoder bodies;
    AssemblyReferenceHandle originalRef;
    EntityHandle corlibScope;
    readonly Dictionary<string, AssemblyReferenceHandle> asmRefs = new(StringComparer.Ordinal);
    readonly Dictionary<string, TypeReferenceHandle> typeRefs = new(StringComparer.Ordinal);
    readonly Dictionary<int, EntityHandle> mappedTokens = new(), mappedLoaded = new();
    readonly Dictionary<(int Parent, string Name, BlobHandle Sig), MemberReferenceHandle> memberRefs = new();
    readonly Dictionary<int, int> fieldRows = new();

    public Emitter(Workspace workspace, Entry entry, int generation, string runtimeAssembly)
    {
        this.workspace = workspace;
        this.entry = entry;
        this.runtimeAssembly = runtimeAssembly;
        o = entry.Loaded; n = entry.Current; md = n.MD;
        co = new Canon(o); cn = new Canon(n);
        patchName = entry.Name + ".HotReload." + generation;
        // No type of a patch may carry a full name that a loaded assembly has, and two generations
        // must not share one either: by-name lookups take the first loaded match.
        patchNamespace = "HotReload.G" + generation;
        scanner = new SigCopier(h => { Visit(h); return h; });
        copier = new SigCopier(Map);
        loadedCopier = new SigCopier(MapLoaded);
    }

    static int Tok(EntityHandle h) => MetadataTokens.GetToken(h);

    // ---- planning: what must exist in the patch ----------------------------------------------

    string Namespace(TypeDefinitionHandle t)
    {
        while (true)
        {
            var td = md.GetTypeDefinition(t);
            var outer = td.GetDeclaringType();
            if (outer.IsNil) return td.Namespace.IsNil ? patchNamespace : patchNamespace + "." + md.GetString(td.Namespace);
            t = outer;
        }
    }

    string Flat(TypeDefinitionHandle t)
    {
        var td = md.GetTypeDefinition(t);
        var outer = td.GetDeclaringType();
        string name = md.GetString(td.Name);
        return outer.IsNil ? name : Flat(outer) + "__" + name;
    }

    PType Holder(TypeDefinitionHandle t)
    {
        int tok = Tok(t);
        if (plan.Holders.TryGetValue(tok, out var h)) return h;
        h = new PType { Holder = true, ValueHolder = n.IsValueType(t), Source = t, Ns = Namespace(t), Name = Flat(t) };
        plan.Holders[tok] = h;
        plan.Order.Add(h);
        return h;
    }

    static string LoweredName(string name) => name switch { ".ctor" => "__ctor", ".cctor" => "__cctor", _ => name };

    /// A method of an existing user type that the patch carries: a changed body (direct = false),
    /// or an added or compiler-generated method (direct = true).
    void Root(MethodDefinitionHandle m, bool direct)
    {
        int tok = Tok(m);
        if (plan.Methods.ContainsKey(tok)) return;
        var d = md.GetMethodDefinition(m);
        var t = d.GetDeclaringType();
        string name = md.GetString(d.Name);
        bool owned = Canon.Owned(name, d.Attributes);
        if (d.GetGenericParameters().Count > 0 || n.IsGenericType(t))
            throw new Unsupported(owned ? Codes.GenericGeneratedMember : Codes.GenericMethod, $"{n.MemberId(m)} is generic or belongs to a generic type");
        if (d.RelativeVirtualAddress == 0) throw new Unsupported(Codes.EmitNotSupported, $"{n.MemberId(m)} has no IL body");
        if (n.IsInterface(t)) throw new Unsupported(Codes.InterfaceChanged, $"{n.MemberId(m)} is a member of an interface");
        // Callers inside the patch reach a direct copy with `call`: a virtual method would lose its dispatch.
        if (direct && (d.Attributes & MethodAttributes.Virtual) != 0)
            throw new Unsupported(Codes.EmitNotSupported, $"{n.MemberId(m)} is virtual and cannot be copied into a patch");
        if (VariableArguments(md.GetBlobReader(d.Signature)))
            throw new Unsupported(Codes.EmitNotSupported, $"{n.MemberId(m)} takes a variable argument list");
        var holder = Holder(t);
        bool instance = (d.Attributes & MethodAttributes.Static) == 0;
        // A class receiver becomes the first parameter of a static method: Mono passes `this` and a
        // leading object reference the same way, so the original can jump straight to the replacement.
        // A struct receiver is a managed pointer, and for a method that returns a struct through a
        // hidden buffer Mono (x86-64) orders [this, buffer] for an instance method but [buffer, pointer]
        // for a static one. So the replacement stays an INSTANCE method, declared on a stand-in struct:
        // identical calling convention by construction, and the body only ever uses `this` through
        // tokens of the original struct.
        var pm = new PMethod
        {
            Kind = MKind.Copy, Source = m, Direct = direct, Owner = holder,
            Lower = instance && !holder.ValueHolder,
            StandIn = instance && holder.ValueHolder,
            Name = LoweredName(name),
            Origin = owned ? origin : n.MemberId(m),
        };
        holder.Methods.Add(pm);
        plan.Methods[tok] = pm;
        scanner.Signature(md.GetBlobReader(d.Signature));
        plan.Scan.Enqueue((m, pm.Origin));
    }

    static bool VariableArguments(BlobReader signature)
    {
        var header = signature.ReadSignatureHeader();
        return header.Kind == SignatureKind.Method && header.CallingConvention == SignatureCallingConvention.VarArgs;
    }

    /// An empty method with the shape of a removed message of the loaded assembly.
    void Stub(MethodDefinitionHandle removed, TypeDefinitionHandle type)
    {
        int tok = Tok(removed);
        if (plan.Stubs.ContainsKey(tok)) return;
        var holder = Holder(type);
        if (holder.ValueHolder) throw new Unsupported(Codes.UnityMessageRemoved, "a struct receives no Unity messages");
        var pm = new PMethod { Kind = MKind.Stub, Removed = removed, Direct = true, Lower = true, Owner = holder, Name = o.MD.GetString(o.MD.GetMethodDefinition(removed).Name) };
        holder.Methods.Add(pm);
        plan.Stubs[tok] = pm;
    }

    string LoadedHash(TypeDefinitionHandle t)
    {
        int tok = Tok(t);
        if (!loadedHashes.TryGetValue(tok, out var hash)) loadedHashes[tok] = hash = co.TypeHash(t);
        return hash;
    }

    /// Anonymous types and generic closures have an identity that code on both sides of a patch
    /// shares (an object made by old code is read by new code), and a generic type cannot be
    /// copied without its instantiations. So the patch refers to the loaded type of the same
    /// structure, whatever number the compiler gave it this time, or the edit is refused.
    Twin? FindTwin(TypeDefinitionHandle t)
    {
        int tok = Tok(t);
        if (twins.TryGetValue(tok, out var known)) return known;
        string wanted = cn.TypeHash(t);
        var outer = md.GetTypeDefinition(t).GetDeclaringType();
        IEnumerable<TypeDefinitionHandle> candidates = Array.Empty<TypeDefinitionHandle>();
        if (outer.IsNil) candidates = o.MD.TypeDefinitions.Where(h => o.MD.GetTypeDefinition(h).GetDeclaringType().IsNil);
        else if (o.Types.TryGetValue(n.TypeName(outer), out var loadedOuter)) candidates = o.MD.GetTypeDefinition(loadedOuter).GetNestedTypes();
        Twin? twin = null;
        foreach (var candidate in candidates)
        {
            if (o.Kind(candidate) != GenKind.Exclusive || LoadedHash(candidate) != wanted) continue;
            twin = new Twin { Loaded = candidate };
            // Equal structure means equal member order; generated member names may carry other ordinals.
            var newType = md.GetTypeDefinition(t);
            var oldType = o.MD.GetTypeDefinition(candidate);
            foreach (var (x, y) in newType.GetMethods().Zip(oldType.GetMethods()))
                twin.Methods[md.GetString(md.GetMethodDefinition(x).Name)] = o.MD.GetString(o.MD.GetMethodDefinition(y).Name);
            foreach (var (x, y) in newType.GetFields().Zip(oldType.GetFields()))
                twin.Fields[md.GetString(md.GetFieldDefinition(x).Name)] = o.MD.GetString(o.MD.GetFieldDefinition(y).Name);
            break;
        }
        return twins[tok] = twin;
    }

    void NeedType(TypeDefinitionHandle t)
    {
        int tok = Tok(t);
        if (plan.Types.ContainsKey(tok)) return;
        switch (n.Kind(t))
        {
            case GenKind.User:
                if (!o.Types.ContainsKey(n.TypeName(t))) throw new Unsupported(Codes.TypeAdded, $"it uses type {n.TypeName(t)}, which the loaded assembly lacks");
                return; // referenced in the loaded assembly
            case GenKind.Exclusive:
            {
                var outer = md.GetTypeDefinition(t).GetDeclaringType();
                if (!outer.IsNil && n.Kind(outer) != GenKind.User)
                {
                    NeedType(outer); // a type nested in a copied type is copied with it
                    if (!plan.Types.ContainsKey(tok)) throw new Unsupported(Codes.GeneratedTypeNotLoaded, $"it uses {n.TypeName(t)}, which is nested in a compiler-generated type of the loaded assembly");
                    return;
                }
                if (outer.IsNil || n.IsGenericType(t))
                {
                    if (FindTwin(t) == null)
                        throw new Unsupported(Codes.GeneratedTypeNotLoaded, $"it needs the compiler-generated type {n.TypeName(t)} (an anonymous type or a generic closure), and the loaded assembly has no identical type");
                    return;
                }
                CopyWhole(t);
                return;
            }
            case GenKind.Pid:
                if (!md.GetTypeDefinition(t).GetDeclaringType().IsNil) CopyWhole(t); // __StaticArrayInitTypeSize=N
                else NewCopy(t, whole: false);
                return;
            case GenKind.Shared:
            {
                var p = NewCopy(t, whole: false);
                foreach (var m in md.GetTypeDefinition(t).GetMethods())
                    if (md.GetString(md.GetMethodDefinition(m).Name) is ".ctor" or ".cctor") AddPartialMethod(p, m);
                foreach (var f in md.GetTypeDefinition(t).GetFields())
                    if (md.GetString(md.GetFieldDefinition(f).Name) == "<>9") NeedField(f);
                return;
            }
        }
    }

    PType NewCopy(TypeDefinitionHandle t, bool whole)
    {
        if (n.IsGenericType(t)) throw new Unsupported(Codes.GeneratedTypeNotLoaded, $"the generic compiler-generated type {n.TypeName(t)} would have to be copied into the patch");
        var p = new PType { Source = t, Whole = whole, Ns = Namespace(t), Name = Flat(t) };
        plan.Types[Tok(t)] = p;
        plan.Order.Add(p);
        var td = md.GetTypeDefinition(t);
        if (!td.BaseType.IsNil) Visit(td.BaseType);
        return p;
    }

    void AddPartialMethod(PType p, MethodDefinitionHandle m)
    {
        int tok = Tok(m);
        if (plan.Methods.ContainsKey(tok)) return;
        var d = md.GetMethodDefinition(m);
        if (d.GetGenericParameters().Count > 0) throw new Unsupported(Codes.GenericGeneratedMember, $"{n.MemberId(m)} is a generic compiler-generated method");
        if (d.RelativeVirtualAddress == 0) throw new Unsupported(Codes.EmitNotSupported, $"{n.MemberId(m)} has no IL body");
        var pm = new PMethod { Kind = MKind.Copy, Source = m, Direct = true, Owner = p, Name = md.GetString(d.Name), Origin = origin };
        p.Methods.Add(pm);
        plan.Methods[tok] = pm;
        scanner.Signature(md.GetBlobReader(d.Signature));
        plan.Scan.Enqueue((m, origin));
    }

    void CopyWhole(TypeDefinitionHandle t)
    {
        var p = NewCopy(t, whole: true);
        var td = md.GetTypeDefinition(t);
        foreach (var i in td.GetInterfaceImplementations()) Visit(md.GetInterfaceImplementation(i).Interface);
        foreach (var f in td.GetFields())
        {
            p.Fields.Add(f);
            scanner.Signature(md.GetBlobReader(md.GetFieldDefinition(f).Signature));
        }
        foreach (var m in td.GetMethods())
        {
            var d = md.GetMethodDefinition(m);
            if ((d.Attributes & MethodAttributes.PinvokeImpl) != 0 || (d.RelativeVirtualAddress == 0 && (d.Attributes & MethodAttributes.Abstract) == 0))
                throw new Unsupported(Codes.EmitNotSupported, $"{n.MemberId(m)} of a copied compiler-generated type has no IL body");
            if (d.GetGenericParameters().Count > 0) throw new Unsupported(Codes.GenericGeneratedMember, $"{n.MemberId(m)} is a generic method of a compiler-generated type");
            var pm = new PMethod { Kind = MKind.Copy, Source = m, Direct = true, Owner = p, Name = md.GetString(d.Name), Origin = origin };
            p.Methods.Add(pm);
            plan.Methods[Tok(m)] = pm;
            scanner.Signature(md.GetBlobReader(d.Signature));
            plan.Scan.Enqueue((m, origin));
        }
        foreach (var mi in td.GetMethodImplementations())
        {
            var x = md.GetMethodImplementation(mi);
            Visit(x.MethodBody);
            Visit(x.MethodDeclaration);
        }
        foreach (var nested in td.GetNestedTypes()) CopyWhole(nested);
    }

    void NeedMethod(MethodDefinitionHandle m)
    {
        int tok = Tok(m);
        if (plan.Methods.TryGetValue(tok, out var planned) && planned.Direct) return;
        var d = md.GetMethodDefinition(m);
        var t = d.GetDeclaringType();
        NeedType(t);
        if (plan.Types.TryGetValue(Tok(t), out var p))
        {
            if (!p.Whole) AddPartialMethod(p, m);
            return;
        }
        scanner.Signature(md.GetBlobReader(d.Signature));
        if (twins.GetValueOrDefault(Tok(t)) != null) return; // a member of a loaded twin
        string name = md.GetString(d.Name);
        if (Canon.Owned(name, d.Attributes) || addedMethods.Contains(tok)) { Root(m, direct: true); return; }
        // Mono passes a cookie for the call-site signature; no patch has exercised that.
        if (VariableArguments(md.GetBlobReader(d.Signature)))
            throw new Unsupported(Codes.EmitNotSupported, $"it calls {n.MemberId(m)}, which takes a variable argument list");
        if (refused.TryGetValue(tok, out var reason))
            throw new Unsupported(reason, $"it uses {n.MemberId(m)}, which this change added and which cannot be patched");
        if (!o.HasMember(n.TypeName(t), n.BareMethodKey(m)))
            throw new Unsupported(Codes.EmitNotSupported, $"it uses {n.MemberId(m)}, which the loaded assembly lacks and which cannot live in a patch");
    }

    void NeedField(FieldDefinitionHandle f)
    {
        int tok = Tok(f);
        if (plan.Accessors.ContainsKey(tok)) return;
        var d = md.GetFieldDefinition(f);
        var t = d.GetDeclaringType();
        NeedType(t);
        scanner.Signature(md.GetBlobReader(d.Signature));
        if (plan.Types.TryGetValue(Tok(t), out var p))
        {
            if (!p.Whole && !p.Fields.Contains(f)) p.Fields.Add(f);
            return;
        }
        if (twins.GetValueOrDefault(Tok(t)) != null) return;
        if (addedFields.Contains(tok))
        {
            var holder = Holder(t);
            string name = md.GetString(d.Name);
            var added = new SideField
            {
                Source = f, Member = n.FieldId(f), IsStatic = (d.Attributes & FieldAttributes.Static) != 0,
                Id = entry.Name + "|" + n.TypeName(t) + "|" + name,
            };
            added.Get = new PMethod { Kind = MKind.FieldGet, Field = added, Direct = true, Owner = holder, Name = "get__" + name };
            added.Set = new PMethod { Kind = MKind.FieldSet, Field = added, Direct = true, Owner = holder, Name = "set__" + name };
            added.Ref = new PMethod { Kind = MKind.FieldRef, Field = added, Direct = true, Owner = holder, Name = "ref__" + name };
            holder.Methods.Add(added.Get); holder.Methods.Add(added.Set); holder.Methods.Add(added.Ref);
            plan.Accessors[tok] = added;
            return;
        }
        if (refused.TryGetValue(tok, out var reason))
            throw new Unsupported(reason, $"it uses field {n.FieldId(f)}, which this change added and which cannot be patched");
        if (!o.HasMember(n.TypeName(t), n.BareFieldKey(f)))
            throw new Unsupported(Codes.EmitNotSupported, $"it uses field {n.FieldId(f)}, which the loaded assembly lacks or declares differently");
    }

    /// The generic type definition a TypeSpec instantiates, or nil.
    EntityHandle GenericDefinition(TypeSpecificationHandle h)
    {
        var blob = md.GetBlobReader(md.GetTypeSpecification(h).Signature);
        if (blob.ReadByte() != 0x15) return default;
        blob.ReadByte();
        return blob.ReadTypeHandle();
    }

    /// The type of its own module that a type reference names; nil for a reference into another
    /// assembly. The compiler names its own types by definition. A post-processor that imports a
    /// type of the assembly it rewrites writes a reference scoped to the module instead: the
    /// Netcode post-processor does so for a struct of the game that an RPC sends
    /// (`FastBufferWriter::WriteValueSafe<BallStats>`). Such a reference means the definition.
    static TypeDefinitionHandle OwnType(Asm a, TypeReferenceHandle h)
    {
        var scope = a.MD.GetTypeReference(h).ResolutionScope;
        if (scope.Kind == HandleKind.TypeReference ? OwnType(a, (TypeReferenceHandle)scope).IsNil : scope.Kind != HandleKind.ModuleDefinition) return default;
        string name = a.TypeRefName(h, bare: true);
        return a.Types.TryGetValue(name, out var type) ? type
            : throw new Unsupported(Codes.EmitNotSupported, $"a type reference scoped to the module names {name}, which the module does not define");
    }

    /// A type or member of another game assembly of the same change must exist in what the Player
    /// loaded of it: an addition there lives in that assembly's patch, which this patch cannot name.
    void CheckForeign(EntityHandle h)
    {
        // Only a reference that passed is remembered: a failing one must fail for every change that uses it.
        if (checkedRefs.Contains(Tok(h))) return;
        CheckForeignReference(h);
        checkedRefs.Add(Tok(h));
    }

    void CheckForeignReference(EntityHandle h)
    {
        if (h.Kind == HandleKind.TypeReference)
        {
            var reference = (TypeReferenceHandle)h;
            string home = n.TypeRefAssembly(reference), name = n.TypeRefName(reference, bare: true);
            if (workspace.Find(home) is { } other && other != entry && !other.Loaded.Types.ContainsKey(name))
                throw new Unsupported(Codes.CrossAssemblyAddition, $"it uses type {name}, which this change added to {home}");
            return;
        }
        var mr = md.GetMemberReference((MemberReferenceHandle)h);
        EntityHandle parent = mr.Parent;
        if (parent.Kind == HandleKind.TypeSpecification) parent = GenericDefinition((TypeSpecificationHandle)parent);
        Asm loaded, current;
        string type, code, where, member = md.GetString(mr.Name);
        if (parent.Kind == HandleKind.TypeReference)
        {
            string assembly = n.TypeRefAssembly((TypeReferenceHandle)parent);
            if (workspace.Find(assembly) is not { } owner || owner == entry) return;
            (loaded, current, type) = (owner.Loaded, owner.Current, n.TypeRefName((TypeReferenceHandle)parent, bare: true));
            (code, where) = (Codes.CrossAssemblyAddition, "to " + assembly);
        }
        else if (parent.Kind == HandleKind.TypeDefinition && n.Kind((TypeDefinitionHandle)parent) == GenKind.User)
        {
            // A member of one of this assembly's own generic types, named through an instantiation.
            (loaded, current, type) = (o, n, n.TypeName((TypeDefinitionHandle)parent));
            (code, where) = (Codes.GenericType, "to a generic type");
        }
        else return;
        if (mr.GetKind() == MemberReferenceKind.Method)
        {
            if (!loaded.HasMember(type, member + Asm.SigText(mr.DecodeMethodSignature(n.Bare, null))))
                throw new Unsupported(code, $"it uses {type}::{member}, which this change added {where} or changed there");
            return;
        }
        // A field reference does not say static or instance; the current compilation of the owner does.
        string field = member + ":" + mr.DecodeFieldSignature(n.Bare, null);
        foreach (string key in new[] { field, field + " static" })
            if (current.HasMember(type, key) != loaded.HasMember(type, key))
                throw new Unsupported(code, $"it uses field {type}::{member}, which this change added {where} or changed there");
        if (!loaded.HasMember(type, field) && !loaded.HasMember(type, field + " static"))
            throw new Unsupported(code, $"it uses field {type}::{member}, which the loaded assembly lacks");
    }

    void Visit(EntityHandle h)
    {
        switch (h.Kind)
        {
            case HandleKind.TypeDefinition: NeedType((TypeDefinitionHandle)h); break;
            case HandleKind.TypeReference:
                if (OwnType(n, (TypeReferenceHandle)h) is { IsNil: false } own) NeedType(own);
                else CheckForeign(h);
                break;
            case HandleKind.TypeSpecification: scanner.TypeSpec(md.GetBlobReader(md.GetTypeSpecification((TypeSpecificationHandle)h).Signature)); break;
            case HandleKind.MethodDefinition: NeedMethod((MethodDefinitionHandle)h); break;
            case HandleKind.FieldDefinition: NeedField((FieldDefinitionHandle)h); break;
            case HandleKind.MemberReference:
            {
                var mr = md.GetMemberReference((MemberReferenceHandle)h);
                // A call site that passes extra arguments is a reference of its own, whose parent is
                // the method itself when that is a method of this assembly.
                if (mr.Parent.Kind == HandleKind.MethodDefinition || (mr.GetKind() == MemberReferenceKind.Method && VariableArguments(md.GetBlobReader(mr.Signature))))
                    throw new Unsupported(Codes.EmitNotSupported, $"it calls {md.GetString(mr.Name)} with a variable argument list");
                Visit(mr.Parent);
                scanner.Signature(md.GetBlobReader(mr.Signature));
                CheckForeign(h);
                break;
            }
            case HandleKind.MethodSpecification:
            {
                var ms = md.GetMethodSpecification((MethodSpecificationHandle)h);
                Visit(ms.Method);
                scanner.Signature(md.GetBlobReader(ms.Signature));
                break;
            }
            case HandleKind.StandaloneSignature:
            {
                var signature = md.GetStandaloneSignature((StandaloneSignatureHandle)h).Signature;
                if (VariableArguments(md.GetBlobReader(signature))) throw new Unsupported(Codes.EmitNotSupported, "it makes an indirect call with a variable argument list");
                scanner.Signature(md.GetBlobReader(signature));
                break;
            }
            default: throw new Unsupported(Codes.EmitNotSupported, $"a token of kind {h.Kind} is not handled");
        }
    }

    /// Refuses the instruction forms that a token swap cannot express.
    void Check(ushort op, ushort previous, EntityHandle h)
    {
        int tok = Tok(h);
        if (h.Kind == HandleKind.FieldDefinition && plan.Accessors.TryGetValue(tok, out var field))
        {
            if (op is not (IL.Ldfld or IL.Ldflda or IL.Stfld or IL.Ldsfld or IL.Ldsflda or IL.Stsfld))
                throw new Unsupported(Codes.UnsupportedInstruction, $"it takes a reflection handle of the added field {field.Member}");
            if (previous is IL.Unaligned or IL.Volatile)
                throw new Unsupported(Codes.UnsupportedInstruction, $"it reads or writes the added field {field.Member} with a volatile. or unaligned. prefix");
        }
        if (h.Kind != HandleKind.MethodDefinition || !plan.Methods.TryGetValue(tok, out var pm) || !pm.Direct || !pm.Owner.Holder) return;
        // The patch's copy of a method of a user type is not that method: it has another owner and,
        // for an instance method, another shape.
        string member = n.MemberId(pm.Source);
        switch (op)
        {
            case IL.Call: break;
            case IL.Callvirt:
                if (previous == IL.Constrained || pm.StandIn) throw new Unsupported(Codes.UnsupportedInstruction, $"it calls the added method {member} through a constrained or virtual call");
                break;
            case IL.Ldftn:
                // A delegate over a static method with the receiver as closed first argument is what
                // C# makes of an extension method group. A boxed struct receiver has no such form.
                if (pm.StandIn) throw new Unsupported(Codes.UnsupportedInstruction, $"it creates a delegate over the added struct method {member}");
                if (addedMethods.Contains(tok)) plan.Delegated.Add(member);
                break;
            default:
                throw new Unsupported(Codes.UnsupportedInstruction, $"it uses the added method {member} with opcode 0x{op:X} (a reflection handle or an expression tree)");
        }
    }

    void ScanBody(MethodDefinitionHandle m, string from)
    {
        origin = from;
        var d = md.GetMethodDefinition(m);
        if (d.RelativeVirtualAddress == 0) return;
        var body = n.PE.GetMethodBody(d.RelativeVirtualAddress);
        if (!body.LocalSignature.IsNil) Visit(body.LocalSignature);
        foreach (var eh in body.ExceptionRegions) if (!eh.CatchType.IsNil) Visit(eh.CatchType);
        byte[] il = body.GetILBytes() ?? Array.Empty<byte>();
        var r = new IL.Reader(il);
        while (r.Read())
        {
            if (!IL.IsToken(r.Operand) || r.Operand == OperandType.InlineString) continue;
            var h = MetadataTokens.EntityHandle(BinaryPrimitives.ReadInt32LittleEndian(il.AsSpan(r.OperandOffset)));
            Visit(h);
            Check(r.OpCode, r.Previous, h);
        }
    }

    void Plant(Change c)
    {
        origin = c.Member;
        switch (c.Kind)
        {
            case "method_body": Root(c.NewMethod, direct: false); break;
            case "method_added": Root(c.NewMethod, direct: true); break;
            case "method_removed": Stub(c.OldMethod, c.NewType); break;
            case "field_added": NeedField(c.NewField); break;
        }
        while (plan.Scan.Count > 0)
        {
            var (method, from) = plan.Scan.Dequeue();
            ScanBody(method, from);
        }
    }

    /// Plans every patchable change on its own, so that one construct the patch cannot carry is
    /// reported for its member and does not hide the others. Such a change becomes unsupported.
    public void Prepare(List<Change> changes)
    {
        foreach (var c in changes)
        {
            if (c.Action == Actions.Patch)
            {
                if (c.Kind == "method_added") addedMethods.Add(Tok(c.NewMethod));
                else if (c.Kind == "field_added") addedFields.Add(Tok(c.NewField));
            }
            else if (c.Action == Actions.Unsupported)
            {
                // Code that uses a refused addition is refused for the same reason.
                if (c.Kind == "method_added" && !c.NewMethod.IsNil) refused[Tok(c.NewMethod)] = c.Code;
                else if (c.Kind == "field_added" && !c.NewField.IsNil) refused[Tok(c.NewField)] = c.Code;
            }
        }
        foreach (var c in changes)
        {
            if (c.Action != Actions.Patch) continue;
            plan = new Plan();
            try { Plant(c); }
            catch (Unsupported e)
            {
                c.Action = Actions.Unsupported;
                c.Code = e.Code;
                c.Message = $"{c.Member} cannot be patched: {e.Message}.";
                c.Notes.Clear();
            }
        }
    }

    // ---- an earlier patch ---------------------------------------------------------------------

    Dictionary<string, MethodDefinitionHandle>? userMethods;

    /// A method of the new compilation by its member id.
    MethodDefinitionHandle FindMethod(string member)
    {
        if (userMethods == null)
        {
            userMethods = new Dictionary<string, MethodDefinitionHandle>(StringComparer.Ordinal);
            foreach (var h in md.MethodDefinitions)
            {
                var d = md.GetMethodDefinition(h);
                if (n.Kind(d.GetDeclaringType()) == GenKind.User && !Canon.Owned(md.GetString(d.Name), d.Attributes)) userMethods[n.MemberId(h)] = h;
            }
        }
        return userMethods.GetValueOrDefault(member);
    }

    /// The generated code a user method of the new compilation owns, as `<part>=<hash>`: its
    /// lambdas and local functions by position, the methods of its state machines by name.
    List<string> OwnedCode(MethodDefinitionHandle method)
    {
        var closure = cn.Compute(method);
        var owned = new List<MethodDefinitionHandle>();
        foreach (var m in closure.Methods) if (Canon.Lambda(md.GetString(md.GetMethodDefinition(m).Name))) owned.Add(m);
        foreach (var t in closure.Types)
        {
            if (n.Kind(t) != GenKind.Exclusive) continue;
            bool machine = n.IsStateMachine(t);
            foreach (var m in md.GetTypeDefinition(t).GetMethods())
                if (machine || Canon.Lambda(md.GetString(md.GetMethodDefinition(m).Name))) owned.Add(m);
        }
        var code = new List<string>(owned.Count);
        foreach (var m in owned) code.Add(PartName(md.GetString(md.GetMethodDefinition(m).Name)) + "=" + cn.MemberHash(m));
        return code;
    }

    /// `lambda 0` for a lambda or local function, the name for any other generated method.
    static string PartName(string name) => Canon.Lambda(name) && Canon.LambdaOrdinal(name) is >= 0 and var ordinal ? "lambda " + ordinal : name;

    /// Nothing redirects from the methods an earlier patch defined to this one's. So this reports
    /// what the Player may still hold of them: a delegate, a running coroutine or async call made
    /// by the earlier patch keeps the earlier code, and a delegate of an added method made then
    /// never equals one made now. `patch` is null when this result carries none.
    public List<(string Code, string Member, string Message)> EarlierPatch(List<PatchMember> previous, int previousGeneration, Patch? patch)
    {
        var notes = new List<(string Code, string Member, string Message)>();
        var added = new Dictionary<string, string>(StringComparer.Ordinal);
        if (patch != null) foreach (var m in patch.Members) if (m.Kind == "added") added[m.Member] = m.BodyHash;
        var told = new HashSet<string>(StringComparer.Ordinal);
        var owners = new SortedDictionary<string, List<string>>(StringComparer.Ordinal);
        foreach (var m in previous)
        {
            if (m.Kind == "added")
            {
                if (added.TryGetValue(m.Member, out var hash) && hash == m.BodyHash) continue;
                told.Add(m.Member);
                notes.Add((Codes.EarlierPatchDelegatesKeepOldCode, m.Member, $"The method {m.Member}, which patch {previousGeneration} added, {(hash == null ? "is gone" : "changed")}; delegates of it and coroutines or async calls of it that were created while that patch was applied keep running its code."));
            }
            else if (m.Kind is "lambda" or "local" or "state_machine")
            {
                // `<owner> [lambda 0]`, `<owner> [<M>d__5::MoveNext]`: the type name of a state
                // machine carries an ordinal that shifts between compilations, its method names do not.
                int open = m.Member.LastIndexOf(" [", StringComparison.Ordinal);
                string owner = open < 0 ? m.Member : m.Member[..open], part = open < 0 ? "" : m.Member[(open + 2)..].TrimEnd(']');
                if (m.Kind == "state_machine" && part.IndexOf("::", StringComparison.Ordinal) is >= 0 and var at) part = part[(at + 2)..];
                if (!owners.TryGetValue(owner, out var list)) owners[owner] = list = new List<string>();
                list.Add(part + "=" + m.BodyHash);
            }
        }
        // A lambda that the earlier patch copied unchanged, or that this compilation has again as
        // it was, is current: the same code at the same position among the lambdas of its method.
        foreach (var (owner, code) in owners)
        {
            if (told.Contains(owner)) continue;
            var method = FindMethod(owner);
            int gone = Differ.Without(code, method.IsNil ? new List<string>() : OwnedCode(method)).Count;
            if (gone > 0)
                notes.Add((Codes.EarlierPatchDelegatesKeepOldCode, owner, $"Lambdas, local functions or state machines of {owner} that patch {previousGeneration} brought were changed, moved or dropped ({gone} generated method(s)); delegates, coroutines and async calls created while that patch was applied keep running its code."));
        }
        if (patch != null)
            foreach (string member in patch.DelegatedAdded)
                if (previous.Any(m => m.Kind == "added" && m.Member == member))
                    notes.Add((Codes.AddedMethodDelegateIdentity, member, $"Patched code makes a delegate of the added method {member}, which patch {previousGeneration} also had; each patch has its own copy, so `-=` does not remove a handler that was subscribed while patch {previousGeneration} was applied, until a full recompile."));
        return notes;
    }

    /// Emits the patch for the changes that are still patchable. Null when there is nothing to carry.
    public Patch? Build(List<Change> changes)
    {
        var accepted = changes.Where(c => c.Action == Actions.Patch).ToList();
        if (accepted.Count == 0) return null;
        plan = new Plan();
        foreach (var c in accepted) Plant(c);
        return Emit(accepted);
    }

    // ---- emission ----------------------------------------------------------------------------

    AssemblyReferenceHandle AssemblyRef(string name, Version version, string? culture, byte[]? publicKeyOrToken, AssemblyFlags flags)
    {
        if (asmRefs.TryGetValue(name, out var h)) return h;
        h = mb.AddAssemblyReference(mb.GetOrAddString(name), version, culture == null ? default : mb.GetOrAddString(culture),
            publicKeyOrToken == null || publicKeyOrToken.Length == 0 ? default : mb.GetOrAddBlob(publicKeyOrToken), flags, default);
        asmRefs[name] = h;
        return h;
    }

    AssemblyReferenceHandle ImportAssemblyRef(MetadataReader reader, AssemblyReferenceHandle h)
    {
        var r = reader.GetAssemblyReference(h);
        string name = reader.GetString(r.Name);
        if (name == o.Name) return originalRef;
        return AssemblyRef(name, r.Version, r.Culture.IsNil ? null : reader.GetString(r.Culture), reader.GetBlobBytes(r.PublicKeyOrToken), r.Flags);
    }

    TypeReferenceHandle TypeRef(EntityHandle scope, string ns, string name)
    {
        string key = Tok(scope).ToString("X8") + "|" + ns + "|" + name;
        if (typeRefs.TryGetValue(key, out var h)) return h;
        return typeRefs[key] = mb.AddTypeReference(scope, ns.Length == 0 ? default : mb.GetOrAddString(ns), mb.GetOrAddString(name));
    }

    /// Reference to a type of the loaded assembly, named from the loaded metadata.
    TypeReferenceHandle LoadedType(TypeDefinitionHandle t)
    {
        var td = o.MD.GetTypeDefinition(t);
        var outer = td.GetDeclaringType();
        return outer.IsNil
            ? TypeRef(originalRef, td.Namespace.IsNil ? "" : o.MD.GetString(td.Namespace), o.MD.GetString(td.Name))
            : TypeRef(LoadedType(outer), "", o.MD.GetString(td.Name));
    }

    /// A TypeDef of the new compilation: its copy in the patch, or the loaded type it stands for.
    EntityHandle MapType(TypeDefinitionHandle t)
    {
        if (plan.Types.TryGetValue(Tok(t), out var p)) return MetadataTokens.TypeDefinitionHandle(p.Row);
        if (twins.GetValueOrDefault(Tok(t)) is { } twin) return LoadedType(twin.Loaded);
        var td = md.GetTypeDefinition(t);
        var outer = td.GetDeclaringType();
        return outer.IsNil
            ? TypeRef(originalRef, td.Namespace.IsNil ? "" : md.GetString(td.Namespace), md.GetString(td.Name))
            : TypeRef(MapType(outer), "", md.GetString(td.Name));
    }

    MemberReferenceHandle MemberRef(EntityHandle parent, string name, BlobBuilder signature)
    {
        var sig = mb.GetOrAddBlob(signature);
        var key = (Tok(parent), name, sig);
        if (memberRefs.TryGetValue(key, out var h)) return h;
        return memberRefs[key] = mb.AddMemberReference(parent, mb.GetOrAddString(name), sig);
    }

    /// The loaded name of a member of a twin type; any other name is kept.
    string TwinName(EntityHandle type, string name, bool method)
    {
        if (type.Kind == HandleKind.TypeSpecification) type = GenericDefinition((TypeSpecificationHandle)type);
        if (type.Kind != HandleKind.TypeDefinition || twins.GetValueOrDefault(Tok(type)) is not { } twin) return name;
        return (method ? twin.Methods : twin.Fields).GetValueOrDefault(name, name);
    }

    EntityHandle Map(EntityHandle h)
    {
        int tok = Tok(h);
        if (mappedTokens.TryGetValue(tok, out var done)) return done;
        EntityHandle result;
        switch (h.Kind)
        {
            case HandleKind.TypeDefinition:
                result = MapType((TypeDefinitionHandle)h);
                break;
            case HandleKind.TypeReference:
            {
                if (OwnType(n, (TypeReferenceHandle)h) is { IsNil: false } own) { result = MapType(own); break; }
                var tr = md.GetTypeReference((TypeReferenceHandle)h);
                string ns = tr.Namespace.IsNil ? "" : md.GetString(tr.Namespace), name = md.GetString(tr.Name);
                var scope = tr.ResolutionScope;
                result = scope.Kind switch
                {
                    HandleKind.AssemblyReference => TypeRef(ImportAssemblyRef(md, (AssemblyReferenceHandle)scope), ns, name),
                    HandleKind.TypeReference => TypeRef(Map(scope), ns, name),
                    _ => throw new Unsupported(Codes.EmitNotSupported, $"a type reference scoped to a {scope.Kind} is not handled"),
                };
                break;
            }
            case HandleKind.TypeSpecification:
                result = mb.AddTypeSpecification(mb.GetOrAddBlob(copier.TypeSpec(md.GetBlobReader(md.GetTypeSpecification((TypeSpecificationHandle)h).Signature))));
                break;
            case HandleKind.MethodDefinition:
            {
                if (plan.Methods.TryGetValue(tok, out var pm) && pm.Direct) { result = MetadataTokens.MethodDefinitionHandle(pm.Row); break; }
                var d = md.GetMethodDefinition((MethodDefinitionHandle)h);
                result = MemberRef(MapType(d.GetDeclaringType()), TwinName(d.GetDeclaringType(), md.GetString(d.Name), method: true), copier.Signature(md.GetBlobReader(d.Signature)));
                break;
            }
            case HandleKind.FieldDefinition:
            {
                if (fieldRows.TryGetValue(tok, out int row)) { result = MetadataTokens.FieldDefinitionHandle(row); break; }
                var d = md.GetFieldDefinition((FieldDefinitionHandle)h);
                result = MemberRef(MapType(d.GetDeclaringType()), TwinName(d.GetDeclaringType(), md.GetString(d.Name), method: false), copier.Signature(md.GetBlobReader(d.Signature)));
                break;
            }
            case HandleKind.MemberReference:
            {
                var mr = md.GetMemberReference((MemberReferenceHandle)h);
                string name = TwinName(mr.Parent, md.GetString(mr.Name), mr.GetKind() == MemberReferenceKind.Method);
                result = MemberRef(Map(mr.Parent), name, copier.Signature(md.GetBlobReader(mr.Signature)));
                break;
            }
            case HandleKind.MethodSpecification:
            {
                var ms = md.GetMethodSpecification((MethodSpecificationHandle)h);
                result = mb.AddMethodSpecification(Map(ms.Method), mb.GetOrAddBlob(copier.Signature(md.GetBlobReader(ms.Signature))));
                break;
            }
            case HandleKind.StandaloneSignature:
                result = mb.AddStandaloneSignature(mb.GetOrAddBlob(copier.Signature(md.GetBlobReader(md.GetStandaloneSignature((StandaloneSignatureHandle)h).Signature))));
                break;
            default:
                throw new Unsupported(Codes.EmitNotSupported, $"a token of kind {h.Kind} is not handled");
        }
        return mappedTokens[tok] = result;
    }

    /// A type token of the LOADED assembly's metadata (the signature of a removed method).
    EntityHandle MapLoaded(EntityHandle h)
    {
        int tok = Tok(h);
        if (mappedLoaded.TryGetValue(tok, out var done)) return done;
        EntityHandle result;
        switch (h.Kind)
        {
            case HandleKind.TypeDefinition:
                result = LoadedType((TypeDefinitionHandle)h);
                break;
            case HandleKind.TypeReference:
            {
                if (OwnType(o, (TypeReferenceHandle)h) is { IsNil: false } own) { result = LoadedType(own); break; }
                var tr = o.MD.GetTypeReference((TypeReferenceHandle)h);
                string ns = tr.Namespace.IsNil ? "" : o.MD.GetString(tr.Namespace), name = o.MD.GetString(tr.Name);
                var scope = tr.ResolutionScope;
                result = scope.Kind switch
                {
                    HandleKind.AssemblyReference => TypeRef(ImportAssemblyRef(o.MD, (AssemblyReferenceHandle)scope), ns, name),
                    HandleKind.TypeReference => TypeRef(MapLoaded(scope), ns, name),
                    _ => throw new Unsupported(Codes.EmitNotSupported, $"a type reference scoped to a {scope.Kind} is not handled"),
                };
                break;
            }
            case HandleKind.TypeSpecification:
                result = mb.AddTypeSpecification(mb.GetOrAddBlob(loadedCopier.TypeSpec(o.MD.GetBlobReader(o.MD.GetTypeSpecification((TypeSpecificationHandle)h).Signature))));
                break;
            default:
                throw new Unsupported(Codes.EmitNotSupported, $"a token of kind {h.Kind} is not handled");
        }
        return mappedLoaded[tok] = result;
    }

    /// `instance R M(A, B)` on class T becomes `static R M(T this, A, B)`.
    static BlobBuilder Lowered(BlobReader r, SigCopier types, EntityHandle owner)
    {
        var w = new BlobBuilder();
        byte header = r.ReadByte();
        w.WriteByte((byte)(header & ~0x60));
        if ((header & 0x10) != 0) w.WriteCompressedInteger(r.ReadCompressedInteger());
        int count = r.ReadCompressedInteger();
        w.WriteCompressedInteger(count + 1);
        types.Type(ref r, w);
        w.WriteByte(0x12);
        w.WriteCompressedInteger(CodedIndex.TypeDefOrRefOrSpec(owner));
        for (int i = 0; i < count; i++) types.Type(ref r, w);
        return w;
    }

    int CopyBody(PMethod pm)
    {
        var d = md.GetMethodDefinition(pm.Source);
        if (d.RelativeVirtualAddress == 0) return -1;
        var body = n.PE.GetMethodBody(d.RelativeVirtualAddress);
        byte[] il = body.GetILBytes() ?? Array.Empty<byte>();
        bool localloc = false;
        var r = new IL.Reader(il);
        while (r.Read())
        {
            if (r.OpCode == IL.Localloc) localloc = true;
            if (!IL.IsToken(r.Operand)) continue;
            int token = BinaryPrimitives.ReadInt32LittleEndian(il.AsSpan(r.OperandOffset));
            int result;
            if (r.Operand == OperandType.InlineString)
                result = MetadataTokens.GetToken(mb.GetOrAddUserString(md.GetUserString(MetadataTokens.UserStringHandle(token & 0xFFFFFF))));
            else
            {
                var h = MetadataTokens.EntityHandle(token);
                Check(r.OpCode, r.Previous, h);
                if (h.Kind == HandleKind.FieldDefinition && plan.Accessors.TryGetValue(token, out var added))
                {
                    // ldfld/stfld/ldflda and their static forms are one opcode byte plus a token, like `call`.
                    var accessor = r.OpCode is IL.Ldfld or IL.Ldsfld ? added.Get : r.OpCode is IL.Stfld or IL.Stsfld ? added.Set : added.Ref;
                    il[r.Offset] = (byte)IL.Call;
                    result = MetadataTokens.GetToken(MetadataTokens.MethodDefinitionHandle(accessor.Row));
                }
                else
                {
                    if (h.Kind == HandleKind.MethodDefinition && r.OpCode == IL.Callvirt && plan.Methods.TryGetValue(token, out var target) && target.Direct && target.Lower)
                        il[r.Offset] = (byte)IL.Call; // a static method cannot be the target of callvirt
                    result = MetadataTokens.GetToken(Map(h));
                }
            }
            BinaryPrimitives.WriteInt32LittleEndian(il.AsSpan(r.OperandOffset), result);
        }
        // The compiler calls an instance method with `callvirt`, which throws on a null receiver
        // before the method runs. The copy of an ADDED instance method is static and called with
        // `call`, so it checks its receiver itself, in front of the copied instructions. Nothing
        // jumps into an added method, and offsets inside a body are relative: only the exception
        // regions and the sequence points move with it.
        byte[] lead = pm.Lower && addedMethods.Contains(Tok(pm.Source)) ? NullCheck() : Array.Empty<byte>();
        pm.Lead = lead.Length;
        var regions = body.ExceptionRegions;
        bool small = ExceptionRegionEncoder.IsSmallRegionCount(regions.Length);
        foreach (var eh in regions)
            small &= ExceptionRegionEncoder.IsSmallExceptionRegion(lead.Length + eh.TryOffset, eh.TryLength) && ExceptionRegionEncoder.IsSmallExceptionRegion(lead.Length + eh.HandlerOffset, eh.HandlerLength);
        var locals = body.LocalSignature.IsNil ? default : (StandaloneSignatureHandle)Map(body.LocalSignature);
        pm.LocalsRow = locals.IsNil ? 0 : MetadataTokens.GetRowNumber(locals);
        var encoded = bodies.AddMethodBody(lead.Length + il.Length, Math.Max(body.MaxStack, lead.Length > 0 ? 1 : 0), regions.Length, small, locals,
            body.LocalVariablesInitialized ? MethodBodyAttributes.InitLocals : MethodBodyAttributes.None, localloc);
        var writer = new BlobWriter(encoded.Instructions);
        writer.WriteBytes(lead);
        writer.WriteBytes(il);
        foreach (var eh in regions)
            encoded.ExceptionRegions.Add(eh.Kind, lead.Length + eh.TryOffset, eh.TryLength, lead.Length + eh.HandlerOffset, eh.HandlerLength,
                eh.CatchType.IsNil ? default : Map(eh.CatchType), eh.Kind == ExceptionRegionKind.Filter ? lead.Length + eh.FilterOffset : eh.FilterOffset);
        return encoded.Offset;
    }

    /// `ldarg.0; callvirt instance class System.Type System.Object::GetType(); pop`: the receiver
    /// check the compiler itself used to write. It throws NullReferenceException like `callvirt`.
    byte[] NullCheck()
    {
        var signature = new BlobBuilder();
        signature.WriteByte(0x20); signature.WriteCompressedInteger(0);
        signature.WriteByte(0x12); signature.WriteCompressedInteger(CodedIndex.TypeDefOrRefOrSpec(CorlibType("System", "Type")));
        var il = new byte[7];
        il[0] = 0x02;
        il[1] = (byte)IL.Callvirt;
        BinaryPrimitives.WriteInt32LittleEndian(il.AsSpan(2), MetadataTokens.GetToken(MemberRef(CorlibType("System", "Object"), "GetType", signature)));
        il[6] = 0x26;
        return il;
    }

    ParameterHandle NextParameter => MetadataTokens.ParameterHandle(mb.GetRowCount(TableIndex.Param) + 1);

    void Added(MethodDefinitionHandle handle, PMethod pm)
    {
        if (MetadataTokens.GetRowNumber(handle) != pm.Row) throw new InvalidOperationException("The method rows of the patch drifted from the plan.");
    }

    void EmitCopiedMethod(PMethod pm)
    {
        var d = md.GetMethodDefinition(pm.Source);
        MethodAttributes attrs;
        if (pm.Owner.Holder) attrs = MethodAttributes.Public | MethodAttributes.HideBySig | (pm.StandIn ? 0 : MethodAttributes.Static);
        else attrs = (d.Attributes & ~MethodAttributes.MemberAccessMask & ~MethodAttributes.HasSecurity) | MethodAttributes.Public;
        var signature = pm.Lower
            ? Lowered(md.GetBlobReader(d.Signature), copier, MapType(d.GetDeclaringType()))
            : copier.Signature(md.GetBlobReader(d.Signature));
        int offset = CopyBody(pm);
        var parameters = NextParameter;
        // The lock of a synchronized method is taken by the original's wrapper, on the original's
        // receiver, before the redirect is reached. A lowered copy must not lock on its own.
        var implementation = pm.Owner.Holder ? d.ImplAttributes & ~MethodImplAttributes.Synchronized : d.ImplAttributes;
        Added(mb.AddMethodDefinition(attrs, implementation, mb.GetOrAddString(pm.Name), mb.GetOrAddBlob(signature), offset, parameters), pm);
        const ParameterAttributes dropped = ParameterAttributes.HasDefault | ParameterAttributes.HasFieldMarshal;
        var rows = d.GetParameters().Select(md.GetParameter).OrderBy(p => p.SequenceNumber).ToList();
        foreach (var p in rows.Where(p => p.SequenceNumber == 0)) mb.AddParameter(p.Attributes & ~dropped, default, 0);
        if (pm.Lower) mb.AddParameter(ParameterAttributes.None, mb.GetOrAddString("this"), 1);
        foreach (var p in rows.Where(p => p.SequenceNumber > 0))
            mb.AddParameter(p.Attributes & ~dropped, mb.GetOrAddString(md.GetString(p.Name)), p.SequenceNumber + (pm.Lower ? 1 : 0));
    }

    void EmitStub(PMethod pm)
    {
        var d = o.MD.GetMethodDefinition(pm.Removed);
        var signature = Lowered(o.MD.GetBlobReader(d.Signature), loadedCopier, LoadedType(d.GetDeclaringType()));
        var il = new InstructionEncoder(new BlobBuilder());
        il.OpCode(ILOpCode.Ret);
        int offset = bodies.AddMethodBody(il, maxStack: 0);
        var parameters = NextParameter;
        Added(mb.AddMethodDefinition(MethodAttributes.Public | MethodAttributes.Static | MethodAttributes.HideBySig, MethodImplAttributes.IL,
            mb.GetOrAddString(pm.Name), mb.GetOrAddBlob(signature), offset, parameters), pm);
        mb.AddParameter(ParameterAttributes.None, mb.GetOrAddString("this"), 1);
    }

    BlobBuilder FieldType(FieldDefinitionHandle f)
    {
        var r = md.GetBlobReader(md.GetFieldDefinition(f).Signature);
        r.ReadByte(); // FIELD
        var w = new BlobBuilder();
        copier.Type(ref r, w);
        return w;
    }

    /// Accessors for a field the loaded class does not have. Storage lives in the applier assembly,
    /// keyed by the field's stable id, so it survives later patches.
    void EmitAccessor(PMethod pm)
    {
        var field = pm.Field!;
        var d = md.GetFieldDefinition(field.Source);
        byte[] type = FieldType(field.Source).ToArray();
        var store = TypeRef(AssemblyRef(runtimeAssembly, new Version(0, 0, 0, 0), null, null, 0), RuntimeNamespace, "FieldStore");

        // ref T FieldStore.Instance<T>(object owner, string id)   /   ref T FieldStore.Static<T>(string id)
        var storeSig = new BlobBuilder();
        storeSig.WriteByte(0x10); storeSig.WriteCompressedInteger(1); storeSig.WriteCompressedInteger(field.IsStatic ? 1 : 2);
        storeSig.WriteByte(0x10); storeSig.WriteByte(0x1E); storeSig.WriteCompressedInteger(0);
        if (!field.IsStatic) storeSig.WriteByte(0x1C);
        storeSig.WriteByte(0x0E);
        var storeMethod = MemberRef(store, field.IsStatic ? "Static" : "Instance", storeSig);
        var instantiation = new BlobBuilder();
        instantiation.WriteByte(0x0A); instantiation.WriteCompressedInteger(1); instantiation.WriteBytes(type);
        var storeSpec = mb.AddMethodSpecification(storeMethod, mb.GetOrAddBlob(instantiation));
        var typeSpec = mb.AddTypeSpecification(mb.GetOrAddBlob(type));

        var sig = new BlobBuilder();
        sig.WriteByte(0x00);
        sig.WriteCompressedInteger((field.IsStatic ? 0 : 1) + (pm.Kind == MKind.FieldSet ? 1 : 0));
        if (pm.Kind == MKind.FieldSet) sig.WriteByte(0x01);
        else { if (pm.Kind == MKind.FieldRef) sig.WriteByte(0x10); sig.WriteBytes(type); }
        if (!field.IsStatic) { sig.WriteByte(0x12); sig.WriteCompressedInteger(CodedIndex.TypeDefOrRefOrSpec(MapType(d.GetDeclaringType()))); }
        if (pm.Kind == MKind.FieldSet) sig.WriteBytes(type);

        var il = new InstructionEncoder(new BlobBuilder());
        if (!field.IsStatic) il.OpCode(ILOpCode.Ldarg_0);
        il.LoadString(mb.GetOrAddUserString(field.Id));
        il.Call(storeSpec);
        if (pm.Kind == MKind.FieldGet) { il.OpCode(ILOpCode.Ldobj); il.Token(typeSpec); }
        else if (pm.Kind == MKind.FieldSet) { il.LoadArgument(field.IsStatic ? 0 : 1); il.OpCode(ILOpCode.Stobj); il.Token(typeSpec); }
        il.OpCode(ILOpCode.Ret);
        int offset = bodies.AddMethodBody(il, maxStack: 2);
        Added(mb.AddMethodDefinition(MethodAttributes.Public | MethodAttributes.Static | MethodAttributes.HideBySig, MethodImplAttributes.IL,
            mb.GetOrAddString(pm.Name), mb.GetOrAddBlob(sig), offset, NextParameter), pm);
    }

    int FieldDataSize(FieldDefinitionHandle f)
    {
        var r = md.GetBlobReader(md.GetFieldDefinition(f).Signature);
        r.ReadByte();
        switch (r.ReadByte())
        {
            case 0x02: case 0x04: case 0x05: return 1;
            case 0x03: case 0x06: case 0x07: return 2;
            case 0x08: case 0x09: case 0x0C: return 4;
            case 0x0A: case 0x0B: case 0x0D: return 8;
            case 0x11:
            {
                var type = r.ReadTypeHandle();
                if (type.Kind == HandleKind.TypeDefinition)
                {
                    int size = md.GetTypeDefinition((TypeDefinitionHandle)type).GetLayout().Size;
                    if (size > 0) return size;
                }
                break;
            }
        }
        throw new Unsupported(Codes.EmitNotSupported, $"the initial data of {n.FieldId(f)} cannot be sized");
    }

    void EmitField(FieldDefinitionHandle f)
    {
        var d = md.GetFieldDefinition(f);
        // Every copied type is compiler-generated; its members are only reached from the patch.
        var attrs = (d.Attributes & ~FieldAttributes.HasFieldMarshal & ~FieldAttributes.FieldAccessMask) | FieldAttributes.Public;
        var handle = mb.AddFieldDefinition(attrs, mb.GetOrAddString(md.GetString(d.Name)), mb.GetOrAddBlob(copier.Signature(md.GetBlobReader(d.Signature))));
        if (MetadataTokens.GetRowNumber(handle) != fieldRows[Tok(f)]) throw new InvalidOperationException("The field rows of the patch drifted from the plan.");
        if ((d.Attributes & FieldAttributes.HasFieldRVA) != 0)
        {
            int size = FieldDataSize(f);
            mapped.Align(8);
            int offset = mapped.Count;
            mapped.WriteBytes(n.PE.GetSectionData(d.GetRelativeVirtualAddress()).GetContent(0, size));
            mb.AddFieldRelativeVirtualAddress(handle, offset);
        }
        if ((d.Attributes & FieldAttributes.HasDefault) != 0)
        {
            var constant = md.GetConstant(d.GetDefaultValue());
            mb.AddConstant(handle, md.GetBlobReader(constant.Value).ReadConstant(constant.TypeCode));
        }
        if (d.GetOffset() >= 0) mb.AddFieldLayout(handle, d.GetOffset());
    }

    EntityHandle CorlibType(string ns, string name) => TypeRef(corlibScope, ns, name);

    /// The assembly the game takes System.Object from. A small assembly may never name Object or
    /// ValueType itself; then any reference into the class library shows which one it is built on.
    AssemblyReferenceHandle CoreLibrary()
    {
        foreach (var reader in new[] { md, o.MD })
        {
            AssemblyReferenceHandle any = default;
            foreach (var h in reader.TypeReferences)
            {
                var tr = reader.GetTypeReference(h);
                if (tr.ResolutionScope.Kind != HandleKind.AssemblyReference) continue;
                var scope = (AssemblyReferenceHandle)tr.ResolutionScope;
                if (reader.GetString(tr.Namespace) == "System" && reader.GetString(tr.Name) is "Object" or "ValueType") return ImportAssemblyRef(reader, scope);
                if (reader.GetString(reader.GetAssemblyReference(scope).Name) is "mscorlib" or "netstandard" or "System.Runtime") any = scope;
            }
            if (!any.IsNil) return ImportAssemblyRef(reader, any);
        }
        throw new Unsupported(Codes.EmitNotSupported, "neither compilation of the assembly names a type of the core library");
    }

    void EmitType(PType p, int nextField, int nextMethod)
    {
        TypeAttributes attrs;
        EntityHandle baseType;
        if (p.ValueHolder)
        {
            attrs = TypeAttributes.Public | TypeAttributes.Sealed | TypeAttributes.SequentialLayout | TypeAttributes.BeforeFieldInit;
            baseType = CorlibType("System", "ValueType");
        }
        else if (p.Holder)
        {
            attrs = TypeAttributes.Public | TypeAttributes.Abstract | TypeAttributes.Sealed | TypeAttributes.BeforeFieldInit;
            baseType = CorlibType("System", "Object");
        }
        else
        {
            var td = md.GetTypeDefinition(p.Source);
            attrs = (td.Attributes & ~TypeAttributes.VisibilityMask & ~TypeAttributes.HasSecurity) | TypeAttributes.Public;
            baseType = td.BaseType.IsNil ? default : Map(td.BaseType);
        }
        var handle = mb.AddTypeDefinition(attrs, mb.GetOrAddString(p.Ns), mb.GetOrAddString(p.Name), baseType,
            MetadataTokens.FieldDefinitionHandle(nextField), MetadataTokens.MethodDefinitionHandle(nextMethod));
        if (MetadataTokens.GetRowNumber(handle) != p.Row) throw new InvalidOperationException("The type rows of the patch drifted from the plan.");
        foreach (var f in p.Fields) EmitField(f);
        foreach (var pm in p.Methods)
        {
            switch (pm.Kind)
            {
                case MKind.Copy: EmitCopiedMethod(pm); break;
                case MKind.Stub: EmitStub(pm); break;
                default: EmitAccessor(pm); break;
            }
        }
    }

    static byte[] PublicKeyToken(byte[] publicKey)
    {
        if (publicKey.Length == 0) return publicKey;
        byte[] hash = SHA1.HashData(publicKey);
        var token = new byte[8];
        for (int i = 0; i < 8; i++) token[i] = hash[hash.Length - 1 - i];
        return token;
    }

    /// The permission set `csc -unsafe+` emits. It is what makes Unity's Mono waive member access
    /// checks for the patch (IgnoresAccessChecksTo has no effect on that runtime):
    ///   [assembly: SecurityPermission(SecurityAction.RequestMinimum, SkipVerification = true)]
    public static byte[] SkipVerificationPermissionSet()
    {
        var set = new BlobBuilder();
        set.WriteByte((byte)'.');
        set.WriteCompressedInteger(1);
        set.WriteSerializedString("System.Security.Permissions.SecurityPermissionAttribute, mscorlib, Version=4.0.0.0, Culture=neutral, PublicKeyToken=b77a5c561934e089");
        var named = new BlobBuilder();
        named.WriteCompressedInteger(1);
        named.WriteByte(0x54); named.WriteByte(0x02); named.WriteSerializedString("SkipVerification"); named.WriteByte(1);
        set.WriteCompressedInteger(named.Count);
        set.LinkSuffix(named);
        return set.ToArray();
    }

    /// Name of a holder method with its parameter types, to keep overloads apart once receivers are explicit.
    string HolderKey(PType p, PMethod pm)
    {
        if (pm.Kind == MKind.Stub)
        {
            string removed = o.BareMethodKey(pm.Removed);
            return pm.Name + "(" + o.TypeName(o.MD.GetMethodDefinition(pm.Removed).GetDeclaringType()) + "," + removed[(removed.IndexOf('(') + 1)..];
        }
        if (pm.Kind != MKind.Copy) return pm.Name + "()";
        string sig = n.BareMethodKey(pm.Source);
        string rest = sig[(sig.IndexOf('(') + 1)..];
        return (pm.StandIn ? "instance " : "") + pm.Name + "(" + (pm.Lower ? n.TypeName(p.Source) + "," : "") + rest;
    }

    /// Id of a compiler-generated member: the user method it belongs to and its own part.
    static string Part(string owner, string name) => $"{owner} [{PartName(name)}]";

    /// What a method of the patch is to code that holds it across patches. Null for a replacement of
    /// a loaded method (callers and delegates name the loaded method, which is redirected anew) and
    /// for the accessors of an added field (their storage is not in the patch).
    string? MemberKind(PType p, PMethod pm)
    {
        if (pm.Kind != MKind.Copy) return null;
        var d = md.GetMethodDefinition(pm.Source);
        string name = md.GetString(d.Name);
        if (name.Contains(">b__", StringComparison.Ordinal)) return "lambda";
        if (name.Contains(">g__", StringComparison.Ordinal)) return "local";
        if (p.Holder) return Canon.Owned(name, d.Attributes) ? "closure" : pm.Direct ? "added" : null;
        return n.IsStateMachine(p.Source) ? "state_machine" : "closure";
    }

    string MemberName(PType p, PMethod pm)
    {
        string name = md.GetString(md.GetMethodDefinition(pm.Source).Name);
        if (p.Holder) return Canon.Owned(name, md.GetMethodDefinition(pm.Source).Attributes) ? Part(pm.Origin, name) : pm.Origin;
        return Canon.Lambda(name) ? Part(pm.Origin, name) : Part(pm.Origin, n.SimpleName(p.Source) + "::" + name);
    }

    Patch Emit(List<Change> accepted)
    {
        mb = new MetadataBuilder();
        ilStream = new BlobBuilder();
        mapped = new BlobBuilder();
        bodies = new MethodBodyStreamEncoder(ilStream);

        var mvid = mb.ReserveGuid();
        mb.AddModule(0, mb.GetOrAddString(patchName + ".dll"), mvid.Handle, default, default);
        mb.AddAssembly(mb.GetOrAddString(patchName), new Version(0, 0, 0, 0), default, default, 0, AssemblyHashAlgorithm.Sha1);

        var loaded = o.MD.GetAssemblyDefinition();
        originalRef = AssemblyRef(o.Name, loaded.Version, loaded.Culture.IsNil ? null : o.MD.GetString(loaded.Culture),
            PublicKeyToken(o.MD.GetBlobBytes(loaded.PublicKey)), 0);

        corlibScope = CoreLibrary();

        // Row numbers of every definition are fixed before the first body is encoded, because bodies refer forward.
        var order = plan.Order;
        var typeNames = new HashSet<string>(StringComparer.Ordinal);
        int typeRow = 2, fieldRow = 1, methodRow = 1;
        foreach (var p in order)
        {
            p.Row = typeRow++;
            while (!typeNames.Add(p.FullName)) p.Name += "_";
            foreach (var f in p.Fields) fieldRows[Tok(f)] = fieldRow++;
            var names = new HashSet<string>(StringComparer.Ordinal);
            foreach (var pm in p.Methods)
            {
                pm.Row = methodRow++;
                if (!p.Holder) continue;
                // `int F(int)` and `static int F(T, int)` of one class would collide once the first is lowered.
                while (!names.Add(HolderKey(p, pm))) pm.Name += "_";
            }
        }

        mb.AddTypeDefinition(default, default, mb.GetOrAddString("<Module>"), default, MetadataTokens.FieldDefinitionHandle(1), MetadataTokens.MethodDefinitionHandle(1));
        int nextField = 1, nextMethod = 1;
        foreach (var p in order)
        {
            EmitType(p, nextField, nextMethod);
            nextField += p.Fields.Count;
            nextMethod += p.Methods.Count;
        }

        // Tables that must be sorted by their owning type are filled in type order.
        foreach (var p in order.Where(p => p.Whole))
        {
            var td = md.GetTypeDefinition(p.Source);
            var type = MetadataTokens.TypeDefinitionHandle(p.Row);
            var interfaces = new List<EntityHandle>();
            foreach (var i in td.GetInterfaceImplementations()) interfaces.Add(Map(md.GetInterfaceImplementation(i).Interface));
            foreach (var i in interfaces.OrderBy(CodedIndex.TypeDefOrRefOrSpec)) mb.AddInterfaceImplementation(type, i);
        }
        foreach (var p in order.Where(p => p.Whole))
        {
            var td = md.GetTypeDefinition(p.Source);
            var type = MetadataTokens.TypeDefinitionHandle(p.Row);
            foreach (var mi in td.GetMethodImplementations())
            {
                var x = md.GetMethodImplementation(mi);
                mb.AddMethodImplementation(type, Map(x.MethodBody), Map(x.MethodDeclaration));
            }
        }
        foreach (var p in order)
        {
            var type = MetadataTokens.TypeDefinitionHandle(p.Row);
            // An empty struct has size 1, as the compiler writes it.
            if (p.ValueHolder) { mb.AddTypeLayout(type, 0, 1); continue; }
            if (p.Holder) continue;
            var typeLayout = md.GetTypeDefinition(p.Source).GetLayout();
            if (!typeLayout.IsDefault) mb.AddTypeLayout(type, (ushort)typeLayout.PackingSize, (uint)typeLayout.Size);
        }

        mb.AddDeclarativeSecurityAttribute(EntityHandle.AssemblyDefinition, DeclarativeSecurityAction.RequestMinimum, mb.GetOrAddBlob(SkipVerificationPermissionSet()));
        var ctorSig = new BlobBuilder();
        ctorSig.WriteByte(0x20); ctorSig.WriteCompressedInteger(0); ctorSig.WriteByte(0x01);
        var unverifiable = MemberRef(CorlibType("System.Security", "UnverifiableCodeAttribute"), ".ctor", ctorSig);
        mb.AddCustomAttribute(EntityHandle.ModuleDefinition, unverifiable, mb.GetOrAddBlob(new byte[] { 1, 0, 0, 0 }));

        var patch = new Patch { AssemblyName = patchName };
        var redirected = new HashSet<int>();
        int Row(PMethod pm) => MetadataTokens.GetToken(MetadataTokens.MethodDefinitionHandle(pm.Row));
        foreach (var c in accepted)
        {
            switch (c.Kind)
            {
                case "method_body":
                    if (redirected.Add(Tok(c.OldMethod))) patch.Detours.Add(new Detour(c.Member, "changed", Tok(c.OldMethod), Row(plan.Methods[Tok(c.NewMethod)])));
                    break;
                case "method_added":
                    patch.AddedMethods.Add(new AddedMethod(c.Member, Row(plan.Methods[Tok(c.NewMethod)])));
                    break;
                case "method_removed":
                    if (redirected.Add(Tok(c.OldMethod))) patch.Detours.Add(new Detour(c.Member, "removed", Tok(c.OldMethod), Row(plan.Stubs[Tok(c.OldMethod)])));
                    break;
            }
        }
        foreach (var c in accepted)
        {
            if (c.Lambdas == null) continue;
            foreach (var pair in c.Lambdas)
            {
                if (!plan.Methods.TryGetValue(Tok(pair.New), out var pm))
                {
                    if (!c.Notes.Any(note => note.Code == Codes.LambdaNotRemapped))
                        c.Notes.Add((Codes.LambdaNotRemapped, $"A lambda of {c.Member} is not part of the patch; delegates created before the patch keep the old body."));
                    continue;
                }
                if (redirected.Add(Tok(pair.Old))) patch.Detours.Add(new Detour(Part(c.Member, md.GetString(md.GetMethodDefinition(pair.New).Name)), "lambda", Tok(pair.Old), Row(pm)));
            }
        }
        foreach (var added in plan.Accessors.Values.OrderBy(a => a.Get.Row)) patch.AddedFields.Add(new AddedField(added.Member, added.Id, added.IsStatic));
        foreach (var p in order)
            foreach (var pm in p.Methods)
                if (MemberKind(p, pm) is { } kind) patch.Members.Add(new PatchMember(MemberName(p, pm), kind, cn.MemberHash(pm.Source)));
        patch.DelegatedAdded.AddRange(plan.Delegated);

        // Sequence points are valid as they are: no instruction moved.
        var debug = new DebugDirectoryBuilder();
        var symbols = new List<(int Row, MethodDefinitionHandle Source, int Locals, int Lead)>();
        foreach (var p in order)
            foreach (var pm in p.Methods)
                if (pm.Kind == MKind.Copy) symbols.Add((pm.Row, pm.Source, pm.LocalsRow, pm.Lead));
        var pdb = PatchPdb.Build(n.Pdb, mb, symbols);
        if (pdb != null)
        {
            patch.Pdb = pdb.Value.Image;
            debug.AddCodeViewEntry(patchName + ".pdb", pdb.Value.Id, pdb.Value.Version);
        }

        var pe = new ManagedPEBuilder(
            new PEHeaderBuilder(machine: Machine.I386, imageCharacteristics: Characteristics.ExecutableImage | Characteristics.Dll),
            new MetadataRootBuilder(mb), ilStream, mappedFieldData: mapped.Count > 0 ? mapped : null,
            debugDirectoryBuilder: pdb != null ? debug : null, flags: CorFlags.ILOnly,
            deterministicIdProvider: PatchPdb.ContentId);
        var image = new BlobBuilder();
        var id = pe.Serialize(image);
        new BlobWriter(mvid.Content).WriteGuid(id.Guid);
        patch.Image = image.ToArray();
        return patch;
    }
}
