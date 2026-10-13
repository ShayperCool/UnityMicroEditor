// Canonical form of a method body.
//
// Two compilations of the same source never agree on metadata tokens, and Roslyn renumbers its
// generated members whenever a member is inserted earlier in the class (the lifecycle rewrite
// inserts members too):
//   <Best>b__12_0  ->  <Best>b__13_0        <>c__DisplayClass9_0  ->  <>c__DisplayClass10_0
//   <Countdown>d__14 -> <Countdown>d__15    <>9__12_0 -> <>9__13_0
// So a body is compared by what its tokens MEAN, and compiler-generated members are compared by
// STRUCTURE: every generated type or method reached from a user method gets an alias in order of
// first reference ($T0, $M0, ...) and its definition is appended to the canonical text of that user
// method. Two user methods are equal exactly when the text (hash) is equal, whatever the ordinals.
using System.Buffers.Binary;
using System.Reflection;
using System.Reflection.Emit;
using System.Reflection.Metadata;
using System.Reflection.Metadata.Ecma335;
using System.Security.Cryptography;
using System.Text;

internal sealed class Closure
{
    public byte[] Hash = Array.Empty<byte>();
    public string? Text;
    /// Generated types reached from the method (closure classes, state machines, `<>c`), in alias order.
    public readonly List<TypeDefinitionHandle> Types = new();
    /// Generated methods that live in a user type, in `<>c` or in `<PrivateImplementationDetails>`.
    public readonly List<MethodDefinitionHandle> Methods = new();
    /// Fields of `<>c` and `<PrivateImplementationDetails>` the method touches.
    public readonly List<FieldDefinitionHandle> Fields = new();
    /// Compiler-named methods of the generated types (a lambda of a closure class), in alias order.
    public readonly List<MethodDefinitionHandle> Locals = new();
    public bool IsEmpty => Types.Count == 0 && Methods.Count == 0 && Fields.Count == 0;
}

/// Not thread-safe: one instance per comparison worker.
internal sealed class Canon
{
    readonly Asm a;
    readonly MetadataReader md;
    readonly SymbolProvider prov, layout;
    /// Text of tokens that does not depend on any compiler-generated alias.
    readonly Dictionary<int, string> tokCache = new();
    readonly Dictionary<int, string> alias = new();
    readonly Dictionary<int, string> layouts = new();
    /// The definitions that member references of this module name; nil for a foreign member.
    readonly Dictionary<int, EntityHandle> definitions = new();
    readonly string cycle = "$cycle" + Interlocked.Increment(ref instances);
    static int instances;
    readonly Queue<EntityHandle> queue = new();
    readonly HashSet<int> seen = new();
    Closure cur = new();
    bool aliasDependent;
    byte[] buf = new byte[4096];
    int len;
    StringBuilder? text;
    int localAliases;
    /// A closure class is reduced to its layout and the lambdas the code names: see MemberHash.
    bool codeOnly;
    /// Diagnostic: a reference through a type of this module is read as its text alone, so that
    /// what it leads to has no owner. The tests use it to reach the check for such code.
    public bool Blind;

    public Canon(Asm a)
    {
        this.a = a;
        md = a.MD;
        prov = new SymbolProvider(a, bare: false) { DefHook = TypeDefText };
        layout = new SymbolProvider(a, bare: false) { DefHook = LayoutDef };
    }

    void Reset(bool explain)
    {
        alias.Clear(); queue.Clear(); seen.Clear();
        cur = new Closure();
        len = 0; localAliases = 0; codeOnly = false;
        text = explain ? new StringBuilder() : null;
    }

    Closure Finish()
    {
        cur.Hash = SHA256.HashData(buf.AsSpan(0, len));
        cur.Text = text?.ToString();
        return cur;
    }

    void Drain()
    {
        while (queue.Count > 0)
        {
            var h = queue.Dequeue();
            if (h.Kind == HandleKind.TypeDefinition) ExpandType((TypeDefinitionHandle)h);
            else ExpandMethod((MethodDefinitionHandle)h);
        }
    }

