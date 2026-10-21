// Classifies every difference between the assembly the Player loaded (old) and the new compilation.
//
// The policy lives here, and for what the Netcode post-processor generates in Netcode.cs. Its
// rule: an edit is "patch" only when the emitter and the runtime are known to carry it exactly;
// everything else is refused with a code, because an edit that is reported as applied and is not
// is worse than a restart.
using System.Globalization;
using System.Reflection;
using System.Reflection.Metadata;
using System.Reflection.Metadata.Ecma335;
using System.Text;

internal readonly record struct LambdaPair(MethodDefinitionHandle Old, MethodDefinitionHandle New);

internal sealed class Change
{
    public string Kind = "", Member = "", Action = Actions.None, Code = "", Message = "";
    public readonly List<(string Code, string Message)> Notes = new();
    /// What the member runs, as a short hash, for a method that is patched or added. A note
    /// carries it, so that a reader of two results in a row can tell a note about code that
    /// changed again from one that only still stands.
    public string Revision = "";

    // Not serialized: what the emitter needs.
    public MethodDefinitionHandle OldMethod, NewMethod;
    public FieldDefinitionHandle NewField;
    public TypeDefinitionHandle NewType;
    /// Old lambdas and local functions to redirect to their edited versions.
    public List<LambdaPair>? Lambdas;
    /// Of an added field Unity serializes on an engine object: the classes whose saved documents
    /// hold its value. Null for a field of a [Serializable] class or struct.
    public List<string>? Holders;
}

internal sealed partial class Differ
{
    readonly Workspace workspace;
    readonly Asm o, n;
    readonly Canon co, cn;
    /// What runs once in the new compilation (see `Callers`); shared by the workers.
    readonly Lazy<Callers> onceCallers;
    readonly Dictionary<int, UnityType> unity = new();
    /// What the comparison walked of each assembly; see `Walk`.
    readonly Walk walkedOld = new(), walkedNew = new();
    public readonly List<Change> Changes = new();

    /// Diagnostic: report every comparable body as changed, so the emitter must carry all of them.
    public bool ForceAll;

    public Differ(Workspace workspace, Entry entry, bool blind = false) : this(workspace, entry.Loaded, entry.Current, blind) { }

    /// `blind`: diagnostic, see `Canon.Blind`.
    Differ(Workspace workspace, Asm loaded, Asm current, bool blind, Lazy<Callers>? once = null)
    {
        this.workspace = workspace;
        this.blind = blind;
        o = loaded; n = current;
        co = new Canon(o) { Blind = blind }; cn = new Canon(n) { Blind = blind };
        netcode = Netcode.Uses(o) || Netcode.Uses(n);
        // `netcode` may still be switched off by the form check before anything reads this.
        onceCallers = once ?? new Lazy<Callers>(() => BuildCallers(workspace, n, netcode, new Lazy<HashSet<MethodDefinitionHandle>>(() => Initialized(n))));
    }

    /// The closure of a user method of the loaded assembly, remembered as walked.
    Closure Old(MethodDefinitionHandle method) { var closure = co.Compute(method); walkedOld.Add(method, closure); return closure; }
    /// The same for the new compilation.
    Closure New(MethodDefinitionHandle method) { var closure = cn.Compute(method); walkedNew.Add(method, closure); return closure; }

    readonly bool blind;

    static bool IsUser(Asm a, TypeDefinitionHandle h) => a.Kind(h) == GenKind.User && a.SimpleName(h) != "<Module>";

    Change Add(string kind, string member, string action, string code = "", string message = "")
    {
        var c = new Change { Kind = kind, Member = member, Action = action, Code = code, Message = message };
        Changes.Add(c);
        return c;
    }

    Change Refuse(string kind, string member, string code, string message) => Add(kind, member, Actions.Unsupported, code, message);

    /// Types are independent of each other, so they are compared on `threads` workers. The result
    /// is merged in type-name order and does not depend on the number of workers.
    public void Run(int threads)
    {
        DiffAssembly();
        CheckNetcodeForm();
        var set = new SortedSet<string>(StringComparer.Ordinal);
        foreach (var (name, h) in o.Types) if (IsUser(o, h)) set.Add(name);
        foreach (var (name, h) in n.Types) if (IsUser(n, h)) set.Add(name);
        var names = set.ToArray();
        if (threads <= 1 || names.Length < 64)
        {
            foreach (var name in names) DiffName(name);
            DiffUnowned();
            return;
        }
        var perName = new List<Change>?[names.Length];
        var workers = new Differ[threads];
        Parallel.For(0, threads, new ParallelOptions { MaxDegreeOfParallelism = threads }, worker =>
        {
            var d = workers[worker] = new Differ(workspace, o, n, blind, onceCallers) { ForceAll = ForceAll, netcode = netcode };
            for (int i = worker; i < names.Length; i += threads)
            {
                int before = d.Changes.Count;
                d.DiffName(names[i]);
                if (d.Changes.Count > before) perName[i] = d.Changes.GetRange(before, d.Changes.Count - before);
            }
        });
        foreach (var list in perName) if (list != null) Changes.AddRange(list);
        foreach (var d in workers) { walkedOld.Add(d.walkedOld); walkedNew.Add(d.walkedNew); }
        DiffUnowned();
    }

    // ---- generated code without an owner ---------------------------------------------------

    /// What the comparison walked of one assembly: the user methods whose closure it computed, and
    /// the generated methods and types those closures hold.
    sealed class Walk
    {
        public readonly HashSet<MethodDefinitionHandle> Roots = new(), Methods = new();
        public readonly HashSet<TypeDefinitionHandle> Types = new();

        public void Add(MethodDefinitionHandle root, Closure closure)
        {
            Roots.Add(root);
            Methods.UnionWith(closure.Methods);
            Methods.UnionWith(closure.Locals);
            Types.UnionWith(closure.Types);
        }

        public void Add(Walk other) { Roots.UnionWith(other.Roots); Methods.UnionWith(other.Methods); Types.UnionWith(other.Types); }
    }

    /// The generated methods with a body that the walk did not reach.
    static List<MethodDefinitionHandle> Unreached(Asm a, Walk walk)
    {
        var rest = new List<MethodDefinitionHandle>();
        foreach (var m in a.MD.MethodDefinitions)
        {
            var d = a.MD.GetMethodDefinition(m);
            if (d.RelativeVirtualAddress == 0 || walk.Methods.Contains(m)) continue;
            var t = d.GetDeclaringType();
            string name = a.MD.GetString(d.Name);
            switch (a.Kind(t))
            {
                case GenKind.User:
                    if (!Canon.Owned(name, d.Attributes)) continue;
                    break;
                // A state machine, a closure class or an anonymous type is compared whole with
                // the method that names it.
                case GenKind.Exclusive:
                    if (walk.Types.Contains(t)) continue;
                    break;
                // The constructors of the class that pools the lambdas of a type only make its instance.
                case GenKind.Shared:
                    if (name is ".ctor" or ".cctor") continue;
                    break;
            }
            rest.Add(m);
        }
        return rest;
    }

    /// The compiler-generated methods that code of the assembly can run and that no user method
    /// reaches, each as the user type it lies in and the hash of what it runs.
    ///
    /// The compiler also emits the method of a lambda or local function that nothing names: code
    /// behind a `return` or a `throw`, a branch behind a constant that is false, a local function
    /// that is not called yet. Such a method cannot run, in either compilation, so it is no part
    /// of any edit that could be missed. `Reach` finds what can run by following every token,
    /// and it takes a reference it cannot place for a use of everything of that name: what is
    /// left out here is only what no body of the assembly names at all.
    static List<(string Type, string Hash)> Unowned(Asm a, Canon canon, Walk walk)
    {
        var rest = new List<(string, string)>();
        // The workers walked every method they compared. Nothing is left in the usual case, and
        // then nothing below costs anything.
        if (Unreached(a, walk).Count == 0) return rest;
        // The methods the comparison had no reason to hash: a removed or refused one, a start-up
        // bridge, a method of a type that only one compilation has.
        foreach (var t in a.MD.TypeDefinitions)
        {
            if (a.Kind(t) != GenKind.User) continue;
            foreach (var m in a.MD.GetTypeDefinition(t).GetMethods())
            {
                var d = a.MD.GetMethodDefinition(m);
                if (walk.Roots.Contains(m) || Canon.Owned(a.MD.GetString(d.Name), d.Attributes) || d.RelativeVirtualAddress == 0) continue;
                walk.Add(m, canon.Compute(m));
            }
        }
        var unreached = Unreached(a, walk);
        if (unreached.Count == 0) return rest;
        var reach = new Reach(a);
        foreach (var m in unreached)
        {
            if (!reach.Methods.Contains(m)) continue;
            var user = a.MD.GetMethodDefinition(m).GetDeclaringType();
            while (a.Kind(user) != GenKind.User && !a.MD.GetTypeDefinition(user).GetDeclaringType().IsNil) user = a.MD.GetTypeDefinition(user).GetDeclaringType();
            rest.Add((a.TypeName(user), Convert.ToHexString(canon.Compute(m).Hash)));
        }
        return rest;
    }

    /// A body is compared through the user method that owns it. Generated code that no user
    /// method reaches is compared with nothing, so both compilations must hold the same of it:
    /// otherwise an edit lies where the comparison did not look, and it is refused.
    void DiffUnowned()
    {
        var olds = Unowned(o, co, walkedOld);
        var news = Unowned(n, cn, walkedNew);
        if (olds.Count == 0 && news.Count == 0) return;
        var before = olds.Select(x => x.Type + " " + x.Hash).ToList();
        var after = news.Select(x => x.Type + " " + x.Hash).ToList();
        var types = new SortedSet<string>(StringComparer.Ordinal);
        foreach (string text in Without(before, after).Concat(Without(after, before))) types.Add(text[..text.LastIndexOf(' ')]);
        foreach (string type in types)
            Refuse("generated_code", type, Codes.UnownedGeneratedCode, $"Compiler-generated code of {type} (a lambda, a local function or a state machine) changed, and no method is known to own it; it cannot be compared, so nothing about it can be patched.");
    }

    void DiffName(string name)
    {
        bool inOld = o.Types.TryGetValue(name, out var ot) && IsUser(o, ot);
        bool inNew = n.Types.TryGetValue(name, out var nt) && IsUser(n, nt);
        if (netcode && name.StartsWith(Netcode.GeneratedNamespace, StringComparison.Ordinal)) DiffGenerated(name, inOld ? ot : default, inNew ? nt : default);
        else if (inOld && inNew) DiffType(ot, nt, name);
        else if (inOld && IsEmbeddedAttribute(o, ot))
            Add("type_removed", name, Actions.None, message: "The compiler no longer embeds an attribute type; the loaded one stays and nothing reads it.");
        else if (inOld) Refuse("type_removed", name, Codes.TypeRemoved, $"Type {name} was removed or renamed.");
        else if (IsEmbeddedAttribute(n, nt))
            Add("type_added", name, Actions.None, message: "The compiler embedded an attribute type; nothing reads it at run time.");
        else Refuse("type_added", name, Codes.TypeAdded, $"Type {name} was added or renamed.");
    }

    // The compiler embeds attribute types the class library lacks (the first `#nullable enable`
    // of an assembly brings three, the first `nint` another) and drops them with their last use.
    // Patches carry no custom attributes, so nothing needs them.
    static bool IsEmbeddedAttribute(Asm a, TypeDefinitionHandle type)
    {
        if (a.TypeName(type) == "Microsoft.CodeAnalysis.EmbeddedAttribute") return true;
        foreach (var h in a.MD.GetTypeDefinition(type).GetCustomAttributes())
            if (a.AttributeTypeName(a.MD.GetCustomAttribute(h)) == "Microsoft.CodeAnalysis.EmbeddedAttribute") return true;
        return false;
    }

    // ---- assembly ----------------------------------------------------------------------------

    void DiffAssembly()
    {
        var od = o.MD.GetAssemblyDefinition();
        var nd = n.MD.GetAssemblyDefinition();
        if (od.Version != nd.Version || o.MD.GetString(od.Culture) != n.MD.GetString(nd.Culture) ||
            !o.MD.GetBlobBytes(od.PublicKey).AsSpan().SequenceEqual(n.MD.GetBlobBytes(nd.PublicKey)))
            Refuse("assembly", n.Name, Codes.AssemblyIdentityChanged, $"The identity of {n.Name} changed from version {od.Version} to {nd.Version}; loaded assemblies refer to the old one.");
        var oldRefs = new SortedSet<string>(StringComparer.Ordinal);
        var newRefs = new SortedSet<string>(StringComparer.Ordinal);
        foreach (var h in o.MD.AssemblyReferences) oldRefs.Add(o.MD.GetString(o.MD.GetAssemblyReference(h).Name));
        foreach (var h in n.MD.AssemblyReferences) newRefs.Add(n.MD.GetString(n.MD.GetAssemblyReference(h).Name));
        foreach (var r in newRefs) if (!oldRefs.Contains(r))
            Add("assembly_reference", r, Actions.None, message: "The assembly reference is new; the Player binds it when patched code first uses it.");
        foreach (var r in oldRefs) if (!newRefs.Contains(r))
            Add("assembly_reference", r, Actions.None, message: "The assembly reference is no longer used.");
        CompareAttributes("assembly", n.Name, od.GetCustomAttributes(), nd.GetCustomAttributes());
        CompareAttributes("assembly", n.Name, o.MD.GetModuleDefinition().GetCustomAttributes(), n.MD.GetModuleDefinition().GetCustomAttributes());
    }

    // ---- attributes --------------------------------------------------------------------------

    // Compile-time encodings that the compiler moves between a type and its members when an
    // unrelated member is added (nullable context, the first extension method or indexer of a
    // type), and the marker it drops when an auto-property gets a body. Nothing reads them at run time.
    // The same holds for what the compiler writes on an iterator or async method: the attribute that
    // names its state machine, and [DebuggerStepThrough] on every async method, which only a
    // debugger reads. Without them, making a method an iterator or async is a body edit.
    // What only a compiler reads of a signature is of the same kind: the names of tuple elements,
    // `nint` against IntPtr, `dynamic` against object, `readonly` on a struct or its members and
    // `params`. Every caller in the assembly was compiled again and is compared on its own.
    static bool Ignored(string name) => name is "System.Runtime.CompilerServices.NullableAttribute"
        or "System.Runtime.CompilerServices.NullableContextAttribute" or "System.Runtime.CompilerServices.NullablePublicOnlyAttribute"
        or "System.Runtime.CompilerServices.TupleElementNamesAttribute" or "System.Runtime.CompilerServices.NativeIntegerAttribute"
        or "System.Runtime.CompilerServices.DynamicAttribute" or "System.Runtime.CompilerServices.IsReadOnlyAttribute" or "System.ParamArrayAttribute"
        or "System.Runtime.CompilerServices.ExtensionAttribute" or "System.Reflection.DefaultMemberAttribute"
        or "System.Runtime.CompilerServices.CompilerGeneratedAttribute"
        or "System.Runtime.CompilerServices.IteratorStateMachineAttribute" or "System.Runtime.CompilerServices.AsyncStateMachineAttribute"
        or "System.Runtime.CompilerServices.AsyncIteratorStateMachineAttribute" or "System.Diagnostics.DebuggerStepThroughAttribute";

    static List<string> AttributeTexts(Asm a, CustomAttributeHandleCollection handles)
    {
        var list = new List<string>(handles.Count);
        foreach (var h in handles)
        {
            var ca = a.MD.GetCustomAttribute(h);
            string name = a.AttributeTypeName(ca);
            if (Ignored(name)) continue;
            list.Add(name + "(" + Convert.ToHexString(a.MD.GetBlobBytes(ca.Value)) + ")");
        }
        list.Sort(StringComparer.Ordinal);
        return list;
    }

    /// Attributes a user wrote on a member, by short name.
    static List<string> AuthoredAttributes(Asm a, CustomAttributeHandleCollection handles)
    {
        var list = new List<string>();
        foreach (var h in handles)
        {
            string name = a.AttributeTypeName(a.MD.GetCustomAttribute(h));
            if (Ignored(name) || name.StartsWith("System.Diagnostics.Debugger", StringComparison.Ordinal)) continue;
            list.Add(ShortAttribute(name));
        }
        return list;
    }

    /// The names a field was serialized under before, from its [FormerlySerializedAs] attributes.
    static List<string> FormerNames(Asm a, CustomAttributeHandleCollection handles)
    {
        var names = new List<string>();
        foreach (var h in handles)
        {
            var ca = a.MD.GetCustomAttribute(h);
            if (a.AttributeTypeName(ca) != "UnityEngine.Serialization.FormerlySerializedAsAttribute") continue;
            var blob = a.MD.GetBlobReader(ca.Value);
            if (blob.Length < 3 || blob.ReadUInt16() != 1) continue;
            if (blob.ReadSerializedString() is { Length: > 0 } name) names.Add(name);
        }
        return names;
    }

    static string ShortAttribute(string text)
    {
        int paren = text.IndexOf('(');
        string name = paren >= 0 ? text[..paren] : text;
        name = name[(name.LastIndexOf('.') + 1)..];
        return name.EndsWith("Attribute", StringComparison.Ordinal) && name.Length > 9 ? name[..^9] : name;
    }

    void CompareAttributes(string kind, string member, CustomAttributeHandleCollection oldHandles, CustomAttributeHandleCollection newHandles)
    {
        if (oldHandles.Count == 0 && newHandles.Count == 0) return;
        CompareAttributes(kind, member, AttributeTexts(o, oldHandles), AttributeTexts(n, newHandles));
    }

    void CompareAttributes(string kind, string member, List<string> ol, List<string> nl)
    {
        if (ol.SequenceEqual(nl)) return;
        var removed = Without(ol, nl).Select(ShortAttribute).Distinct().ToList();
        var added = Without(nl, ol).Select(ShortAttribute).Distinct().ToList();
        // The same attribute on both sides is one whose arguments changed.
        var changed = added.Intersect(removed).ToList();
        var parts = new List<string>();
        if (changed.Count > 0) parts.Add("changed [" + string.Join("], [", changed) + "]");
        if (added.Except(changed).Any()) parts.Add("added [" + string.Join("], [", added.Except(changed)) + "]");
        if (removed.Except(changed).Any()) parts.Add("removed [" + string.Join("], [", removed.Except(changed)) + "]");
        Refuse(kind, member, Codes.CustomAttributesChanged, $"Attributes of {member} changed: {string.Join(", ", parts)}.");
    }

    /// The elements of `from` that `other` does not have, counted: two equal texts need two partners.
    internal static List<string> Without(List<string> from, List<string> other)
    {
        var rest = new List<string>(other);
        var result = new List<string>();
        foreach (var x in from) if (!rest.Remove(x)) result.Add(x);
        return result;
    }

    // ---- types -------------------------------------------------------------------------------

    static string GenericParams(Asm a, GenericParameterHandleCollection handles)
    {
        if (handles.Count == 0) return "";
        var sb = new StringBuilder();
        foreach (var h in handles)
        {
            var gp = a.MD.GetGenericParameter(h);
            var constraints = new List<string>();
            foreach (var ch in gp.GetConstraints()) constraints.Add(a.HandleText(a.MD.GetGenericParameterConstraint(ch).Type));
            constraints.Sort(StringComparer.Ordinal);
            sb.Append((int)gp.Attributes).Append('[').Append(string.Join(",", constraints)).Append("];");
        }
        return sb.ToString();
    }

    UnityType Unity(TypeDefinitionHandle type)
    {
        int token = MetadataTokens.GetToken(type);
        if (!unity.TryGetValue(token, out var info)) unity[token] = info = UnityTypes.Classify(workspace, n, type);
        return info;
    }

    void DiffType(TypeDefinitionHandle ot, TypeDefinitionHandle nt, string name)
    {
        var od = o.MD.GetTypeDefinition(ot);
        var nd = n.MD.GetTypeDefinition(nt);

        if (od.Attributes != nd.Attributes)
            Refuse("type", name, Codes.TypeFlagsChanged, $"Flags of type {name} changed from [{od.Attributes}] to [{nd.Attributes}].");
        string oldBase = o.HandleText(od.BaseType), newBase = n.HandleText(nd.BaseType);
        if (oldBase != newBase)
            Refuse("type", name, Codes.BaseTypeChanged, $"The base type of {name} changed from {oldBase} to {newBase}.");
        var oldInterfaces = new SortedSet<string>(StringComparer.Ordinal);
        var newInterfaces = new SortedSet<string>(StringComparer.Ordinal);
        foreach (var i in od.GetInterfaceImplementations()) oldInterfaces.Add(o.HandleText(o.MD.GetInterfaceImplementation(i).Interface));
        foreach (var i in nd.GetInterfaceImplementations()) newInterfaces.Add(n.HandleText(n.MD.GetInterfaceImplementation(i).Interface));
        if (!oldInterfaces.SetEquals(newInterfaces))
            Refuse("type", name, Codes.InterfacesChanged, $"The interface list of {name} changed.");
        if (GenericParams(o, od.GetGenericParameters()) != GenericParams(n, nd.GetGenericParameters()))
            Refuse("type", name, Codes.GenericType, $"Generic parameters or constraints of {name} changed.");
        var oldLayout = od.GetLayout();
        var newLayout = nd.GetLayout();
        if (oldLayout.PackingSize != newLayout.PackingSize || oldLayout.Size != newLayout.Size)
            Refuse("type", name, Codes.TypeLayoutChanged, $"Packing or size of {name} changed.");
        CompareAttributes("type", name, od.GetCustomAttributes(), nd.GetCustomAttributes());
        DiffFields(ot, nt, name);
        DiffMethods(ot, nt, name);
        if (netcode) DiffNetworked(ot, nt, name);
        DiffOverrides(od, nd, name);
        DiffProperties(od, nd, name);
        DiffEvents(od, nd, name);
    }

    void DiffProperties(TypeDefinition od, TypeDefinition nd, string type)
    {
        // Indexers overload: the name alone does not identify a property.
        var os = new Dictionary<string, PropertyDefinition>(StringComparer.Ordinal);
        foreach (var h in od.GetProperties()) { var p = o.MD.GetPropertyDefinition(h); os[o.MD.GetString(p.Name) + Asm.SigText(p.DecodeSignature(o.Plain, null))] = p; }
        foreach (var h in nd.GetProperties())
        {
            var p = n.MD.GetPropertyDefinition(h);
            string name = n.MD.GetString(p.Name);
            if (os.Remove(name + Asm.SigText(p.DecodeSignature(n.Plain, null)), out var old)) CompareAttributes("property", type + "::" + name, old.GetCustomAttributes(), p.GetCustomAttributes());
            else Add("property", type + "::" + name, Actions.None, message: "A property is metadata only; its accessors are reported as methods.");
        }
        foreach (var p in os.Values) Add("property", type + "::" + o.MD.GetString(p.Name), Actions.None, message: "A property is metadata only; its accessors are reported as methods.");
    }

    void DiffEvents(TypeDefinition od, TypeDefinition nd, string type)
    {
        var os = new Dictionary<string, EventDefinition>(StringComparer.Ordinal);
        foreach (var h in od.GetEvents()) { var e = o.MD.GetEventDefinition(h); os[o.MD.GetString(e.Name)] = e; }
        foreach (var h in nd.GetEvents())
        {
            var e = n.MD.GetEventDefinition(h);
            string name = n.MD.GetString(e.Name);
            if (os.Remove(name, out var old)) CompareAttributes("event", type + "::" + name, old.GetCustomAttributes(), e.GetCustomAttributes());
            else Add("event", type + "::" + name, Actions.None, message: "An event is metadata only; its accessors are reported as methods.");
        }
        foreach (var name in os.Keys) Add("event", type + "::" + name, Actions.None, message: "An event is metadata only; its accessors are reported as methods.");
    }

    void DiffOverrides(TypeDefinition od, TypeDefinition nd, string name)
    {
        var oi = od.GetMethodImplementations();
        var ni = nd.GetMethodImplementations();
        if (oi.Count == 0 && ni.Count == 0) return;
        var os = new SortedSet<string>(StringComparer.Ordinal);
        var ns = new SortedSet<string>(StringComparer.Ordinal);
        foreach (var h in oi) { var x = o.MD.GetMethodImplementation(h); os.Add(OverrideText(o, x.MethodBody) + " = " + OverrideText(o, x.MethodDeclaration)); }
        foreach (var h in ni) { var x = n.MD.GetMethodImplementation(h); ns.Add(OverrideText(n, x.MethodBody) + " = " + OverrideText(n, x.MethodDeclaration)); }
        if (!os.SetEquals(ns)) Refuse("type", name, Codes.MethodOverridesChanged, $"Explicit interface implementations or overrides of {name} changed.");
    }

    static string OverrideText(Asm a, EntityHandle h)
    {
        switch (h.Kind)
        {
            case HandleKind.MethodDefinition:
            {
                var m = (MethodDefinitionHandle)h;
                return a.TypeName(a.MD.GetMethodDefinition(m).GetDeclaringType()) + "::" + a.MethodKey(m);
            }
            case HandleKind.MemberReference:
            {
                var mr = a.MD.GetMemberReference((MemberReferenceHandle)h);
                return a.HandleText(mr.Parent) + "::" + a.MD.GetString(mr.Name) + Asm.SigText(mr.DecodeMethodSignature(a.Plain, null));
            }
            default: return h.Kind.ToString();
        }
    }

    // ---- fields ------------------------------------------------------------------------------

    static string ConstantText(Asm a, ConstantHandle h)
    {
        if (h.IsNil) return "";
        var c = a.MD.GetConstant(h);
        return c.TypeCode + ":" + Convert.ToHexString(a.MD.GetBlobBytes(c.Value));
    }

    static string MarshalText(Asm a, FieldDefinition d)
    {
        var h = d.GetMarshallingDescriptor();
        return h.IsNil ? "" : Convert.ToHexString(a.MD.GetBlobBytes(h));
    }

    const FieldAttributes Identity = FieldAttributes.Static | FieldAttributes.Literal;