    /// The method with every compiler-generated member it owns.
    public Closure Compute(MethodDefinitionHandle root, bool explain = false)
    {
        Reset(explain);
        Body(root);
        Drain();
        return Finish();
    }

    /// The body of one method alone: generated members it names are aliased, not expanded. Two
    /// methods with the same hash name their generated members in the same places, so the lists of
    /// the result pair up by position.
    public Closure Shallow(MethodDefinitionHandle method)
    {
        Reset(false);
        var d = md.GetMethodDefinition(method);
        T(((int)(d.Attributes & MethodAttributes.Static)).ToString());
        Body(method);
        return Finish();
    }

    /// The code a method can run: its signature and body, the lambdas, local functions and state
    /// machines it names, and theirs. A closure class counts as data here, by its layout: a lambda
    /// does not run the other lambdas of the class it lives in. Two methods with the same hash do
    /// the same when they are given the same objects, so a delegate over one of them is as good as
    /// a delegate over the other.
    public string MemberHash(MethodDefinitionHandle method)
    {
        Reset(false);
        codeOnly = true;
        T(Asm.SigText(md.GetMethodDefinition(method).DecodeSignature(prov, null)));
        Body(method);
        Drain();
        return Convert.ToHexString(Finish().Hash).ToLowerInvariant();
    }

    /// Structure of one compiler-generated type on its own (an anonymous type, a generic closure),
    /// to find the identical type in another compilation.
    public string TypeHash(TypeDefinitionHandle type)
    {
        Reset(false);
        Alias(type);
        Drain();
        return Convert.ToHexString(Finish().Hash);
    }

    /// Field layout of a closure type by position, and of every closure it can reach through its
    /// fields (an inner closure holds its outer one): code compiled for one type may be handed an
    /// object of another type exactly when both have this text, whatever their ordinals.
    public string Layout(TypeDefinitionHandle type)
    {
        int token = MetadataTokens.GetToken(type);
        if (layouts.TryGetValue(token, out var known)) return known;
        layouts[token] = cycle; // closures do not refer back; a type that does matches nothing
        var sb = new StringBuilder(a.IsValueType(type) ? "struct" : "class");
        var td = md.GetTypeDefinition(type);
        var explicitLayout = td.GetLayout();
        if (!explicitLayout.IsDefault) sb.Append(" pack=").Append(explicitLayout.PackingSize).Append(" size=").Append(explicitLayout.Size);
        foreach (var f in td.GetFields())
        {
            var fd = md.GetFieldDefinition(f);
            if ((fd.Attributes & FieldAttributes.Static) != 0) continue;
            sb.Append(';').Append(md.GetString(fd.Name)).Append(':').Append(fd.DecodeSignature(layout, null));
        }
        return layouts[token] = sb.ToString();
    }

    /// Whether the signature of a method names a compiler-generated type that a patch copies (a
    /// closure struct or class): anonymous types and generic closures are never copied.
    public bool NamesCopiedType(MethodDefinitionHandle method)
    {
        bool found = false;
        var names = new SymbolProvider(a, bare: false)
        {
            DefHook = h =>
            {
                found |= a.Kind(h) is GenKind.Exclusive or GenKind.Shared && !md.GetTypeDefinition(h).GetDeclaringType().IsNil && !a.IsGenericType(h);
                return "";
            },
        };
        md.GetMethodDefinition(method).DecodeSignature(names, null);
        return found;
    }

    /// Signature of a lambda with closure types replaced by their layout.
    public string LayoutSignature(MethodDefinitionHandle method) => Asm.SigText(md.GetMethodDefinition(method).DecodeSignature(layout, null));

    string LayoutDef(TypeDefinitionHandle h) => a.Kind(h) is GenKind.User or GenKind.Pid ? a.TypeName(h) : "$generated{" + Layout(h) + "}";

    void Reserve(int extra)
    {
        if (len + extra > buf.Length) Array.Resize(ref buf, Math.Max(buf.Length * 2, len + extra));
    }