    void DiffFields(TypeDefinitionHandle ot, TypeDefinitionHandle nt, string type)
    {
        var of = new Dictionary<string, FieldDefinitionHandle>(StringComparer.Ordinal);
        var nf = new Dictionary<string, FieldDefinitionHandle>(StringComparer.Ordinal);
        var oldOrder = new List<string>();
        var newOrder = new List<string>();
        foreach (var f in o.MD.GetTypeDefinition(ot).GetFields()) { string name = o.MD.GetString(o.MD.GetFieldDefinition(f).Name); of[name] = f; oldOrder.Add(name); }
        foreach (var f in n.MD.GetTypeDefinition(nt).GetFields()) { string name = n.MD.GetString(n.MD.GetFieldDefinition(f).Name); nf[name] = f; newOrder.Add(name); }
        var typeAttributes = n.MD.GetTypeDefinition(nt).Attributes;
        bool isEnum = n.IsEnum(nt);
        // Structs, and classes that ask for it, have a layout that other code and native code rely on.
        bool fixedLayout = n.IsValueType(nt) || (typeAttributes & TypeAttributes.LayoutMask) != TypeAttributes.AutoLayout;
        bool enumChanged = false;

        foreach (var name in newOrder)
        {
            var nh = nf[name];
            var nd = n.MD.GetFieldDefinition(nh);
            string id = type + "::" + name;
            if (!of.TryGetValue(name, out var oh))
            {
                enumChanged |= isEnum;
                if (isEnum) continue;
                // A field of the same type and kind went with this edit: as far as anyone can
                // tell, a rename. The value the old field has in the objects that exist is not
                // the new field's.
                string added = nd.DecodeSignature(n.Plain, null);
                bool replaces = oldOrder.Any(x => !nf.ContainsKey(x) && o.MD.GetFieldDefinition(of[x]) is var gone
                    && (gone.Attributes & Identity) == (nd.Attributes & Identity) && gone.DecodeSignature(o.Plain, null) == added);
                FieldAdded(nh, nt, id, fixedLayout, retyped: false, replaces);
                continue;
            }
            var od = o.MD.GetFieldDefinition(oh);
            string oldType = od.DecodeSignature(o.Plain, null), newType = nd.DecodeSignature(n.Plain, null);
            if (isEnum)
            {
                enumChanged |= oldType != newType || od.Attributes != nd.Attributes || ConstantText(o, od.GetDefaultValue()) != ConstantText(n, nd.GetDefaultValue());
                CompareAttributes("field", id, od.GetCustomAttributes(), nd.GetCustomAttributes());
                continue;
            }
            if (oldType != newType || (od.Attributes & Identity) != (nd.Attributes & Identity))
            {
                // Another type, or static toggled: the loaded storage cannot serve; a removed and an added field.
                bool oldLiteral = (od.Attributes & FieldAttributes.Literal) != 0, newLiteral = (nd.Attributes & FieldAttributes.Literal) != 0;
                if (!oldLiteral && (od.Attributes & FieldAttributes.Static) == 0 && fixedLayout)
                    Refuse("field_added", id, Codes.StructLayoutChanged, $"Field {id} changed its type; the layout of {type} is fixed.").NewField = nh;
                else FieldAdded(nh, nt, id, fixedLayout, retyped: !oldLiteral && !newLiteral);
                continue;
            }
            // `readonly` is a rule for the compiler, with one exception: the JIT folds a read of a
            // static readonly field of an initialized class into a constant (observed on Unity's Mono
            // for int; for references on a runtime whose collector does not move objects). Code
            // compiled before or by the patch would keep that constant after an assignment. Adding
            // `readonly`, and either direction on an instance field, behaves like a new compilation.
            const FieldAttributes benign = FieldAttributes.FieldAccessMask | FieldAttributes.InitOnly;
            const FieldAttributes foldable = FieldAttributes.Static | FieldAttributes.InitOnly;
            if ((od.Attributes & ~benign) != (nd.Attributes & ~benign) || MarshalText(o, od) != MarshalText(n, nd))
                Refuse("field", id, Codes.FieldFlagsChanged, $"Flags of field {id} changed from [{od.Attributes}] to [{nd.Attributes}].");
            else if ((od.Attributes & foldable) == foldable && (nd.Attributes & FieldAttributes.InitOnly) == 0)
                Refuse("field", id, Codes.StaticReadonlyRemoved, $"`readonly` was removed from the static field {id}; the runtime may have compiled its value into existing code as a constant, so an assignment would not be seen.");
            else if ((od.Attributes & benign) != (nd.Attributes & benign))
            {
                // A field that Unity starts or stops serializing is the same edit as [SerializeField]
                // on it: nothing of it is in the Player before a restart, and a scene value for the
                // field is not read. The bodies are as they were, so a note would be all of the answer.
                // Unity serializes neither a read-only field nor one that is not public or marked,
                // so `readonly` decides it like the accessibility does.
                bool access = (od.Attributes & FieldAttributes.FieldAccessMask) != (nd.Attributes & FieldAttributes.FieldAccessMask);
                bool before = Serialized(nd, od.Attributes, nt, AuthoredAttributes(o, od.GetCustomAttributes()));
                bool after = Serialized(nd, nd.Attributes, nt, AuthoredAttributes(n, nd.GetCustomAttributes()));
                if (before != after)
                {
                    string what = !access ? "`readonly`" : (od.Attributes & FieldAttributes.InitOnly) != (nd.Attributes & FieldAttributes.InitOnly) ? "accessibility and `readonly`" : "accessibility";
                    Refuse("field", id, Codes.SerializationChanged, after
                        ? $"Field {id} changed {what} and Unity now serializes it; the Player reads scenes, prefabs and assets with the loaded fields until a restart, so its saved value is not applied."
                        : $"Field {id} changed {what} and Unity no longer serializes it; the Player keeps reading it from scenes, prefabs and assets until a restart.");
                }
                else if (access)
                    Add("field", id, Actions.Note).Notes.Add((Codes.AccessibilityChanged,
                        $"Field {id} changed accessibility; reflection keeps the loaded accessibility until a full recompile."));
            }
            if (ConstantText(o, od.GetDefaultValue()) != ConstantText(n, nd.GetDefaultValue()))
                Add("const", id, Actions.None, message: "A constant has no storage; the methods that use it were recompiled and are compared on their own.");
            if (od.GetOffset() != nd.GetOffset())
                Refuse("field", id, Codes.TypeLayoutChanged, $"The explicit offset of field {id} changed.");
            CompareAttributes("field", id, od.GetCustomAttributes(), nd.GetCustomAttributes());
        }
        foreach (var name in oldOrder)
        {
            if (nf.ContainsKey(name)) continue;
            var od = o.MD.GetFieldDefinition(of[name]);
            string id = type + "::" + name;
            if (isEnum) enumChanged = true;
            else if ((od.Attributes & FieldAttributes.Literal) != 0) Add("const", id, Actions.None, message: "A removed constant had no storage.");
            else if ((od.Attributes & FieldAttributes.Static) == 0 && fixedLayout)
                Refuse("field_removed", id, Codes.StructLayoutChanged, $"Field {id} was removed; the layout of {type} is fixed.");
            else Add("field_removed", id, Actions.None, message: "The loaded storage stays; new code no longer uses it.");
        }
        if (enumChanged) { Refuse("type", type, Codes.EnumChanged, $"Members of enum {type} changed."); return; }
        if (!fixedLayout) return;
        // The order of the surviving instance fields is the layout.
        var oldCommon = oldOrder.Where(x => nf.ContainsKey(x) && (o.MD.GetFieldDefinition(of[x]).Attributes & FieldAttributes.Static) == 0).ToList();
        var newCommon = newOrder.Where(x => of.ContainsKey(x) && (n.MD.GetFieldDefinition(nf[x]).Attributes & FieldAttributes.Static) == 0).ToList();
        if (!oldCommon.SequenceEqual(newCommon))
            Refuse("type", type, Codes.StructLayoutChanged, $"The order of the fields of {type} changed; it is the layout of the type.");
    }

    /// `replaces`: a field of the same type was removed by the same edit.
    void FieldAdded(FieldDefinitionHandle field, TypeDefinitionHandle type, string id, bool fixedLayout, bool retyped, bool replaces = false)
    {
        var d = n.MD.GetFieldDefinition(field);
        bool isStatic = (d.Attributes & FieldAttributes.Static) != 0;
        if ((d.Attributes & FieldAttributes.Literal) != 0)
        {
            Add("const", id, Actions.None, message: "A constant has no storage; the compiler inlines it into the methods that use it.");
            return;
        }
        (string Code, string Message)? obstacle =
            n.IsInterface(type) ? (Codes.InterfaceChanged, $"Field {id} was added to an interface.") :
            n.IsGenericType(type) ? (Codes.GenericType, $"Field {id} was added to a generic type.") :
            !isStatic && fixedLayout ? (Codes.StructLayoutChanged, $"Instance field {id} was added; it changes the size of {n.TypeName(type)}, and a struct has no identity to key a side table on.") :
            SpecialField(field) is { } reason ? (Codes.SpecialFieldAdded, $"Field {id} cannot live in the side table: {reason}.") : null;
        if (obstacle is { } refusal)
        {
            Refuse("field_added", id, refusal.Code, refusal.Message).NewField = field;
            return;
        }

        var change = Add("field_added", id, Actions.Patch);
        change.NewField = field;
        if (retyped)
            change.Notes.Add((Codes.FieldValueReset, $"Field {id} changed its type or static modifier; it is stored anew and its previous value is lost."));
        // Without an initializer the default is what a new compilation gives every object too:
        // there is nothing an existing object lacks, and nothing to tell. A field that takes
        // the place of another one is told: the objects that exist had a value there.
        else if (!isStatic && StoredByConstructor(field, type))
            change.Notes.Add((Codes.ExistingInstancesKeepDefault, $"Objects that already exist read the default value of the added field {id}; its initializer runs only in objects constructed after the patch."));
        else if (!isStatic && replaces)
            change.Notes.Add((Codes.ExistingInstancesKeepDefault, $"Objects that already exist read the default value of the added field {id}; if it is a renamed field, the value they had under the old name is not taken over."));
        var attributes = AuthoredAttributes(n, d.GetCustomAttributes());
        if (Serialized(d, d.Attributes, type, attributes))
        {
            // The session's scene loader matches saved keys to fields by name and does not apply
            // [FormerlySerializedAs]: a value saved under a former name is not read after a
            // restart either (stage 5 runner5-2).
            var former = FormerNames(n, d.GetCustomAttributes());
            string formerly = former.Count == 0 ? "" : $" It carries [FormerlySerializedAs(\"{string.Join("\"), FormerlySerializedAs(\"", former)}\")], which the session's scene loader does not apply, also not after a restart: a value saved under {(former.Count == 1 ? "that name" : "those names")} stays unread until the data is saved again with the field's new name.";
            change.Notes.Add((Codes.SerializedFieldNotRestored, $"The added field {id} is not restored from scenes, prefabs or assets until a restart.{formerly}"));
            change.Holders = DocumentHolders(type);
        }
        if (attributes.Count > 0)
            change.Notes.Add((Codes.AddedMemberAttributesIgnored, $"[{string.Join("], [", attributes)}] on the added field {id} is invisible to reflection until a full recompile."));
    }

    /// The classes whose saved documents (a component of a scene or prefab, an asset) hold the value
    /// of an instance field of `type`: the type and every class of the game that derives from it,
    /// in this assembly and in the others that may name it. Unity writes an inherited field into
    /// the document of the derived class. Null when the type is no engine object: a [Serializable]
    /// class or struct is held inline by whatever document holds a field of it.
    List<string>? DocumentHolders(TypeDefinitionHandle type)
    {
        if (Unity(type).Kind == UnityKind.No) return null;
        string name = n.TypeName(type);
        var holders = new SortedSet<string>(StringComparer.Ordinal) { name };
        void Scan(Asm a)
        {
            foreach (var (_, h) in a.Types)
                if (a.Kind(h) == GenKind.User && Bases(workspace, a, h).Any(x => x.Name == name)) holders.Add(a.TypeName(h));
        }
        Scan(n);
        if (workspace.Find(n.Name) is { } self)
            foreach (var other in workspace.Others(self)) Scan(other);
        return holders.ToList();
    }

    /// Whether a constructor of the type gives the field a value or hands out its address: an
    /// initializer of the field, or an assignment in a constructor body.
    bool StoredByConstructor(FieldDefinitionHandle field, TypeDefinitionHandle type)
    {
        int token = MetadataTokens.GetToken(field);
        foreach (var m in n.MD.GetTypeDefinition(type).GetMethods())
        {
            var d = n.MD.GetMethodDefinition(m);
            if (d.RelativeVirtualAddress == 0 || n.MD.GetString(d.Name) != ".ctor") continue;
            byte[] il = n.PE.GetMethodBody(d.RelativeVirtualAddress).GetILBytes() ?? Array.Empty<byte>();
            var r = new IL.Reader(il);
            while (r.Read())
                if (r.OpCode is IL.Stfld or IL.Ldflda && System.Buffers.Binary.BinaryPrimitives.ReadInt32LittleEndian(il.AsSpan(r.OperandOffset)) == token) return true;
        }
        return false;
    }

    // [NonSerialized] and [Serializable] are metadata flags. The framework marks its own names for
    // them obsolete, Unity's serializer still reads both.
    const FieldAttributes NotSerialized = (FieldAttributes)0x0080;
    const TypeAttributes SerializableType = (TypeAttributes)0x2000;

    /// Whether Unity serializes the instance field `field` of the new compilation when it has
    /// these flags and attributes in `type`: public or marked, not read-only, in an engine object
    /// or a [Serializable] type, and of a type Unity can serialize. A field type that cannot be
    /// decided counts as serialized: the answer is then a refusal or a note that errs toward what
    /// the Player lacks.
    bool Serialized(FieldDefinition field, FieldAttributes flags, TypeDefinitionHandle type, List<string> attributes)
    {
        if ((flags & (FieldAttributes.Static | FieldAttributes.Literal | FieldAttributes.InitOnly | NotSerialized)) != 0) return false;
        bool reference = attributes.Contains("SerializeReference");
        bool marked = reference || attributes.Contains("SerializeField");
        if (!marked && (flags & FieldAttributes.FieldAccessMask) != FieldAttributes.Public) return false;
        if (Unity(type).Kind == UnityKind.No && (n.MD.GetTypeDefinition(type).Attributes & SerializableType) == 0) return false;
        // Unity 6 serializes a Dictionary that carries [SerializeField] (a public one without
        // it is skipped, analyzer UAC1015), at the top of the field only.
        return CanSerialize(field.DecodeSignature(Shapes.Instance, null), reference, element: false, dictionary: attributes.Contains("SerializeField"));
    }

    /// Unity's rules for the type of a serialized field: a primitive, a string, an enum of at
    /// most 32 bits, a reference to an engine object, a [Serializable] class or struct, a fixed
    /// buffer, a type of the engine that it serializes, and an array or a List<T> of one of these;
    /// with [SerializeField], a Dictionary<K,V> of them as well. Not serialized: a delegate; an
    /// interface, `object` or a class without [Serializable], unless the field is a
    /// [SerializeReference]; an array or list of arrays or lists; a multi-dimensional array; an
    /// enum of 64 bits (analyzer UAC1011); Nullable<T>, tuples, the other dictionaries and the
    /// other types of the class library; the engine's types that it skips (analyzer UAC1001,
    /// `UnityTypes.EngineSkipped`). Read from Unity 6000.6.3f1: SerializedObject of components
    /// with every such field, and the analyzer's warnings.
    bool CanSerialize(Shape shape, bool reference, bool element, bool dictionary = false)
    {
        switch (shape.Kind)
        {
            case ShapeKind.Primitive:
                return shape.Code switch
                {
                    PrimitiveTypeCode.Boolean or PrimitiveTypeCode.Char or PrimitiveTypeCode.SByte or PrimitiveTypeCode.Byte or PrimitiveTypeCode.Int16
                        or PrimitiveTypeCode.UInt16 or PrimitiveTypeCode.Int32 or PrimitiveTypeCode.UInt32 or PrimitiveTypeCode.Int64 or PrimitiveTypeCode.UInt64
                        or PrimitiveTypeCode.Single or PrimitiveTypeCode.Double or PrimitiveTypeCode.String => true,
                    PrimitiveTypeCode.Object => reference,
                    _ => false,
                };
            case ShapeKind.Vector:
                return !element && CanSerialize(shape.Element!, reference, element: true);
            case ShapeKind.Generic:
            {
                var generic = shape.Element!;
                if (generic.Kind == ShapeKind.Reference && UnityTypes.IsClassLibrary(n.TypeRefAssembly(generic.Reference)))
                {
                    string name = n.TypeRefName(generic.Reference, bare: true);
                    if (name == "System.Collections.Generic.List`1")
                        return !element && shape.Arguments.Length == 1 && CanSerialize(shape.Arguments[0], reference, element: true);
                    // Unity 6000.6.3f1 keeps the entries of a [SerializeField] Dictionary whose values it
                    // serializes, a Dictionary among them, whatever the key: a key it cannot write
                    // (a 64-bit enum, an object) is dropped and the entries stay (builder6-6).
                    if (name == "System.Collections.Generic.Dictionary`2" && dictionary && !element && shape.Arguments.Length == 2
                        && CanSerialize(shape.Arguments[1], false, element: false, dictionary: true))
                        return true;
                    // HashSet, Nullable, a tuple, Action<T>, any other dictionary: no enum is among them.
                    return reference && !generic.ValueType;
                }
                return CanSerialize(generic, reference, element);
            }
            case ShapeKind.Definition:
                return SerializableNamed(n, shape.Definition, reference);
            case ShapeKind.Reference:
            {
                string home = n.TypeRefAssembly(shape.Reference), name = n.TypeRefName(shape.Reference, bare: true);
                bool library = UnityTypes.IsClassLibrary(home);
                // A type that cannot be read: a class of the class library is plain data to
                // nobody, a value type of it may be an enum, and anything else may be the engine's.
                if (home.Length == 0 || workspace.Resolve(home) is not { } assembly || !assembly.Types.TryGetValue(name, out var found))
                    return library ? (shape.ValueType ? !UnityTypes.LibraryStructs.Contains(name) : reference)
                        : !(UnityTypes.IsEngine(home) && UnityTypes.EngineSkipped.Contains(name));
                if (library) return (assembly.IsEnum(found) && !WideEnum(assembly, found)) || (reference && !assembly.IsValueType(found));
                if (UnityTypes.IsEngine(home)) return EngineSerializes(assembly, found, name, reference);
                return SerializableNamed(assembly, found, reference);
            }
            // A type parameter is whatever the instantiation makes of it.
            case ShapeKind.Parameter:
                return true;
            default:
                return false;
        }
    }

    static bool IsDelegate(Asm a, TypeDefinitionHandle type)
    {
        var parent = a.MD.GetTypeDefinition(type).BaseType;
        return parent.Kind == HandleKind.TypeReference && a.TypeRefName((TypeReferenceHandle)parent, bare: true) is "System.MulticastDelegate" or "System.Delegate";
    }

    /// A type of the engine as the type of a serialized field: an engine object, one of the types
    /// native code stores in a form of its own (`UnityTypes.EngineNative`, AnimationCurve and
    /// PropertyName among them, which have no flag), and otherwise what a type of the project
    /// would be: a class or struct with [Serializable] that is not abstract. The rules of the
    /// layout helper of tools/LifecycleCompiler, measured on a build of every public type the
    /// engine exports to scripts.
    bool EngineSerializes(Asm assembly, TypeDefinitionHandle type, string name, bool reference)
    {
        if (IsDelegate(assembly, type)) return false;
        if (assembly.IsInterface(type)) return reference;
        if (assembly.IsEnum(type)) return !WideEnum(assembly, type);
        if (UnityTypes.EngineNative.Contains(name)) return true;
        if (UnityTypes.EngineSkipped.Contains(name)) return false;
        if (reference && !assembly.IsValueType(type)) return true;
        // UnityEngine.Object itself is no class below it, and a field of it is a reference.
        if (name == "UnityEngine.Object" || UnityTypes.Classify(workspace, assembly, type).Kind != UnityKind.No) return true;
        var flags = assembly.MD.GetTypeDefinition(type).Attributes;
        return (flags & TypeAttributes.Abstract) == 0 && (flags & SerializableType) != 0;
    }

    /// An enum whose underlying type has 64 bits: Unity does not serialize it.
    static bool WideEnum(Asm a, TypeDefinitionHandle type)
    {
        foreach (var f in a.MD.GetTypeDefinition(type).GetFields())
        {
            var d = a.MD.GetFieldDefinition(f);
            if ((d.Attributes & FieldAttributes.Static) != 0) continue;
            var blob = a.MD.GetBlobReader(d.Signature);
            blob.ReadByte();
            return blob.ReadByte() is 0x0A or 0x0B; // ELEMENT_TYPE_I8, ELEMENT_TYPE_U8
        }
        return false;
    }

    /// A type of a game assembly as the type of a serialized field.
    bool SerializableNamed(Asm a, TypeDefinitionHandle type, bool reference)
    {
        if (a.IsEnum(type)) return !WideEnum(a, type);
        // `fixed int buffer[4]` is a field of the compiler's struct <buffer>e__FixedBuffer, which
        // has no flag; Unity serializes it as an array of its size.
        if (a.SimpleName(type).EndsWith(">e__FixedBuffer", StringComparison.Ordinal)) return true;
        if (IsDelegate(a, type)) return false;
        if (a.IsInterface(type)) return reference;
        if (!a.IsValueType(type) && UnityTypes.Classify(workspace, a, type).Kind != UnityKind.No) return true;
        // Unity serializes no field of an abstract class but a [SerializeReference] (builder6-11).
        var flags = a.MD.GetTypeDefinition(type).Attributes;
        return reference || ((flags & SerializableType) != 0 && (flags & TypeAttributes.Abstract) == 0);
    }

    /// Why a field cannot be a `FieldStore` slot of its type. Null when it can.
    string? SpecialField(FieldDefinitionHandle field)
    {
        var d = n.MD.GetFieldDefinition(field);
        if ((d.Attributes & (FieldAttributes.HasFieldRVA | FieldAttributes.HasFieldMarshal)) != 0) return "it has mapped data or marshalling";
        if (d.GetOffset() >= 0) return "it has an explicit offset";
        var blob = n.MD.GetBlobReader(d.Signature);
        blob.ReadByte();
        switch (blob.ReadByte())
        {
            case 0x0F: case 0x10: case 0x1B: case 0x16: return "a pointer, by-ref or function pointer type is not a valid type argument";
            case 0x1F: case 0x20: return "it is volatile or carries another required modifier";
        }
        foreach (var h in d.GetCustomAttributes())
            if (n.AttributeTypeName(n.MD.GetCustomAttribute(h)) is "System.ThreadStaticAttribute" or "System.ContextStaticAttribute") return "it is thread-static";
        return null;
    }

    // ---- methods -----------------------------------------------------------------------------

    static bool IsBridge(string name) => name.StartsWith(UnityTypes.RuntimeInitializerPrefix, StringComparison.Ordinal);

    static Dictionary<string, MethodDefinitionHandle> UserMethods(Asm a, TypeDefinitionHandle t, bool networked)
    {
        var map = new Dictionary<string, MethodDefinitionHandle>(StringComparer.Ordinal);
        foreach (var m in a.MD.GetTypeDefinition(t).GetMethods())
        {
            // <M>b__1_0 and <M>g__Local|1_0 belong to the user method that reaches them; start-up
            // bridges are compared as a set, and so is what the Netcode post-processor added to a
            // NetworkBehaviour (DiffNetworked).
            var d = a.MD.GetMethodDefinition(m);
            string name = a.MD.GetString(d.Name);
            if (Canon.Owned(name, d.Attributes) || IsBridge(name) || (networked && Netcode.Generated(name))) continue;
            map[a.MethodKey(m)] = m;
        }
        return map;
    }

    static List<string> Bridges(Asm a, Canon canon, TypeDefinitionHandle t)
    {
        var list = new List<string>();
        foreach (var m in a.MD.GetTypeDefinition(t).GetMethods())
            if (IsBridge(a.MD.GetString(a.MD.GetMethodDefinition(m).Name))) list.Add(Convert.ToHexString(canon.Compute(m).Hash));
        return list;
    }

    void DiffMethods(TypeDefinitionHandle ot, TypeDefinitionHandle nt, string type)
    {
        var om = UserMethods(o, ot, netcode && Netcode.Networked(o, ot));
        var nm = UserMethods(n, nt, netcode && Netcode.Networked(n, nt));
        var removed = new List<string>();
        var added = new List<string>();
        foreach (var k in om.Keys) if (!nm.ContainsKey(k)) removed.Add(k);
        foreach (var k in nm.Keys) if (!om.ContainsKey(k)) added.Add(k);
        removed.Sort(StringComparer.Ordinal);
        added.Sort(StringComparer.Ordinal);
        // A signature change, a rename and a static toggle are each a removed and an added method:
        // every caller inside the assembly was recompiled, so its body differs and is patched too.
        foreach (var key in added) MethodAdded(nm[key], nt);
        foreach (var key in removed) MethodRemoved(om[key], nt);
        foreach (var (key, oh) in om.OrderBy(p => p.Key, StringComparer.Ordinal))
            if (nm.TryGetValue(key, out var nh)) DiffMethod(oh, nh, nt);

        // The bridges are numbered across the assembly, so their names shift when one is added
        // elsewhere. What a bridge calls identifies it.
        var oldBridges = Bridges(o, co, ot);
        var newBridges = Bridges(n, cn, nt);
        if (oldBridges.Count == 0 && newBridges.Count == 0) return;
        foreach (var _ in Without(newBridges, oldBridges))
            Add("runtime_initializer", type, Actions.Note).Notes.Add((Codes.RuntimeInitializerAdded, $"A [RuntimeInitializeOnLoadMethod] method of {type} was added; it runs at the next start."));
        foreach (var _ in Without(oldBridges, newBridges))
            Add("runtime_initializer", type, Actions.Note).Notes.Add((Codes.RuntimeInitializerRemoved, $"A [RuntimeInitializeOnLoadMethod] method of {type} was removed or renamed; the change is effective at the next start."));
    }

    /// Why a method that the loaded type lacks cannot live in the patch. Null when it can.
    (string Code, string Message)? AddedMethodObstacle(MethodDefinitionHandle method, TypeDefinitionHandle type, string id)
    {
        var d = n.MD.GetMethodDefinition(method);
        string name = n.MD.GetString(d.Name);
        if (n.IsInterface(type)) return (Codes.InterfaceChanged, $"{id} was added to an interface.");
        if (d.GetGenericParameters().Count > 0) return (Codes.GenericMethod, $"The added method {id} is generic.");
        if (n.IsGenericType(type)) return (Codes.GenericType, $"{id} was added to a generic type.");
        if ((d.Attributes & (MethodAttributes.Virtual | MethodAttributes.Abstract)) != 0)
            return (Codes.VirtualMethodAdded, $"The added method {id} is virtual, an override or an interface implementation; the vtable of the loaded class is fixed.");
        if ((d.Attributes & MethodAttributes.PinvokeImpl) != 0 || d.RelativeVirtualAddress == 0)
            return (Codes.ExternMethodAdded, $"The added method {id} has no IL body.");
        if (name == ".cctor")
            return (Codes.StaticConstructorChanged, $"{n.TypeName(type)} gained a static constructor or its first static field initializer; it runs once when the type is first used.");
        if (name == ".ctor")
            return (Codes.ConstructorAdded, $"Constructor {id} was added or its signature changed; `new` needs a real constructor on the loaded type.");
        if ((d.ImplAttributes & MethodImplAttributes.Synchronized) != 0)
            return (Codes.EmitNotSupported, $"The added method {id} is synchronized; its copy in a patch would lock another object.");
        if (VariableArguments(n, d)) return (Codes.EmitNotSupported, $"The added method {id} takes a variable argument list.");
        if ((d.Attributes & MethodAttributes.Static) == 0 && UnityTypes.Messages.Contains(name) && Unity(type).Kind != UnityKind.No)
            return (Codes.UnityMessageAdded, $"{id} is a Unity message added to an existing class; the engine binds messages once per class.");
        return null;
    }

    void MethodAdded(MethodDefinitionHandle method, TypeDefinitionHandle type)
    {
        var d = n.MD.GetMethodDefinition(method);
        string name = n.MD.GetString(d.Name), id = n.MemberId(method);
        if (netcode && Netcode.IsRpc(n, d))
        {
            rpcReported.Add(id);
            Refuse("method_added", id, Codes.NetcodeRpcChanged, $"The RPC {id} was added, or an RPC was renamed or its parameters changed; the RPCs of a class are registered by an id of their signature when its first object is initialized.").NewMethod = method;
            return;
        }
        if (AddedMethodObstacle(method, type, id) is { } obstacle)
        {
            Refuse("method_added", id, obstacle.Code, obstacle.Message).NewMethod = method;
            return;
        }
        var change = Add("method_added", id, Actions.Patch);
        change.NewMethod = method;
        byte[] hash = New(method).Hash;
        change.Revision = Short(hash);
        var attributes = AuthoredAttributes(n, d.GetCustomAttributes());
        if (attributes.Count > 0)
            change.Notes.Add((Codes.AddedMemberAttributesIgnored, $"[{string.Join("], [", attributes)}] on the added method {id} is invisible to reflection until a full recompile."));
        // The engine finds methods of its objects by name, whatever their accessibility, and only in
        // the loaded class: a message of an input component, an animation event, a UnityEvent wired
        // in a scene (which may also set a property). No string of this assembly needs to name it.
        bool special = d.Attributes.HasFlag(MethodAttributes.SpecialName);
        bool instance = (d.Attributes & MethodAttributes.Static) == 0;
        // A string of the game that equals the name is, as far as anyone can tell, a call by
        // name: Invoke("Blast", 1f) finds nothing and does nothing. That is code of the edit that
        // has no effect, which the note for every added method of an engine object does not say.
        // The note is a change of its own: it is about the code that holds the string, and its
        // revision moves when another place starts or stops naming the method.
        // The string may stand in another assembly of the game: SendMessage("TakeDamage") needs
        // no reference to the class that has the method.
        var holders = !special ? Holders(name) : null;
        bool engine = instance && (!special || name.StartsWith("set_", StringComparison.Ordinal)) && Unity(type).Kind != UnityKind.No;
        // A call by a name the game computes may mean a method of its objects that such a call can
        // reach.
        var computed = holders == null && engine ? Computed(name, d.DecodeSignature(n.Plain, null).ParameterTypes.Length) : null;
        if (holders != null || computed != null)
        {
            var named = Add("method_named", id, Actions.Note);
            using (var sha = System.Security.Cryptography.IncrementalHash.CreateHash(System.Security.Cryptography.HashAlgorithmName.SHA256))
            {
                sha.AppendData(hash);
                foreach (var (holder, count) in (IEnumerable<KeyValuePair<string, int>>?)holders ?? computed!.Select(x => new KeyValuePair<string, int>(x, 1)))
                    sha.AppendData(Encoding.UTF8.GetBytes(holder + "=" + count.ToString(CultureInfo.InvariantCulture) + ";"));
                named.Revision = Short(sha.GetHashAndReset());
            }
            if (holders != null)
                named.Notes.Add((Codes.AddedMethodNamedByString, $"The string \"{name}\" in code of {string.Join(", ", holders.Keys)} names the added method {id}; Invoke, SendMessage, StartCoroutine(string) and reflection look in the loaded class and do not find it until a full recompile, so a call by that name does nothing."));
            else
                named.Notes.Add((Codes.AddedMethodComputedName, $"The game calls methods of its objects by a name it computes ({string.Join(", ", computed!.Take(3))}{(computed!.Count > 3 ? $" and {computed.Count - 3} more" : "")}); SendMessage, Invoke and StartCoroutine(string) look in the loaded class, so such a call that means the added method {id} does nothing until a full recompile."));
        }
        else if (engine)
            change.Notes.Add((Codes.AddedMethodInvisibleByName, $"The added method {id} belongs to an engine object; SendMessage, Invoke, StartCoroutine(string), a UnityEvent, an animation event or an input message cannot find it by name until a full recompile."));
    }

    /// The types whose code passes `name` to a call by name (see `NameCalls`), with how often: in
    /// this assembly by type, in another assembly of the game as `type (assembly)`. A string that
    /// only equals the name (an input axis "Jump", an animator trigger "Attack") calls nothing
    /// (builder6-9). Null when no such call names it.
    SortedDictionary<string, int>? Holders(string name)
    {
        var holders = new SortedDictionary<string, int>(StringComparer.Ordinal);
        foreach (var call in workspace.NameCalls(n))
            if (call.Name == name) holders[call.Holder] = holders.GetValueOrDefault(call.Holder) + 1;
        if (workspace.Find(n.Name) is { } self)
            foreach (var other in workspace.Others(self))
                foreach (var call in workspace.NameCalls(other))
                {
                    if (call.Name != name) continue;
                    string holder = call.Holder + " (" + other.Name + ")";
                    holders[holder] = holders.GetValueOrDefault(holder) + 1;
                }
        return holders.Count > 0 ? holders : null;
    }

    /// Where the game calls a method of its objects by a name it computes, for an added instance
    /// method that such a call can reach: one with at most one parameter (SendMessage passes one
    /// value, Invoke none). The game's code is this assembly and the others of the game. A package
    /// next to them counts with its SendMessage and BroadcastMessage for a method named as Unity
    /// names messages, `On` and a capital letter: the messages a package sends by a name it makes
    /// (an input component's `On<Action>`, for one) are named that way. Null when nowhere.
    List<string>? Computed(string name, int parameters)
    {
        if (parameters > 1) return null;
        var sites = new SortedSet<string>(StringComparer.Ordinal);
        void Add(Asm a, string suffix, bool messages)
        {
            foreach (var call in workspace.NameCalls(a))
                if (call.Name == null && ByName.Contains(call.Api) && (!messages || Messaging.Contains(call.Api))) sites.Add(call.Holder + suffix);
        }
        Add(n, "", false);
        if (workspace.Find(n.Name) is { } self)
        {
            foreach (var other in workspace.Others(self)) Add(other, "", false);
            if (name.Length > 2 && name.StartsWith("On", StringComparison.Ordinal) && char.IsUpper(name[2]))
                foreach (var package in workspace.Packages(self)) Add(package, " (" + package.Name + ")", true);
        }
        return sites.Count > 0 ? sites.ToList() : null;
    }

    static readonly HashSet<string> ByName = new(StringComparer.Ordinal)
    {
        "SendMessage", "SendMessageUpwards", "BroadcastMessage", "Invoke", "InvokeRepeating", "CancelInvoke", "IsInvoking", "StartCoroutine", "StopCoroutine",
    };
    /// The calls by name that reach a method of any component of an object.
    static readonly HashSet<string> Messaging = new(StringComparer.Ordinal) { "SendMessage", "SendMessageUpwards", "BroadcastMessage" };
    /// Reflection on a type by a member's name.
    static readonly HashSet<string> Reflection = new(StringComparer.Ordinal) { "GetMethod", "GetProperty", "GetField", "GetMember", "InvokeMember" };

    /// A call by name: the method that makes it, the user type whose code that is, the API, and
    /// the name when the call has it written as a string (null when the code computes it).
    internal readonly record struct NameCall(MethodDefinitionHandle Caller, string Holder, string Api, string? Name);

    /// The API of a call whose first parameter is the name of a method it looks up: one of the
    /// engine's (`ByName`) on a component, a game object or a MonoBehaviour, or reflection on
    /// System.Type. Null for any other call.
    static string? NameApi(Asm a, EntityHandle h)
    {
        if (h.Kind == HandleKind.MethodSpecification) h = a.MD.GetMethodSpecification((MethodSpecificationHandle)h).Method;
        if (h.Kind != HandleKind.MemberReference) return null;
        var mr = a.MD.GetMemberReference((MemberReferenceHandle)h);
        if (mr.GetKind() != MemberReferenceKind.Method) return null;
        string name = a.MD.GetString(mr.Name);
        if (!ByName.Contains(name) && !Reflection.Contains(name)) return null;
        string parent = a.HandleText(mr.Parent, bare: true);
        bool engine = ByName.Contains(name) && parent is "UnityEngine.Component" or "UnityEngine.GameObject" or "UnityEngine.MonoBehaviour";
        bool reflection = Reflection.Contains(name) && parent == "System.Type";
        if (!engine && !reflection) return null;
        var signature = mr.DecodeMethodSignature(a.Bare, null);
        return signature.ParameterTypes.Length > 0 && signature.ParameterTypes[0] == "System.String" ? name : null;
    }

    /// See `Workspace.NameCalls`.
    internal static List<NameCall> NameCalls(Asm a)
    {
        var calls = new List<NameCall>();
        Canon? canon = null;
        foreach (var m in a.MD.MethodDefinitions)
        {
            var d = a.MD.GetMethodDefinition(m);
            if (d.RelativeVirtualAddress == 0) continue;
            byte[] il = a.PE.GetMethodBody(d.RelativeVirtualAddress).GetILBytes() ?? Array.Empty<byte>();
            string? Api(IL.Reader r) => r.OpCode is IL.Call or IL.Callvirt
                ? NameApi(a, MetadataTokens.EntityHandle(System.Buffers.Binary.BinaryPrimitives.ReadInt32LittleEndian(il.AsSpan(r.OperandOffset)))) : null;
            var scan = new IL.Reader(il);
            bool any = false;
            while (!any && scan.Read()) any = Api(scan) != null;
            if (!any) continue;
            canon ??= new Canon(a);
            var code = canon.Instructions(m);
            var user = d.GetDeclaringType();
            while (a.Kind(user) != GenKind.User && !a.MD.GetTypeDefinition(user).GetDeclaringType().IsNil) user = a.MD.GetTypeDefinition(user).GetDeclaringType();
            string holder = a.TypeName(user);
            var r = new IL.Reader(il);
            for (int index = 0; r.Read() && index < code.Count; index++)
            {
                if (Api(r) is not { } api || code[index].Pops < 1) continue;
                // The name is the first argument after the object.
                string? argument = Argument(code, index, code[index].Pops, 1);
                string? literal = argument != null && argument.Split('\n').Length == 1 && argument.StartsWith("72 S:", StringComparison.Ordinal) ? argument["72 S:".Length..] : null;
                calls.Add(new NameCall(m, holder, api, literal));
            }
        }
        return calls;
    }

    void MethodRemoved(MethodDefinitionHandle method, TypeDefinitionHandle newType)
    {
        var d = o.MD.GetMethodDefinition(method);
        string name = o.MD.GetString(d.Name), id = o.MemberId(method);
        if (netcode && Netcode.IsRpc(o, d))
        {
            rpcReported.Add(id);
            Refuse("method_removed", id, Codes.NetcodeRpcChanged, $"The RPC {id} was removed or renamed, or its parameters changed; the Player keeps it registered, and other peers may still send it.");
            return;
        }
        if ((d.Attributes & MethodAttributes.Virtual) != 0)
        {
            Refuse("method_removed", id, Codes.VirtualMethodRemoved, $"The removed method {id} is virtual, an override or an interface implementation; the loaded class keeps dispatching to it.");
            return;
        }
        if (n.IsInterface(newType)) { Refuse("method_removed", id, Codes.InterfaceChanged, $"{id} was removed from an interface."); return; }
        if (name == ".cctor")
        {
            Refuse("method_removed", id, Codes.StaticConstructorChanged, $"The static constructor or the last static field initializer of {n.TypeName(newType)} was removed; the loaded one already ran or will still run.");
            return;
        }
        bool instance = (d.Attributes & MethodAttributes.Static) == 0;
        if (!instance || !UnityTypes.Messages.Contains(name) || Unity(newType).Kind == UnityKind.No)
        {
            Add("method_removed", id, Actions.None, message: "The loaded method stays; new code no longer calls it.").OldMethod = method;
            // A coroutine, iterator or async call of it that runs keeps running the removed code,
            // which a restart would not have.
            var closure = Old(method);
            if (closure.Types.Any(t => o.IsStateMachine(t)))
                Add("method_removed", id, Actions.Note).Notes.Add((Codes.RunningCoroutinesKeepOldCode,
                    $"{id} was removed or renamed; a coroutine, iterator or async call of it that is already running keeps running its old code until it ends."));
            return;
        }
        // The engine keeps calling the loaded message. An empty replacement equals "no such method"
        // only when nothing comes back from it and no base class would take over.
        var info = Unity(newType);
        bool returnsVoid = d.DecodeSignature(o.Plain, null).ReturnType == "System.Void";
        string? obstacle =
            !returnsVoid ? "it returns a value" :
            UnityTypes.PresenceMatters.Contains(name) ? "the engine works differently while a class declares this message, so an empty method does not equal its removal" :
            d.GetGenericParameters().Count > 0 || n.IsGenericType(newType) ? "it is generic or belongs to a generic type" :
            !info.ChainRead ? "a base class outside the readable assemblies may declare the same message" :
            info.BaseMethods.Contains(name) ? "a base class declares the same message, which the engine would call after a recompile" : null;
        if (obstacle != null)
        {
            Refuse("method_removed", id, Codes.UnityMessageRemoved, $"The removed Unity message {id} cannot be switched off: {obstacle}.");
            return;
        }
        var change = Add("method_removed", id, Actions.Patch, message: "The engine keeps calling the loaded message; it is redirected to an empty method.");
        change.OldMethod = method;
        change.NewType = newType;
    }

    static bool VariableArguments(Asm a, MethodDefinition d) =>
        a.MD.GetBlobReader(d.Signature).ReadSignatureHeader().CallingConvention == SignatureCallingConvention.VarArgs;

    static string ParamText(Asm a, MethodDefinition d)
    {
        var sb = new StringBuilder();
        foreach (var ph in d.GetParameters())
        {
            var p = a.MD.GetParameter(ph);
            sb.Append(p.SequenceNumber).Append(':').Append(a.MD.GetString(p.Name)).Append(':').Append((int)p.Attributes).Append(':').Append(ConstantText(a, p.GetDefaultValue())).Append(';');
        }
        return sb.ToString();
    }

    static Dictionary<int, List<string>> ParamAttributes(Asm a, MethodDefinition d)
    {
        var map = new Dictionary<int, List<string>>();
        foreach (var ph in d.GetParameters())
        {
            var p = a.MD.GetParameter(ph);
            map[p.SequenceNumber] = AttributeTexts(a, p.GetCustomAttributes());
        }
        return map;
    }

    /// What the runtime's stub for a method without IL body is made from, besides the signature:
    /// direction and marshalling of every parameter and of the return value.
    static string StubText(Asm a, MethodDefinition d)
    {
        var sb = new StringBuilder();
        foreach (var ph in d.GetParameters())
        {
            var p = a.MD.GetParameter(ph);
            var direction = p.Attributes & (ParameterAttributes.In | ParameterAttributes.Out);
            var marshal = p.GetMarshallingDescriptor();
            if (direction == 0 && marshal.IsNil) continue;
            sb.Append(p.SequenceNumber).Append(':').Append((int)direction).Append(':').Append(marshal.IsNil ? "" : Convert.ToHexString(a.MD.GetBlobBytes(marshal))).Append(';');
        }
        return sb.ToString();
    }

    static string ImportText(Asm a, MethodDefinition d)
    {
        if ((d.Attributes & MethodAttributes.PinvokeImpl) == 0) return "";
        var import = d.GetImport();
        return a.MD.GetString(a.MD.GetModuleReference(import.Module).Name) + "!" + a.MD.GetString(import.Name) + ":" + (int)import.Attributes;
    }

    void DiffMethod(MethodDefinitionHandle oh, MethodDefinitionHandle nh, TypeDefinitionHandle nt)
    {
        var od = o.MD.GetMethodDefinition(oh);
        var nd = n.MD.GetMethodDefinition(nh);
        string id = n.MemberId(nh), name = n.MD.GetString(nd.Name);

        if ((od.Attributes & ~MethodAttributes.MemberAccessMask) != (nd.Attributes & ~MethodAttributes.MemberAccessMask))
            Refuse("method", id, Codes.MethodFlagsChanged, $"Flags of {id} changed from [{od.Attributes}] to [{nd.Attributes}].");
        else if (od.Attributes != nd.Attributes)
            Add("method", id, Actions.Note).Notes.Add((Codes.AccessibilityChanged, $"{id} changed accessibility; reflection keeps the loaded accessibility until a full recompile."));
        // Inlining hints change nothing a redirect depends on: the Player runs with inlining off.
        const MethodImplAttributes hints = MethodImplAttributes.NoInlining | MethodImplAttributes.AggressiveInlining | MethodImplAttributes.NoOptimization | MethodImplAttributes.AggressiveOptimization;
        if ((od.ImplAttributes & ~hints) != (nd.ImplAttributes & ~hints))
            Refuse("method", id, Codes.MethodFlagsChanged, $"Implementation flags of {id} changed from [{od.ImplAttributes}] to [{nd.ImplAttributes}].");
        if (GenericParams(o, od.GetGenericParameters()) != GenericParams(n, nd.GetGenericParameters()))
            Refuse("method", id, Codes.GenericMethod, $"Generic constraints of {id} changed.");
        CompareAttributes("method", id, od.GetCustomAttributes(), nd.GetCustomAttributes());
        if (ParamText(o, od) != ParamText(n, nd))
            Add("parameter", id, Actions.None, message: "Parameter names, defaults or flags are metadata for compilers; callers were recompiled and are compared on their own.");
        var oldParams = ParamAttributes(o, od);
        var newParams = ParamAttributes(n, nd);
        foreach (int sequence in oldParams.Keys.Union(newParams.Keys).OrderBy(x => x))
            CompareAttributes("parameter", id + (sequence == 0 ? " return value" : " parameter " + sequence),
                oldParams.GetValueOrDefault(sequence) ?? new List<string>(), newParams.GetValueOrDefault(sequence) ?? new List<string>());
        if (ImportText(o, od) != ImportText(n, nd))
            Refuse("method", id, Codes.ExternMethodChanged, $"The P/Invoke import of {id} changed.");

        bool oldBody = od.RelativeVirtualAddress != 0, newBody = nd.RelativeVirtualAddress != 0;
        if (oldBody != newBody) { Refuse("method", id, Codes.ExternMethodChanged, $"{id} gained or lost its IL body."); return; }
        if (!oldBody)
        {
            // The runtime makes the code of such a method (a P/Invoke, the Invoke of a delegate) from
            // its metadata, once.
            if ((nd.Attributes & MethodAttributes.Abstract) == 0 && StubText(o, od) != StubText(n, nd))
                Refuse("method", id, Codes.ExternMethodChanged, $"The direction or marshalling of a parameter or of the return value of {id} changed; the runtime keeps the stub it made for the loaded method.");
            return;
        }
        if (netcode && !NetcodeRpc(oh, nh, od, nd, id)) return;
        var oc = Old(oh);
        var nc = New(nh);
        bool generic = nd.GetGenericParameters().Count > 0, genericType = n.IsGenericType(nt);
        if (oc.Hash.AsSpan().SequenceEqual(nc.Hash) && !(ForceAll && name != ".cctor" && !generic && !genericType && !n.IsInterface(nt) && !VariableArguments(n, nd))) return;

        if (name == ".cctor")
            Refuse("method_body", id, Codes.StaticConstructorChanged, $"The static constructor or a static field initializer of {n.TypeName(nt)} changed; it runs once when the type is first used.");
        else if (generic)
            Refuse("method_body", id, Codes.GenericMethod, $"{id} is a generic method; its code is shared between instantiations.");
        else if (genericType)
            Refuse("method_body", id, Codes.GenericType, $"{id} belongs to a generic type; its code is shared between instantiations.");
        else if (n.IsInterface(nt))
            Refuse("method_body", id, Codes.InterfaceChanged, $"{id} is a method body of an interface.");
        else if (VariableArguments(n, nd))
            Refuse("method_body", id, Codes.EmitNotSupported, $"{id} takes a variable argument list; the applier does not redirect such a method.");
        else
        {
            var change = Add("method_body", id, Actions.Patch);
            change.OldMethod = oh;
            change.NewMethod = nh;
            change.Revision = Short(nc.Hash);
            if (name == ".ctor")
                change.Notes.Add((Codes.ConstructorAppliesToNewObjects, $"{id} and the instance field initializers apply to objects constructed after the patch; existing objects keep their values."));
            if (StaleStateMachines(oc, nc))
                change.Notes.Add((Codes.RunningCoroutinesKeepOldCode, $"Calls of {id} made after the patch run the new code; a coroutine, iterator or async call that is already running finishes on the old code."));
            bool lambdasOnly = MapLambdas(change, id, oh, nh, oc, nc);
            // The engine, the start-up and Netcode call some methods once for what exists, and a
            // method only they call runs no more often. An edit that lies only in lambdas the
            // method made, each redirected, reaches the delegates it subscribed: it is live
            // (builder6-8).
            if (!lambdasOnly && OnceNote(nh, id) is { } once) change.Notes.Insert(0, once);
        }
    }

    static string Short(byte[] hash) => Convert.ToHexString(hash, 0, 8).ToLowerInvariant();

    // ---- methods that run once ---------------------------------------------------------------

    /// `TypeInitialized`: a static constructor, which runs when its type is first used.
    /// `Constructed`: a constructor, which runs the field initializers too, once per object.
    /// `Inherited`: an override of a method another assembly of the game declares and calls only
    /// from methods that run once (`Callers.Inherited`).
    internal enum Once { None, Initializer, Started, Enabled, Loaded, Deserialized, Spawned, Authority, TypeInitialized, Constructed, Inherited }

    /// Whether the engine, the start-up, the runtime or Netcode calls the method once for what
    /// exists, and how. `unity` classifies a type of the new compilation.
    static Once RootOf(Workspace workspace, Asm a, bool netcode, Func<TypeDefinitionHandle, UnityType> unity, Lazy<HashSet<MethodDefinitionHandle>> initialized, MethodDefinitionHandle m)
    {
        var d = a.MD.GetMethodDefinition(m);
        var t = d.GetDeclaringType();
        if (a.Kind(t) != GenKind.User) return Once.None;
        string name = a.MD.GetString(d.Name);
        // The runtime runs a static constructor when the type is first used, and a constructor
        // with the field initializers once per object (builder6-2).
        if (name == ".cctor") return Once.TypeInitialized;
        if (name == ".ctor") return Once.Constructed;
        if ((d.Attributes & MethodAttributes.Static) != 0)
            return !IsBridge(name) && initialized.Value.Contains(m) ? Once.Initializer : Once.None;
        // Netcode runs a spawn callback once per spawn; OnNetworkPreSpawn takes the
        // NetworkManager by reference (builder6-1).
        if (netcode && Netcode.SpawnCallback(name) && Netcode.Networked(a, t)) return Once.Spawned;
        if (d.DecodeSignature(a.Plain, null).ParameterTypes.Length != 0) return Once.None;
        // Start of a MonoBehaviour, Awake of a MonoBehaviour or a ScriptableObject: once per object.
        // An overload with parameters and the Start of another class are methods of the game.
        if (name == "Start" && unity(t).Starts) return Once.Started;
        if (name == "Awake" && unity(t).Awakes) return Once.Started;
        // OnEnable runs when an object is enabled, and for an asset when it is loaded.
        if (name == "OnEnable" && unity(t) is { Kind: not UnityKind.No } info)
            return info.Root is "UnityEngine.ScriptableObject" or "UnityEngine.StateMachineBehaviour" ? Once.Loaded : Once.Enabled;
        if ((name == "OnAfterDeserialize" || name.EndsWith(".ISerializationCallbackReceiver.OnAfterDeserialize", StringComparison.Ordinal))
            && Implements(workspace, a, t, "UnityEngine.ISerializationCallbackReceiver"))
            return Once.Deserialized;
        // NetworkTransform reads the authority of an object when it is spawned and keeps it.
        if (netcode && name == "OnIsServerAuthoritative" && DerivesFrom(workspace, a, t, "Unity.Netcode.Components.NetworkTransform")) return Once.Authority;
        return Once.None;
    }

    /// The static methods the start-up runs once: those with [RuntimeInitializeOnLoadMethod], and
    /// those a start-up bridge of the lifecycle rewrite calls.
    internal static HashSet<MethodDefinitionHandle> Initialized(Asm a)
    {
        var set = new HashSet<MethodDefinitionHandle>();
        foreach (var m in a.MD.MethodDefinitions)
        {
            var d = a.MD.GetMethodDefinition(m);
            foreach (var h in d.GetCustomAttributes())
                if (a.AttributeTypeName(a.MD.GetCustomAttribute(h)) == "UnityEngine.RuntimeInitializeOnLoadMethodAttribute") set.Add(m);
            if (!IsBridge(a.MD.GetString(d.Name)) || d.RelativeVirtualAddress == 0) continue;
            byte[] il = a.PE.GetMethodBody(d.RelativeVirtualAddress).GetILBytes() ?? Array.Empty<byte>();
            var r = new IL.Reader(il);
            while (r.Read())
                if (r.OpCode == IL.Call && MetadataTokens.EntityHandle(System.Buffers.Binary.BinaryPrimitives.ReadInt32LittleEndian(il.AsSpan(r.OperandOffset))) is { Kind: HandleKind.MethodDefinition } target)
                    set.Add((MethodDefinitionHandle)target);
        }
        return set;
    }

    /// The base types of a type, by name, as far as they can be read.
    static IEnumerable<(Asm Assembly, TypeDefinitionHandle Type, string Name)> Bases(Workspace workspace, Asm a, TypeDefinitionHandle type)
    {
        var t = type;
        for (int depth = 0; depth < 64; depth++)
        {
            EntityHandle parent = a.MD.GetTypeDefinition(t).BaseType;
            // An interface and <Module> have none.
            if (parent.IsNil) yield break;
            if (parent.Kind == HandleKind.TypeSpecification)
            {
                var blob = a.MD.GetBlobReader(a.MD.GetTypeSpecification((TypeSpecificationHandle)parent).Signature);
                if (blob.ReadByte() != 0x15) yield break;
                blob.ReadByte();
                parent = blob.ReadTypeHandle();
            }
            if (parent.Kind == HandleKind.TypeDefinition) t = (TypeDefinitionHandle)parent;
            else if (parent.Kind == HandleKind.TypeReference)
            {
                var reference = (TypeReferenceHandle)parent;
                string name = a.TypeRefName(reference, bare: true), home = a.TypeRefAssembly(reference);
                if (UnityTypes.IsClassLibrary(home) || home.Length == 0 || workspace.Resolve(home) is not { } next || !next.Types.TryGetValue(name, out var found))
                {
                    yield return (a, default, name);
                    yield break;
                }
                (a, t) = (next, found);
            }
            else yield break;
            yield return (a, t, a.TypeName(t));
        }
    }

    static bool DerivesFrom(Workspace workspace, Asm a, TypeDefinitionHandle type, string name) => Bases(workspace, a, type).Any(x => x.Name == name);

    static bool Implements(Workspace workspace, Asm a, TypeDefinitionHandle type, string name)
    {
        bool Own(Asm x, TypeDefinitionHandle t) => x.MD.GetTypeDefinition(t).GetInterfaceImplementations()
            .Any(i => x.HandleText(x.MD.GetInterfaceImplementation(i).Interface, bare: true) == name);
        return Own(a, type) || Bases(workspace, a, type).Any(x => !x.Type.IsNil && Own(x.Assembly, x.Type));
    }

    /// Which methods of the new compilation run only when methods that run once run: every
    /// caller in the game's code is such a method or one of these. A method a delegate can reach,
    /// a method with an attribute, a virtual public method, a message of the engine, a method of
    /// that name another assembly of the game calls by name and a public method another assembly
    /// calls can be called from where the assembly does not show, and are none of them. A call by
    /// a name written in the code (`Invoke(nameof(LateInit))`) is a call of the methods of that
    /// name (builder6-3).
    internal sealed class Callers
    {
        public readonly Dictionary<MethodDefinitionHandle, HashSet<MethodDefinitionHandle>> Of = new();
        public readonly Dictionary<MethodDefinitionHandle, Once> Roots = new();
        public readonly HashSet<MethodDefinitionHandle> OnlyOnce = new();
        /// Overrides of a method that another assembly of the game declares and calls only from
        /// methods that run once there: that assembly, its analysis and the declaration.
        public readonly Dictionary<MethodDefinitionHandle, (Asm Assembly, Callers Theirs, MethodDefinitionHandle Slot)> Inherited = new();
    }

    internal static Callers BuildCallers(Workspace workspace, Asm a, bool netcode, Lazy<HashSet<MethodDefinitionHandle>> initialized)
    {
        var result = new Callers();
        var types = new Dictionary<int, UnityType>();
        UnityType Classify(TypeDefinitionHandle t)
        {
            int token = MetadataTokens.GetToken(t);
            if (!types.TryGetValue(token, out var info)) types[token] = info = UnityTypes.Classify(workspace, a, t);
            return info;
        }
        // Virtual methods by name and signature: a call of one may land in any of them.
        var virtuals = new Dictionary<string, List<MethodDefinitionHandle>>(StringComparer.Ordinal);
        // User methods by name, for calls by a name the code writes.
        var named = new Dictionary<string, List<MethodDefinitionHandle>>(StringComparer.Ordinal);
        foreach (var m in a.MD.MethodDefinitions)
        {
            var d = a.MD.GetMethodDefinition(m);
            if (a.Kind(d.GetDeclaringType()) == GenKind.User)
            {
                string name = a.MD.GetString(d.Name);
                if (!named.TryGetValue(name, out var same)) named[name] = same = new List<MethodDefinitionHandle>();
                same.Add(m);
            }
            if ((d.Attributes & MethodAttributes.Virtual) == 0) continue;
            string key = a.BareMethodKey(m);
            if (!virtuals.TryGetValue(key, out var list)) virtuals[key] = list = new List<MethodDefinitionHandle>();
            list.Add(m);
        }
        var escaped = new HashSet<MethodDefinitionHandle>();
        void Edge(MethodDefinitionHandle caller, MethodDefinitionHandle target)
        {
            if (!result.Of.TryGetValue(target, out var set)) result.Of[target] = set = new HashSet<MethodDefinitionHandle>();
            set.Add(caller);
        }
        foreach (var m in a.MD.MethodDefinitions)
        {
            var d = a.MD.GetMethodDefinition(m);
            if (d.RelativeVirtualAddress == 0) continue;
            byte[] il = a.PE.GetMethodBody(d.RelativeVirtualAddress).GetILBytes() ?? Array.Empty<byte>();
            var r = new IL.Reader(il);
            while (r.Read())
            {
                if (r.Operand is not (System.Reflection.Emit.OperandType.InlineMethod or System.Reflection.Emit.OperandType.InlineTok)) continue;
                var h = MetadataTokens.EntityHandle(System.Buffers.Binary.BinaryPrimitives.ReadInt32LittleEndian(il.AsSpan(r.OperandOffset)));
                if (h.Kind == HandleKind.MethodSpecification) h = a.MD.GetMethodSpecification((MethodSpecificationHandle)h).Method;
                string? key = null;
                var targets = new List<MethodDefinitionHandle>();
                if (h.Kind == HandleKind.MethodDefinition)
                {
                    var target = (MethodDefinitionHandle)h;
                    targets.Add(target);
                    if ((a.MD.GetMethodDefinition(target).Attributes & MethodAttributes.Virtual) != 0) key = a.BareMethodKey(target);
                }
                else if (h.Kind == HandleKind.MemberReference && a.MD.GetMemberReference((MemberReferenceHandle)h) is { } mr && mr.GetKind() == MemberReferenceKind.Method)
                    key = a.MD.GetString(mr.Name) + Asm.SigText(mr.DecodeMethodSignature(a.Bare, null));
                // A virtual call lands in an override; a plain call of a virtual method (base.M())
                // lands in that method only.
                if (key != null && r.OpCode is IL.Callvirt or 0xFE07 && virtuals.TryGetValue(key, out var overrides)) targets.AddRange(overrides);
                foreach (var target in targets)
                {
                    if (r.OpCode is IL.Call or IL.Callvirt or 0x73) Edge(m, target);
                    else escaped.Add(target); // ldftn, ldvirtftn, ldtoken: a delegate or reflection
                }
            }
        }
        // A call by a name the code writes reaches the methods of that name.
        foreach (var call in workspace.NameCalls(a))
            if (call.Name != null && named.TryGetValue(call.Name, out var targets))
                foreach (var target in targets) Edge(call.Caller, target);
        // What the other assemblies of the game call by name or by reference.
        var byName = new HashSet<string>(StringComparer.Ordinal);
        var external = new HashSet<string>(StringComparer.Ordinal);
        if (workspace.Find(a.Name) is { } self)
            foreach (var other in workspace.Others(self))
            {
                foreach (var call in workspace.NameCalls(other)) if (call.Name != null) byName.Add(call.Name);
                foreach (var h in other.MD.MemberReferences)
                {
                    var mr = other.MD.GetMemberReference(h);
                    if (mr.GetKind() != MemberReferenceKind.Method || mr.Parent.Kind != HandleKind.TypeReference) continue;
                    var parent = (TypeReferenceHandle)mr.Parent;
                    if (other.TypeRefAssembly(parent) == a.Name) external.Add(other.TypeRefName(parent, bare: true) + "::" + other.MD.GetString(mr.Name));
                }
            }
        foreach (var m in a.MD.MethodDefinitions)
        {
            var root = RootOf(workspace, a, netcode, Classify, initialized, m);
            if (root != Once.None) result.Roots[m] = root;
        }
        // The declaration of an override whose slot a type outside the assembly declares.
        (Asm, MethodDefinitionHandle)? Slot(MethodDefinitionHandle m, MethodDefinition d)
        {
            if ((d.Attributes & MethodAttributes.Virtual) == 0 || (d.Attributes & MethodAttributes.NewSlot) != 0) return null;
            string key = a.BareMethodKey(m);
            foreach (var (assembly, type, _) in Bases(workspace, a, d.GetDeclaringType()))
            {
                if (type.IsNil) return null;
                foreach (var other in assembly.MD.GetTypeDefinition(type).GetMethods())
                {
                    if (assembly.BareMethodKey(other) != key) continue;
                    if ((assembly.MD.GetMethodDefinition(other).Attributes & MethodAttributes.NewSlot) == 0) break;
                    return assembly == a ? null : (assembly, other);
                }
            }
            return null;
        }
        // An override that only methods running once in another assembly of the game call (an
        // abstract hook a base class's Start calls) runs once too.
        foreach (var m in a.MD.MethodDefinitions)
        {
            var d = a.MD.GetMethodDefinition(m);
            if (result.Roots.ContainsKey(m) || a.Kind(d.GetDeclaringType()) != GenKind.User) continue;
            if (Slot(m, d) is not { } found) continue;
            var (assembly, slot) = found;
            if (UnityTypes.IsEngine(assembly.Name) || UnityTypes.IsClassLibrary(assembly.Name)) continue;
            // Its code is read, so the whole image; a handle names the same row in either.
            assembly = workspace.Code(assembly);
            var theirs = workspace.Callers(assembly);
            if (!theirs.OnlyOnce.Contains(slot)) continue;
            result.Roots[m] = Once.Inherited;
            result.Inherited[m] = (assembly, theirs, slot);
        }
        // Declared outside the assembly: the override chain of a virtual method starts in a type
        // this assembly does not have.
        bool Foreign(MethodDefinitionHandle m, MethodDefinition d)
        {
            if ((d.Attributes & MethodAttributes.Virtual) == 0) return false;
            if ((d.Attributes & MethodAttributes.NewSlot) != 0) return false;
            string key = a.BareMethodKey(m);
            foreach (var (assembly, type, _) in Bases(workspace, a, d.GetDeclaringType()))
            {
                if (assembly != a || type.IsNil) return true;
                foreach (var other in a.MD.GetTypeDefinition(type).GetMethods())
                    if (a.BareMethodKey(other) == key && (a.MD.GetMethodDefinition(other).Attributes & MethodAttributes.NewSlot) != 0) return false;
            }
            return true;
        }
        var candidates = new HashSet<MethodDefinitionHandle>();
        foreach (var (m, callers) in result.Of)
        {
            var d = a.MD.GetMethodDefinition(m);
            var t = d.GetDeclaringType();
            string name = a.MD.GetString(d.Name);
            if (result.Roots.ContainsKey(m) || a.Kind(t) != GenKind.User || Canon.Owned(name, d.Attributes) || escaped.Contains(m)) continue;
            bool exposed = (d.Attributes & MethodAttributes.MemberAccessMask) == MethodAttributes.Public;
            // A public slot is called through an interface or a base class from anywhere.
            if (exposed && (d.Attributes & MethodAttributes.Virtual) != 0) continue;
            if (byName.Contains(name) || exposed && external.Contains(a.TypeName(t) + "::" + name)) continue;
            if (UnityTypes.Messages.Contains(name) && Classify(t).Kind != UnityKind.No) continue;
            // An attribute is how a framework finds a method to call: the bridge runs an
            // [InvokeOnHotReloadLocal] method again after a patch, for one.
            if (AuthoredAttributes(a, d.GetCustomAttributes()).Count > 0) continue;
            if (Foreign(m, d)) continue;
            candidates.Add(m);
        }
        // The largest set in which every caller is a root or a member.
        var only = new HashSet<MethodDefinitionHandle>(candidates);
        for (bool changed = true; changed;)
        {
            changed = false;
            foreach (var m in only.ToList())
                if (result.Of[m].Any(c => !result.Roots.ContainsKey(c) && !only.Contains(c))) { only.Remove(m); changed = true; }
        }
        result.OnlyOnce.UnionWith(only);
        return result;
    }

    /// The methods that run once and lead to `m` through methods only they call, by how they run
    /// and as the member a user wrote (an override's leads through its declaration in the other
    /// assembly, named with that assembly).
    static void Reached(Asm a, Callers callers, MethodDefinitionHandle m, HashSet<Once> kinds, SortedSet<string> from, string suffix)
    {
        var seen = new HashSet<MethodDefinitionHandle> { m };
        var queue = new Queue<MethodDefinitionHandle>();
        queue.Enqueue(m);
        while (queue.Count > 0)
            foreach (var caller in callers.Of.GetValueOrDefault(queue.Dequeue()) ?? new HashSet<MethodDefinitionHandle>())
            {
                if (!seen.Add(caller)) continue;
                if (!callers.Roots.TryGetValue(caller, out var kind)) { queue.Enqueue(caller); continue; }
                if (kind == Once.Inherited && callers.Inherited.TryGetValue(caller, out var inherited))
                    Reached(inherited.Assembly, inherited.Theirs, inherited.Slot, kinds, from, " (" + inherited.Assembly.Name + ")");
                else
                {
                    kinds.Add(kind);
                    if (!IsBridge(a.MD.GetString(a.MD.GetMethodDefinition(caller).Name))) from.Add(a.MemberId(caller) + suffix);
                }
            }
    }

    /// The note for a changed method that runs once for what exists, or that only such methods
    /// call; null for any other method, and for a constructor or a static constructor, whose own
    /// change has a note or a refusal of its own.
    (string Code, string Message)? OnceNote(MethodDefinitionHandle m, string shown)
    {
        var callers = onceCallers.Value;
        var kinds = new HashSet<Once>();
        var reached = new SortedSet<string>(StringComparer.Ordinal);
        if (callers.Roots.TryGetValue(m, out var root))
        {
            switch (root)
            {
                case Once.Initializer: return (Codes.RuntimeInitializerChanged, $"{shown} is a [RuntimeInitializeOnLoadMethod] method: it ran when the Player started and does not run again, so its new body is not in effect before the next start; what the old body registered or set stays.");
                case Once.Started: return (Codes.StartedObjectsKeepState, $"{shown} does not run again for the objects that already ran it; its new body runs for objects that start after the patch.");
                case Once.Enabled: return (Codes.StartedObjectsKeepState, $"{shown} does not run again for the objects that are enabled; its new body runs when an object is enabled after the patch. What the old body subscribed or registered stays, and a changed OnDisable does not undo it.");
                case Once.Loaded: return (Codes.StartedObjectsKeepState, $"{shown} ran when the asset was loaded and does not run again for the assets that are loaded; its new body runs for assets loaded or created after the patch.");
                case Once.Deserialized: return (Codes.StartedObjectsKeepState, $"{shown} ran when the objects that exist were deserialized and does not run again for them; its new body runs for objects loaded or instantiated after the patch.");
                case Once.Spawned: return (Codes.SpawnedObjectsKeepState, $"{shown} does not run again for the objects that are spawned; its new body runs for objects spawned after the patch.");
                case Once.Authority: return (Codes.SpawnedObjectsKeepState, $"{shown} is read by NetworkTransform when an object is spawned, which keeps that authority, and again on later calls: the objects that are spawned mix the old and the new answer; objects spawned after the patch take the new one.");
                case Once.Inherited when callers.Inherited.TryGetValue(m, out var inherited):
                    Reached(inherited.Assembly, inherited.Theirs, inherited.Slot, kinds, reached, " (" + inherited.Assembly.Name + ")");
                    break;
                default: return null;
            }
        }
        else if (callers.OnlyOnce.Contains(m)) Reached(n, callers, m, kinds, reached, "");
        else return null;
        if (kinds.Count == 0) return null;
        string from = string.Join(", ", reached.Take(3)) + (reached.Count > 3 ? $" and {reached.Count - 3} more" : "");
        bool one = reached.Count == 1;
        // A public method may also be called by saved data (a UnityEvent), which the code does not show.
        var d = n.MD.GetMethodDefinition(m);
        string only = (d.Attributes & MethodAttributes.MemberAccessMask) == MethodAttributes.Public ? "in the game's code only" : "only";
        string data = only == "only" ? "" : " A call that saved data makes (a UnityEvent of a scene) runs the new body.";
        if (kinds.Contains(Once.Initializer) || kinds.Contains(Once.TypeInitialized))
        {
            string when = kinds.Contains(Once.Initializer) && kinds.Contains(Once.TypeInitialized) ? "when the Player started or when a type was first used"
                : kinds.Contains(Once.Initializer) ? "when the Player started" : "when its type was first used";
            return (Codes.RuntimeInitializerChanged, $"{shown} is called {only} from {from}, which ran {when} and {(one ? "does" : "do")} not run again; what it set stays, and its new body is not in effect before the next start.{data}");
        }
        if (kinds.Count == 1 && kinds.Contains(Once.Spawned))
            return (Codes.SpawnedObjectsKeepState, $"{shown} is called {only} from {from}, which Netcode does not run again for the objects that are spawned; its new body runs for objects spawned after the patch.{data}");
        if (kinds.Count == 1 && kinds.Contains(Once.Constructed))
            return (Codes.ConstructorAppliesToNewObjects, $"{shown} is called {only} from {from}, which ran when the objects that exist were constructed (a constructor runs the field initializers too); they keep what it gave them, and its new body runs for objects constructed after the patch.{data}");
        return (Codes.StartedObjectsKeepState, $"{shown} is called {only} from {from}, which {(one ? "does" : "do")} not run again for the objects that already ran {(one ? "it" : "them")}; its new body runs for objects that start after the patch.{data}");
    }

    // ---- lambdas and state machines --------------------------------------------------------

    static string Name(Asm a, MethodDefinitionHandle m) => a.MD.GetString(a.MD.GetMethodDefinition(m).Name);

    /// The lambdas and local functions a method owns, with those of its closures and state machines.
    static List<MethodDefinitionHandle> Lambdas(Asm a, Closure closure)
    {
        var list = new List<MethodDefinitionHandle>();
        foreach (var m in closure.Methods) if (Canon.Lambda(Name(a, m))) list.Add(m);
        foreach (var t in closure.Types)
            if (a.Kind(t) == GenKind.Exclusive)
                foreach (var m in a.MD.GetTypeDefinition(t).GetMethods()) if (Canon.Lambda(Name(a, m))) list.Add(m);
        return list;
    }

    /// A state machine object that exists runs the code it was made with. That is worth telling
    /// when a state machine of the old method has no identical one in the new method.
    bool StaleStateMachines(Closure oc, Closure nc)
    {
        var olds = new List<string>();
        foreach (var t in oc.Types) if (o.IsStateMachine(t)) olds.Add(co.TypeHash(t));
        if (olds.Count == 0) return false;
        var news = new List<string>();
        foreach (var t in nc.Types) if (n.IsStateMachine(t)) news.Add(cn.TypeHash(t));
        return Without(olds, news).Count > 0;
    }

    /// Old lambdas and their new versions, as far as the pairing is proven.
    sealed class Pairing
    {
        public readonly Dictionary<MethodDefinitionHandle, MethodDefinitionHandle> Proven = new();
        /// Proven pairs whose old method runs code that the new one does not.
        public readonly List<(MethodDefinitionHandle Old, MethodDefinitionHandle New)> Edited = new();
        public readonly HashSet<MethodDefinitionHandle> Walked = new();
        public bool Conflict;
        /// The method itself, or a member it names that is never redirected on its own (a state
        /// machine), differs: the edit is not only in its lambdas.
        public bool Own;
    }

    /// Follows the code from a method into everything it creates, as long as both compilations
    /// agree. When two bodies are identical apart from the names of the generated members, the k-th
    /// member one of them names is the k-th member the other names: the same place of the same
    /// source. That is the only proof of which new lambda an old one became; an ordinal or a similar
    /// body is a guess. Below a body that differs nothing is paired.
    void Pair(Pairing p, MethodDefinitionHandle xo, MethodDefinitionHandle xn, bool lambda)
    {
        if (lambda)
        {
            if (p.Proven.TryGetValue(xo, out var known)) { p.Conflict |= known != xn; return; }
            p.Proven[xo] = xn;
        }
        else if (!p.Walked.Add(xo)) return;
        Closure so = co.Shallow(xo), sn = cn.Shallow(xn);
        // Its own code differs: it must be replaced, and what it creates has no proven partner.
        if (!so.Hash.AsSpan().SequenceEqual(sn.Hash)) { if (lambda) p.Edited.Add((xo, xn)); else p.Own = true; return; }
        if (so.Methods.Count != sn.Methods.Count || so.Locals.Count != sn.Locals.Count || so.Types.Count != sn.Types.Count) { p.Conflict = true; return; }

        // A member that is never redirected on its own (a state machine, a base call helper) makes
        // the method that names it stale when it differs: an async lambda is a stub that starts its
        // state machine, and the edit is in the state machine.
        bool stale = false;
        for (int i = 0; i < so.Methods.Count; i++)
        {
            MethodDefinitionHandle mo = so.Methods[i], mn = sn.Methods[i];
            if (o.Kind(o.MD.GetMethodDefinition(mo).GetDeclaringType()) == GenKind.Pid) continue; // named by its content
            bool oldLambda = Canon.Lambda(Name(o, mo)), newLambda = Canon.Lambda(Name(n, mn));
            if (oldLambda && newLambda) Pair(p, mo, mn, lambda: true);
            else stale |= oldLambda != newLambda || !co.Compute(mo).Hash.AsSpan().SequenceEqual(cn.Compute(mn).Hash);
        }
        for (int i = 0; i < so.Locals.Count; i++)
        {
            bool oldLambda = Canon.Lambda(Name(o, so.Locals[i])), newLambda = Canon.Lambda(Name(n, sn.Locals[i]));
            if (oldLambda && newLambda) Pair(p, so.Locals[i], sn.Locals[i], lambda: true);
            else stale |= oldLambda != newLambda;
        }
        for (int i = 0; i < so.Types.Count; i++)
        {
            TypeDefinitionHandle to = so.Types[i], tn = sn.Types[i];
            if (o.Kind(to) != n.Kind(tn) || o.IsClosure(to) != n.IsClosure(tn)) { stale = true; continue; }
            // The lambdas of a closure class are paired where they are created, and its layout is
            // compared when one of them is redirected.
            if (o.Kind(to) != GenKind.Exclusive || o.IsClosure(to)) continue;
            stale |= co.TypeHash(to) != cn.TypeHash(tn);
            // What a state machine creates is created by its named methods (MoveNext).
            var named = new Dictionary<string, MethodDefinitionHandle>(StringComparer.Ordinal);
            foreach (var m in o.MD.GetTypeDefinition(to).GetMethods()) if (!Canon.Generated(Name(o, m))) named[o.MethodKey(m)] = m;
            foreach (var m in n.MD.GetTypeDefinition(tn).GetMethods())
                if (!Canon.Generated(Name(n, m)) && named.Remove(n.MethodKey(m), out var old)) Pair(p, old, m, lambda: false);
        }
        if (stale && lambda) p.Edited.Add((xo, xn));
        else if (stale) p.Own = true;
    }

    /// What a redirect of a lambda needs both sides to agree on: the signature and what `this` points at.
    static string Shape(Asm a, Canon canon, MethodDefinitionHandle lambda)
    {
        var owner = a.MD.GetMethodDefinition(lambda).GetDeclaringType();
        return canon.LayoutSignature(lambda) + " @ " + (a.Kind(owner) == GenKind.User ? a.TypeName(owner) : canon.Layout(owner));
    }

    static bool Redirectable(Asm a, MethodDefinitionHandle lambda)
    {
        var d = a.MD.GetMethodDefinition(lambda);
        return d.GetGenericParameters().Count == 0 && !a.IsGenericType(d.GetDeclaringType());
    }

    /// A lambda as the k-th lambda of its method with the code it can run. The compiler numbers the
    /// lambdas and local functions of a method in source order.
    static string Positioned(Asm a, Canon canon, MethodDefinitionHandle lambda)
    {
        string name = Name(a, lambda);
        int ordinal = Canon.LambdaOrdinal(name);
        return (ordinal >= 0 ? ordinal.ToString(CultureInfo.InvariantCulture) : name) + ":" + canon.MemberHash(lambda);
    }

    /// Delegates created before the patch hold the OLD lambda methods. An old lambda is redirected
    /// to its new version only when the pairing is proven (see Pair) and both agree on signature
    /// and closure layout. Any other old lambda must be the same code at the same position among
    /// the lambdas of the new method, or the caller is told that old delegates keep old code.
    /// Returns whether the edit lies only in lambdas that are redirected to their new versions.
    bool MapLambdas(Change change, string id, MethodDefinitionHandle oh, MethodDefinitionHandle nh, Closure oc, Closure nc)
    {
        var olds = Lambdas(o, oc);
        if (olds.Count == 0) return false;
        var pairing = new Pairing();
        Pair(pairing, oh, nh, lambda: false);
        if (pairing.Conflict) { pairing.Proven.Clear(); pairing.Edited.Clear(); }

        var pairs = new List<LambdaPair>();
        bool unmatched = false, byReference = false;
        foreach (var (x, y) in pairing.Edited)
        {
            if (!Redirectable(o, x) || !Redirectable(n, y) || Shape(o, co, x) != Shape(n, cn, y)) unmatched = true;
            // The patch has its own copy of a closure type, and a redirect needs both sides to
            // declare the very same parameter types.
            else if (co.NamesCopiedType(x) || cn.NamesCopiedType(y)) byReference = true;
            else pairs.Add(new LambdaPair(x, y));
        }
        // Where the code that creates a lambda changed, nothing says which new lambda it became. An
        // old lambda is then current only if the new method has the very same code as its k-th
        // lambda too: a lambda that was edited, removed, or moved behind a new one is not.
        var orphans = new List<string>();
        foreach (var m in olds) if (!pairing.Proven.ContainsKey(m)) orphans.Add(Positioned(o, co, m));
        if (orphans.Count > 0)
        {
            var taken = new HashSet<MethodDefinitionHandle>(pairing.Proven.Values);
            var fresh = new List<string>();
            foreach (var m in Lambdas(n, nc)) if (!taken.Contains(m)) fresh.Add(Positioned(n, cn, m));
            unmatched |= Without(orphans, fresh).Count > 0;
        }
        change.Lambdas = pairs;
        if (unmatched)
            change.Notes.Add((Codes.LambdaNotRemapped, $"Lambdas or local functions of {id} were edited, added or removed and an old one cannot be redirected to its new version (the code that creates it changed too, or what it captures changed); delegates created before the patch keep the old body."));
        else if (byReference)
            change.Notes.Add((Codes.LambdaNotRemapped, $"A local function of {id} takes its captured variables by reference; code of {id} that is already running keeps calling the old version."));
        return !pairing.Conflict && !pairing.Own && !unmatched && !byReference && pairs.Count > 0;
    }

    /// Canonical text of one method in both assemblies, for `--explain`.
    public (string? Old, string? New) Explain(string memberId)
    {
        static string? Find(Asm a, Canon c, string id)
        {
            foreach (var h in a.MD.MethodDefinitions)
                if (a.MemberId(h) == id) return c.Compute(h, explain: true).Text;
            return null;
        }
        return (Find(o, co, memberId), Find(n, cn, memberId));
    }
}