    void T(string s)
    {
        Reserve(Encoding.UTF8.GetMaxByteCount(s.Length) + 1);
        len += Encoding.UTF8.GetBytes(s, 0, s.Length, buf, len);
        buf[len++] = (byte)'\n';
        text?.Append(s).Append('\n');
    }

    void B(ReadOnlySpan<byte> bytes)
    {
        if (bytes.Length == 0) return;
        Reserve(bytes.Length);
        bytes.CopyTo(buf.AsSpan(len));
        len += bytes.Length;
        text?.Append("  il ").Append(Convert.ToHexString(bytes)).Append('\n');
    }

    void Body(MethodDefinitionHandle m)
    {
        var d = md.GetMethodDefinition(m);
        if (d.RelativeVirtualAddress == 0) { T("nobody"); return; }
        var body = a.PE.GetMethodBody(d.RelativeVirtualAddress);
        // MaxStack is derived from the instructions and is left out on purpose.
        T(body.LocalVariablesInitialized ? "init" : "noinit");
        if (!body.LocalSignature.IsNil) T(Tok(body.LocalSignature));
        byte[] il = body.GetILBytes() ?? Array.Empty<byte>();
        var r = new IL.Reader(il);
        int last = 0;
        while (r.Read())
        {
            if (!IL.IsToken(r.Operand)) continue;
            B(il.AsSpan(last, r.OperandOffset - last));
            int token = BinaryPrimitives.ReadInt32LittleEndian(il.AsSpan(r.OperandOffset));
            T(r.Operand == OperandType.InlineString
                ? "S:" + md.GetUserString(MetadataTokens.UserStringHandle(token & 0xFFFFFF))
                : Tok(MetadataTokens.EntityHandle(token)));
            last = r.OperandOffset + 4;
        }
        B(il.AsSpan(last));
        foreach (var eh in body.ExceptionRegions)
            T($"eh {eh.Kind} {eh.TryOffset} {eh.TryLength} {eh.HandlerOffset} {eh.HandlerLength} {eh.FilterOffset} {(eh.CatchType.IsNil ? "" : Tok(eh.CatchType))}");
    }

    string Tok(EntityHandle h)
    {
        int token = MetadataTokens.GetToken(h);
        if (tokCache.TryGetValue(token, out var s)) return s;
        bool saved = aliasDependent;
        aliasDependent = false;
        s = TokCore(h);
        if (!aliasDependent) tokCache[token] = s;
        aliasDependent |= saved;
        return s;
    }

    string TokCore(EntityHandle h)
    {
        switch (h.Kind)
        {
            case HandleKind.TypeDefinition: return TypeDefText((TypeDefinitionHandle)h);
            case HandleKind.TypeReference: return a.TypeRefName((TypeReferenceHandle)h);
            case HandleKind.TypeSpecification: return md.GetTypeSpecification((TypeSpecificationHandle)h).DecodeSignature(prov, null);
            case HandleKind.MethodDefinition: return MethodDefText((MethodDefinitionHandle)h);
            case HandleKind.FieldDefinition: return FieldDefText((FieldDefinitionHandle)h);
            case HandleKind.MemberReference:
            {
                var mr = md.GetMemberReference((MemberReferenceHandle)h);
                string parent = Tok(mr.Parent), name = md.GetString(mr.Name);
                bool method = mr.GetKind() == MemberReferenceKind.Method;
                string reference = method
                    ? parent + "::" + name + Asm.SigText(mr.DecodeMethodSignature(prov, null))
                    : parent + "::" + name + ":" + mr.DecodeFieldSignature(prov, null);
                // Generic code names the members of its own type, and its lambdas, through an
                // instantiation: `Box<!0>::<Get>b__1_0`, `<>c__2<!!0>::<>9__2_0`. Such a reference
                // means its definition, and a generated member it leads to belongs to the method
                // like one that is named directly.
                var definition = Definition((MemberReferenceHandle)h, mr, method);
                if (definition.IsNil) return reference;
                return reference + " = " + (method ? MethodDefText((MethodDefinitionHandle)definition) : FieldDefText((FieldDefinitionHandle)definition));
            }
            case HandleKind.MethodSpecification:
            {
                var ms = md.GetMethodSpecification((MethodSpecificationHandle)h);
                return Tok(ms.Method) + "<" + string.Join(",", ms.DecodeSignature(prov, null)) + ">";
            }
            case HandleKind.StandaloneSignature:
            {
                var ss = md.GetStandaloneSignature((StandaloneSignatureHandle)h);
                return ss.GetKind() == StandaloneSignatureKind.LocalVariables
                    ? "locals " + string.Join(",", ss.DecodeLocalSignature(prov, null))
                    : "sig" + Asm.SigText(ss.DecodeMethodSignature(prov, null));
            }
            case HandleKind.ModuleReference:
                return "[.module " + md.GetString(md.GetModuleReference((ModuleReferenceHandle)h).Name) + "]";
            default:
                throw new InvalidDataException($"Unexpected token kind {h.Kind} in a method body of {a.Name}.");
        }
    }

    /// The method or field of this module that a reference through a type of this module names;
    /// nil for a member of another assembly, of an array type, or a call site with its own
    /// signature. A reference that names no member is not understood, and nothing that holds it
    /// may count as unchanged.
    EntityHandle Definition(MemberReferenceHandle handle, MemberReference mr, bool method)
    {
        if (Blind) return default;
        int token = MetadataTokens.GetToken(handle);
        if (definitions.TryGetValue(token, out var known)) return known;
        TypeDefinitionHandle type;
        switch (mr.Parent.Kind)
        {
            case HandleKind.TypeDefinition:
                type = (TypeDefinitionHandle)mr.Parent;
                break;
            case HandleKind.TypeSpecification:
            {
                var blob = md.GetBlobReader(md.GetTypeSpecification((TypeSpecificationHandle)mr.Parent).Signature);
                if (blob.ReadSignatureTypeCode() != SignatureTypeCode.GenericTypeInstance || blob.ReadSignatureTypeCode() != SignatureTypeCode.TypeHandle) return definitions[token] = default;
                var generic = blob.ReadTypeHandle();
                if (generic.Kind != HandleKind.TypeDefinition) return definitions[token] = default;
                type = (TypeDefinitionHandle)generic;
                break;
            }
            default:
                return definitions[token] = default;
        }
        string name = md.GetString(mr.Name);
        var td = md.GetTypeDefinition(type);
        EntityHandle found = default;
        if (method)
        {
            // A call with a variable argument list has the signature of its call site.
            if (md.GetBlobReader(mr.Signature).ReadSignatureHeader().CallingConvention == SignatureCallingConvention.VarArgs) return definitions[token] = default;
            string signature = Asm.SigText(mr.DecodeMethodSignature(a.Plain, null));
            foreach (var m in td.GetMethods())
            {
                var d = md.GetMethodDefinition(m);
                if (md.StringComparer.Equals(d.Name, name) && Asm.SigText(d.DecodeSignature(a.Plain, null)) == signature) { found = m; break; }
            }
        }
        else
        {
            string fieldType = mr.DecodeFieldSignature(a.Plain, null);
            foreach (var f in td.GetFields())
            {
                var d = md.GetFieldDefinition(f);
                if (md.StringComparer.Equals(d.Name, name) && d.DecodeSignature(a.Plain, null) == fieldType) { found = f; break; }
            }
        }
        if (found.IsNil) throw new InvalidDataException($"{a.TypeName(type)} of {a.Name} declares no member {name} that a method body names through it.");
        return definitions[token] = found;
    }

    string Alias(TypeDefinitionHandle h)
    {
        int token = MetadataTokens.GetToken(h);
        if (!alias.TryGetValue(token, out var s))
        {
            alias[token] = s = "$T" + cur.Types.Count;
            cur.Types.Add(h);
            queue.Enqueue(h);
        }
        return s;
    }