/// What the code of an assembly can run: every method a user wrote, and every compiler-generated
/// method and type that a body which can run names, followed token by token. A member reference
/// through a type of the module means the methods of that name in that type; a reference this
/// class cannot place means every generated method of that name. So a generated method outside
/// `Methods` is one that no instruction of the assembly leads to.
internal sealed class Reach
{
    readonly Asm a;
    readonly MetadataReader md;
    readonly SymbolProvider names;
    readonly Queue<MethodDefinitionHandle> queue = new();
    Dictionary<string, List<MethodDefinitionHandle>>? generated;
    public readonly HashSet<MethodDefinitionHandle> Methods = new();
    public readonly HashSet<TypeDefinitionHandle> Types = new();

    public Reach(Asm a)
    {
        this.a = a;
        md = a.MD;
        names = new SymbolProvider(a, bare: true) { DefHook = h => { Type(h); return ""; } };
        foreach (var t in md.TypeDefinitions)
        {
            if (a.Kind(t) != GenKind.User) continue;
            foreach (var m in md.GetTypeDefinition(t).GetMethods())
            {
                var d = md.GetMethodDefinition(m);
                if (!Canon.Owned(md.GetString(d.Name), d.Attributes)) Method(m);
            }
        }
        while (queue.Count > 0) Scan(queue.Dequeue());
    }