    string MethodAlias(MethodDefinitionHandle h)
    {
        int token = MetadataTokens.GetToken(h);
        if (!alias.TryGetValue(token, out var s))
        {
            alias[token] = s = "$M" + cur.Methods.Count;
            cur.Methods.Add(h);
            queue.Enqueue(h);
        }
        return s;
    }

    /// Alias for a `<`-named method of a type that is expanded whole anyway.
    string LocalAlias(MethodDefinitionHandle h)
    {
        int token = MetadataTokens.GetToken(h);
        if (!alias.TryGetValue(token, out var s))
        {
            alias[token] = s = "$m" + localAliases++;
            cur.Locals.Add(h);
        }
        return s;
    }

    string TypeDefText(TypeDefinitionHandle h)
    {
        switch (a.Kind(h))
        {
            case GenKind.User: return a.TypeName(h);
            case GenKind.Pid: aliasDependent = true; return a.TypeName(h);
            default: aliasDependent = true; return Alias(h);
        }
    }

    internal static bool Generated(string name) => name.Length > 0 && name[0] == '<';

    /// A lambda (`<M>b__1_0`) or a local function (`<M>g__Name|1_0`).
    internal static bool Lambda(string name) => Generated(name) && (name.Contains(">b__", StringComparison.Ordinal) || name.Contains(">g__", StringComparison.Ordinal));

    /// A compiler-named method of a USER type that belongs to the method that reaches it: a lambda,
    /// a local function or a base call helper (`<>n__0`). Its name carries an ordinal, so it is
    /// compared by structure and a patch brings its own copy. Any other `<` name is a member like
    /// one the user wrote, with the same name in every compilation. The clone method of a record
    /// (`<Clone>$`) is one, and it is virtual: a copy would be called without dispatch.
    internal static bool Owned(string name, MethodAttributes attributes) =>
        (attributes & MethodAttributes.Virtual) == 0 && (Lambda(name) || name.StartsWith("<>n__", StringComparison.Ordinal));

    /// Position of a lambda or local function among those of its method, from the compiler's name:
    /// `<M>b__5_0`, `<M>b__0` (inside a closure class), `<M>g__Local|5_1`. -1 when it does not parse.
    internal static int LambdaOrdinal(string name)
    {
        string tail;
        int at = name.IndexOf(">b__", StringComparison.Ordinal);
        if (at >= 0) tail = name[(at + 4)..];
        else
        {
            int bar = name.LastIndexOf('|');
            if (bar < 0) return -1;
            tail = name[(bar + 1)..];
        }
        int underscore = tail.LastIndexOf('_');
        if (underscore >= 0) tail = tail[(underscore + 1)..];
        return int.TryParse(tail, System.Globalization.NumberStyles.None, System.Globalization.CultureInfo.InvariantCulture, out int value) ? value : -1;
    }

    string MethodDefText(MethodDefinitionHandle h)
    {
        var d = md.GetMethodDefinition(h);
        var t = d.GetDeclaringType();
        string name = md.GetString(d.Name);
        string sig = Asm.SigText(d.DecodeSignature(prov, null));
        switch (a.Kind(t))
        {
            case GenKind.User:
                if (!Owned(name, d.Attributes)) return a.TypeName(t) + "::" + name + sig;
                aliasDependent = true;
                return a.TypeName(t) + "::" + MethodAlias(h) + sig;
            case GenKind.Pid:
                aliasDependent = true;
                if (seen.Add(MetadataTokens.GetToken(h))) { cur.Methods.Add(h); queue.Enqueue(h); }
                return a.TypeName(t) + "::" + name + sig;
            case GenKind.Shared:
                aliasDependent = true;
                return Alias(t) + "::" + (Generated(name) ? MethodAlias(h) : name) + sig;
            default:
            {
                aliasDependent = true;
                string owner = Alias(t);
                if (!Generated(name)) return owner + "::" + name + sig;
                if (codeOnly && a.IsClosure(t) && seen.Add(MetadataTokens.GetToken(h))) queue.Enqueue(h);
                return owner + "::" + LocalAlias(h) + sig;
            }
        }
    }