    void Method(MethodDefinitionHandle m)
    {
        if (!Methods.Add(m)) return;
        queue.Enqueue(m);
        Type(md.GetMethodDefinition(m).GetDeclaringType());
    }

    void Type(TypeDefinitionHandle t)
    {
        if (!Types.Add(t)) return;
        var td = md.GetTypeDefinition(t);
        switch (a.Kind(t))
        {
            // An object of a closure class or a state machine runs any of its methods: the
            // comparison takes such a type whole, too.
            case GenKind.Exclusive:
                foreach (var m in td.GetMethods()) Method(m);
                foreach (var nested in td.GetNestedTypes()) Type(nested);
                break;
            // The class that pools the lambdas of a type: only its instance is made here.
            case GenKind.Shared:
                foreach (var m in td.GetMethods())
                    if (md.GetString(md.GetMethodDefinition(m).Name) is ".ctor" or ".cctor") Method(m);
                break;
        }
    }

    void Scan(MethodDefinitionHandle m)
    {
        var d = md.GetMethodDefinition(m);
        d.DecodeSignature(names, null);
        if (d.RelativeVirtualAddress == 0) return;
        var body = a.PE.GetMethodBody(d.RelativeVirtualAddress);
        if (!body.LocalSignature.IsNil) md.GetStandaloneSignature(body.LocalSignature).DecodeLocalSignature(names, null);
        byte[] il = body.GetILBytes() ?? Array.Empty<byte>();
        var r = new IL.Reader(il);
        while (r.Read())
        {
            if (!IL.IsToken(r.Operand) || r.Operand == System.Reflection.Emit.OperandType.InlineString) continue;
            Token(MetadataTokens.EntityHandle(System.Buffers.Binary.BinaryPrimitives.ReadInt32LittleEndian(il.AsSpan(r.OperandOffset))));
        }
        foreach (var region in body.ExceptionRegions) if (!region.CatchType.IsNil) Token(region.CatchType);
    }