    string FieldDefText(FieldDefinitionHandle h)
    {
        var d = md.GetFieldDefinition(h);
        var t = d.GetDeclaringType();
        string name = md.GetString(d.Name);
        string type = d.DecodeSignature(prov, null);
        string suffix = (d.Attributes & FieldAttributes.Static) != 0 ? " static" : "";
        switch (a.Kind(t))
        {
            case GenKind.User: return a.TypeName(t) + "::" + name + ":" + type + suffix;
            case GenKind.Pid:
                aliasDependent = true;
                if (seen.Add(MetadataTokens.GetToken(h))) cur.Fields.Add(h);
                return a.TypeName(t) + "::" + name + ":" + type;
            case GenKind.Shared:
            {
                aliasDependent = true;
                string owner = Alias(t);
                int token = MetadataTokens.GetToken(h);
                if (!alias.TryGetValue(token, out var s))
                {
                    // `<>9` is the singleton instance; `<>9__12_0` caches the delegate of one lambda.
                    alias[token] = s = name == "<>9" ? name : "$F" + cur.Fields.Count;
                    cur.Fields.Add(h);
                }
                return owner + "::" + s + ":" + type + suffix;
            }
            default:
                aliasDependent = true;
                return Alias(t) + "::" + name + ":" + type;
        }
    }

    void ExpandType(TypeDefinitionHandle h)
    {
        var td = md.GetTypeDefinition(h);
        string self = Alias(h);
        string baseText = td.BaseType.IsNil ? "" : Tok(td.BaseType);
        var attrs = td.Attributes & ~TypeAttributes.VisibilityMask;
        if (a.Kind(h) == GenKind.Shared)
        {
            // Its members are added one by one, as they are referenced.
            T($"shared {self} {(int)attrs} {baseText}");
            return;
        }
        if (codeOnly && a.IsClosure(h))
        {
            T($"closure {self} {Layout(h)}");
            return;
        }
        T($"type {self} {(int)attrs} {baseText} generic={td.GetGenericParameters().Count}");
        var interfaces = new List<string>();
        foreach (var i in td.GetInterfaceImplementations()) interfaces.Add(Tok(md.GetInterfaceImplementation(i).Interface));
        interfaces.Sort(StringComparer.Ordinal);
        T("implements " + string.Join(",", interfaces));
        var typeLayout = td.GetLayout();
        if (!typeLayout.IsDefault) T($"layout {typeLayout.PackingSize} {typeLayout.Size}");
        foreach (var f in td.GetFields())
        {
            var fd = md.GetFieldDefinition(f);
            T($"field {md.GetString(fd.Name)} {(int)fd.Attributes} {fd.DecodeSignature(prov, null)}");
        }
        foreach (var m in td.GetMethods())
        {
            var d = md.GetMethodDefinition(m);
            string name = md.GetString(d.Name);
            T($"method {(Generated(name) ? LocalAlias(m) : name)} {(int)d.Attributes} {(int)d.ImplAttributes} {Asm.SigText(d.DecodeSignature(prov, null))}");
            Body(m);
        }
        foreach (var mi in td.GetMethodImplementations())
        {
            var x = md.GetMethodImplementation(mi);
            T($"override {Tok(x.MethodBody)} = {Tok(x.MethodDeclaration)}");
        }
        foreach (var nested in td.GetNestedTypes()) Alias(nested);
    }

    void ExpandMethod(MethodDefinitionHandle m)
    {
        var d = md.GetMethodDefinition(m);
        string name = md.GetString(d.Name);
        string shown = alias.TryGetValue(MetadataTokens.GetToken(m), out var s) ? s : name;
        // Accessibility of a lambda method is not significant; static/instance is.
        var attrs = d.Attributes & ~MethodAttributes.MemberAccessMask;
        T($"generated {shown} {(int)attrs} {(int)d.ImplAttributes} {Asm.SigText(d.DecodeSignature(prov, null))}");
        Body(m);
    }
}