    void Token(EntityHandle h)
    {
        switch (h.Kind)
        {
            case HandleKind.TypeDefinition: Type((TypeDefinitionHandle)h); break;
            case HandleKind.TypeSpecification: md.GetTypeSpecification((TypeSpecificationHandle)h).DecodeSignature(names, null); break;
            case HandleKind.MethodDefinition: Method((MethodDefinitionHandle)h); break;
            case HandleKind.FieldDefinition: Type(md.GetFieldDefinition((FieldDefinitionHandle)h).GetDeclaringType()); break;
            case HandleKind.MemberReference: Member(md.GetMemberReference((MemberReferenceHandle)h)); break;
            case HandleKind.MethodSpecification:
            {
                var ms = md.GetMethodSpecification((MethodSpecificationHandle)h);
                ms.DecodeSignature(names, null);
                Token(ms.Method);
                break;
            }
            case HandleKind.StandaloneSignature:
            {
                var ss = md.GetStandaloneSignature((StandaloneSignatureHandle)h);
                if (ss.GetKind() == StandaloneSignatureKind.Method) ss.DecodeMethodSignature(names, null);
                break;
            }
        }
    }

    void Member(MemberReference mr)
    {
        TypeDefinitionHandle type = default;
        bool foreign = false;
        switch (mr.Parent.Kind)
        {
            case HandleKind.TypeDefinition:
                type = (TypeDefinitionHandle)mr.Parent;
                break;
            case HandleKind.TypeSpecification:
            {
                var specification = md.GetTypeSpecification((TypeSpecificationHandle)mr.Parent);
                specification.DecodeSignature(names, null);
                var blob = md.GetBlobReader(specification.Signature);
                if (blob.ReadSignatureTypeCode() != SignatureTypeCode.GenericTypeInstance || blob.ReadSignatureTypeCode() != SignatureTypeCode.TypeHandle) break;
                var generic = blob.ReadTypeHandle();
                if (generic.Kind == HandleKind.TypeDefinition) type = (TypeDefinitionHandle)generic;
                else if (generic.Kind == HandleKind.TypeReference) foreign = a.TypeRefAssembly((TypeReferenceHandle)generic).Length > 0;
                break;
            }
            case HandleKind.TypeReference:
                foreign = a.TypeRefAssembly((TypeReferenceHandle)mr.Parent).Length > 0;
                break;
            // The call site of a method with a variable argument list.
            case HandleKind.MethodDefinition:
                Method((MethodDefinitionHandle)mr.Parent);
                return;
            case HandleKind.ModuleReference:
                foreign = true;
                break;
        }
        if (!type.IsNil) Type(type);
        if (mr.GetKind() != MemberReferenceKind.Method) return;
        mr.DecodeMethodSignature(names, null);
        string name = md.GetString(mr.Name);
        if (!type.IsNil)
        {
            foreach (var m in md.GetTypeDefinition(type).GetMethods())
                if (md.StringComparer.Equals(md.GetMethodDefinition(m).Name, name)) Method(m);
            return;
        }
        if (foreign || !Canon.Generated(name)) return;
        if (generated == null)
        {
            generated = new Dictionary<string, List<MethodDefinitionHandle>>(StringComparer.Ordinal);
            foreach (var m in md.MethodDefinitions)
            {
                string own = md.GetString(md.GetMethodDefinition(m).Name);
                if (!Canon.Generated(own)) continue;
                if (!generated.TryGetValue(own, out var list)) generated[own] = list = new List<MethodDefinitionHandle>();
                list.Add(m);
            }
        }
        if (generated.TryGetValue(name, out var same)) foreach (var m in same) Method(m);
    }
}

internal enum ShapeKind { Other, Primitive, Definition, Reference, Vector, Generic, Parameter }

/// The type of a field as far as Unity's serializer tells types apart.
internal sealed class Shape
{
    public ShapeKind Kind;
    public PrimitiveTypeCode Code;
    public TypeDefinitionHandle Definition;
    public TypeReferenceHandle Reference;
    /// Written as a value type in the signature.
    public bool ValueType;
    /// The element of an array, or the generic type of an instantiation.
    public Shape? Element;
    public System.Collections.Immutable.ImmutableArray<Shape> Arguments = System.Collections.Immutable.ImmutableArray<Shape>.Empty;
}

internal sealed class Shapes : ISignatureTypeProvider<Shape, object?>
{
    public static readonly Shapes Instance = new();
    static readonly Shape other = new();

    public Shape GetPrimitiveType(PrimitiveTypeCode c) => new() { Kind = ShapeKind.Primitive, Code = c };
    public Shape GetTypeFromDefinition(MetadataReader r, TypeDefinitionHandle h, byte rawTypeKind) => new() { Kind = ShapeKind.Definition, Definition = h, ValueType = rawTypeKind == 0x11 };
    public Shape GetTypeFromReference(MetadataReader r, TypeReferenceHandle h, byte rawTypeKind) => new() { Kind = ShapeKind.Reference, Reference = h, ValueType = rawTypeKind == 0x11 };
    public Shape GetTypeFromSpecification(MetadataReader r, object? ctx, TypeSpecificationHandle h, byte rawTypeKind) => r.GetTypeSpecification(h).DecodeSignature(this, ctx);
    public Shape GetSZArrayType(Shape e) => new() { Kind = ShapeKind.Vector, Element = e };
    public Shape GetArrayType(Shape e, ArrayShape s) => other;
    public Shape GetByReferenceType(Shape e) => other;
    public Shape GetPointerType(Shape e) => other;
    public Shape GetPinnedType(Shape e) => e;
    public Shape GetGenericInstantiation(Shape g, System.Collections.Immutable.ImmutableArray<Shape> args) => new() { Kind = ShapeKind.Generic, Element = g, Arguments = args };
    public Shape GetGenericTypeParameter(object? ctx, int i) => new() { Kind = ShapeKind.Parameter };
    public Shape GetGenericMethodParameter(object? ctx, int i) => new() { Kind = ShapeKind.Parameter };
    public Shape GetFunctionPointerType(MethodSignature<Shape> s) => other;
    public Shape GetModifiedType(Shape modifier, Shape unmodified, bool isRequired) => unmodified;
}
