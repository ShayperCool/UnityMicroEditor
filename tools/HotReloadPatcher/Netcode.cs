// Netcode for GameObjects: what its post-processor writes into a game assembly, and which of it a
// running Player cannot take over.
//
// tools/NetcodePostProcessor runs the package's own CodeGen on every assembly whose compiled image
// references Unity.Netcode.Runtime. Read from an image of com.unity.netcode.gameobjects 2.13.2:
//   - every class below NetworkBehaviour gets three virtual methods: `__initializeRpcs` registers
//     the handlers of its RPCs by id, once per class, when its first object is initialized;
//     `__initializeVariables` lists the NetworkVariable fields of an object, once per object;
//     `__getTypeName` returns the name of the class;
//   - every RPC gets a static `__rpc_handler_<id>` that reads the parameters and calls it, and its
//     own body gets the code that writes the parameters and sends them put in front of it. That
//     code ends with the first store into `NetworkBehaviour.__rpc_exec_stage`. The id is a hash
//     of the module name and the signature;
//   - the type `__GEN.NetworkVariableSerializationHelper` registers the serializers of the value
//     types of the network variables. The tool's helper adds `__GEN.UnityPlayNetcodeInitializers`,
//     through which the loader runs it when the Player starts.
// Nothing else of an image differs from the compiler's output, and the same input gives the same
// bytes.
//
// The rule: what is registered once, and what the two ends of a connection must agree on, is
// fixed in a running Player. That is the set of RPCs of a class with their ids, the code that
// writes and the code that reads the parameters of each, the list of network variables of a
// class, the registered serializers and the names of the classes. A difference in any of them is
// refused with a code of its own. Everything else is code like any other and is compared and
// patched as such, the body of an RPC included: the post-processed body of the new compilation is
// what a restarted Player would run, and the loaded handler keeps calling the loaded method, which
// the Player redirects.
//
// An image that is not of the form above (another version of the package, an assembly that was
// not post-processed, the Netcode runtime itself) says nothing about what was registered; every
// edit of it is refused.
using System.Buffers.Binary;
using System.Reflection.Metadata;
using System.Reflection.Metadata.Ecma335;

internal static class Netcode
{
    public const string Runtime = "Unity.Netcode.Runtime";
    public const string InitializeRpcs = "__initializeRpcs", InitializeVariables = "__initializeVariables", TypeName = "__getTypeName", Handler = "__rpc_handler_";
    /// The field whose first store ends the send code in front of an RPC body.
    public const string ExecStage = "Unity.Netcode.NetworkBehaviour::__rpc_exec_stage:";
    public const string GeneratedNamespace = "__GEN.", Serialization = "__GEN.NetworkVariableSerializationHelper", SerializationMethod = "InitializeSerialization";
    /// The type the tool's helper adds: its one method calls the registration above.
    public const string Bridge = "__GEN.UnityPlayNetcodeInitializers";
    const string Behaviour = "Unity.Netcode.NetworkBehaviour";
    /// How the canonical form ends the name of a static method without parameters or result.
    public const string Parameterless = "():System.Void static";

    /// The post-processor rewrites exactly the assemblies whose emitted image references the
    /// Netcode runtime: the compiler drops a reference that no code uses, and the helper returns
    /// such an image as it is. The project's assembly graph does not tell: there Assembly-CSharp
    /// references every auto-referenced assembly, whatever its code uses.
    public static bool Referenced(Asm a)
    {
        foreach (var h in a.MD.AssemblyReferences)
            if (a.MD.GetString(a.MD.GetAssemblyReference(h).Name) == Runtime) return true;
        return false;
    }

    /// The image is one the rules of this file are about: it references the runtime, or is it.
    public static bool Uses(Asm a) => a.Name == Runtime || Referenced(a);

    public static bool IsRpc(Asm a, MethodDefinition d)
    {
        foreach (var h in d.GetCustomAttributes())
            if (a.AttributeTypeName(a.MD.GetCustomAttribute(h)) is "Unity.Netcode.RpcAttribute" or "Unity.Netcode.ServerRpcAttribute" or "Unity.Netcode.ClientRpcAttribute") return true;
        return false;
    }

    /// A member the post-processor adds to a class. Its names start with two underscores, which
    /// the language reserves; `__UnityPlay_` is the lifecycle rewrite's, and those are compared
    /// like methods the user wrote.
    public static bool Generated(string name) => name.StartsWith("__", StringComparison.Ordinal) && !name.StartsWith("__UnityPlay_", StringComparison.Ordinal);

    /// Callbacks Netcode runs once for an object that is spawned.
    public static bool SpawnCallback(string name) => name is "OnNetworkSpawn" or "OnNetworkPreSpawn" or "OnNetworkPostSpawn" or "OnNetworkSessionSynchronized" or "OnInSceneObjectsSpawned";

    static bool Declares(Asm a, TypeDefinitionHandle type, string name)
    {
        foreach (var m in a.MD.GetTypeDefinition(type).GetMethods())
            if (a.MD.StringComparer.Equals(a.MD.GetMethodDefinition(m).Name, name)) return true;
        return false;
    }

    /// The post-processor took the class for a NetworkBehaviour.
    public static bool Networked(Asm a, TypeDefinitionHandle type) => Declares(a, type, InitializeRpcs);

    /// Why the image is not of the form this file describes; null when it is, and for an image
    /// that does not use Netcode.
    public static string? Unknown(Asm a, Canon canon)
    {
        // The runtime registers more than a game assembly does (its messages, and what its own
        // post-processing makes of its NetworkBehaviours); none of that is read here.
        if (a.Name == Runtime) return "it is the Netcode runtime itself";
        if (!Referenced(a)) return null;
        if (!a.Types.TryGetValue(Serialization, out var helper) || !Declares(a, helper, SerializationMethod))
            return $"{Serialization}::{SerializationMethod} is missing";
        foreach (var t in a.MD.TypeDefinitions)
        {
            if (a.Kind(t) != GenKind.User) continue;
            var td = a.MD.GetTypeDefinition(t);
            int rpcs = 0, handlers = 0;
            bool registers = false, variables = false, named = false;
            string? headless = null;
            foreach (var m in td.GetMethods())
            {
                var d = a.MD.GetMethodDefinition(m);
                string name = a.MD.GetString(d.Name);
                if (name == InitializeRpcs) registers = true;
                else if (name == InitializeVariables) variables = true;
                else if (name == TypeName) named = true;
                else if (name.StartsWith(Handler, StringComparison.Ordinal)) handlers++;
                else if (IsRpc(a, d))
                {
                    rpcs++;
                    if (headless == null && canon.Head(m, ExecStage) == null) headless = a.MemberId(m);
                }
            }
            string type = a.TypeName(t);
            if (!registers)
            {
                bool derives = td.BaseType.Kind == HandleKind.TypeReference && a.TypeRefName((TypeReferenceHandle)td.BaseType, bare: true) == Behaviour;
                if (derives || rpcs + handlers > 0 || variables) return $"{type} is a NetworkBehaviour or declares RPCs and has no {InitializeRpcs}";
                continue;
            }
            if (!variables || !named) return $"{type} has {InitializeRpcs} and lacks {InitializeVariables} or {TypeName}";
            if (rpcs != handlers) return $"{type} has {rpcs} RPCs and {handlers} methods named {Handler}*";
            if (headless != null) return $"the body of the RPC {headless} does not start with the code that sends it";
        }
        return null;
    }

    /// The members a body calls, sorted, when the body is nothing but calls without arguments
    /// and its return; null for any other body. That is the form of the start-up registration:
    /// one call per serializer, each of which sets what belongs to one type, so their order
    /// carries nothing. It follows the order in which the post-processor met the types, which
    /// moves when a class moves in its file.
    public static List<string>? Calls(Asm a, Canon canon, MethodDefinitionHandle method)
    {
        var d = a.MD.GetMethodDefinition(method);
        if (d.RelativeVirtualAddress == 0) return null;
        var body = a.PE.GetMethodBody(d.RelativeVirtualAddress);
        if (body.ExceptionRegions.Length > 0 || !body.LocalSignature.IsNil) return null;
        byte[] il = body.GetILBytes() ?? Array.Empty<byte>();
        int calls = 0, returns = 0;
        var r = new IL.Reader(il);
        while (r.Read())
        {
            if (r.OpCode == IL.Call) calls++;
            else if (r.OpCode == 0x2A && r.Next == il.Length) returns++;
            else if (r.OpCode != 0x00) return null;
        }
        if (returns != 1) return null;
        var list = new List<string>(calls);
        foreach (var (_, text) in canon.Tokens(method))
        {
            // A call that takes or returns something leaves a value on the stack: not this form.
            // What follows the signature is the type arguments of a generic method.
            int at = text.IndexOf(Parameterless, StringComparison.Ordinal);
            if (at < 0 || (at + Parameterless.Length < text.Length && text[at + Parameterless.Length] != '<')) return null;
            list.Add(text);
        }
        if (list.Count != calls) return null;
        list.Sort(StringComparer.Ordinal);
        return list;
    }
}

internal sealed partial class Differ
{
    /// Either image uses Netcode and both are in the known form: the rules of this file apply.
    bool netcode;
    /// RPCs that already have their reason.
    readonly HashSet<string> rpcReported = new(StringComparer.Ordinal);

    /// Why an image of this assembly is not of the form the Netcode rules know; null when both are.
    /// Nothing of such an assembly may be patched: it is compared without these rules, only to
    /// see whether anything differs at all.
    public string? NetcodeUnknown { get; private set; }

    void CheckNetcodeForm()
    {
        if (!netcode) return;
        NetcodeUnknown = Netcode.Unknown(o, co) is { } loaded ? "in the loaded assembly " + loaded
            : Netcode.Unknown(n, cn) is { } current ? "in the new compilation " + current : null;
        netcode = NetcodeUnknown == null;
    }

    /// The methods the post-processor added to a type, by name and signature, with what they run.
    static SortedDictionary<string, (MethodDefinitionHandle Handle, string Hash)> GeneratedMethods(Asm a, Canon canon, TypeDefinitionHandle type, bool all)
    {
        var map = new SortedDictionary<string, (MethodDefinitionHandle, string)>(StringComparer.Ordinal);
        foreach (var m in a.MD.GetTypeDefinition(type).GetMethods())
        {
            var d = a.MD.GetMethodDefinition(m);
            if (!all && !Netcode.Generated(a.MD.GetString(d.Name))) continue;
            map[a.MethodKey(m)] = (m, (int)d.Attributes + ":" + (int)d.ImplAttributes + ":" + Convert.ToHexString(canon.Compute(m).Hash));
        }
        return map;
    }

    /// The RPC a handler calls, as its member id; the handler's own id when none is found.
    static string HandlerTarget(Asm a, MethodDefinitionHandle handler)
    {
        var d = a.MD.GetMethodDefinition(handler);
        if (d.RelativeVirtualAddress != 0)
        {
            byte[] il = a.PE.GetMethodBody(d.RelativeVirtualAddress).GetILBytes() ?? Array.Empty<byte>();
            var r = new IL.Reader(il);
            while (r.Read())
            {
                if (r.OpCode is not (IL.Call or IL.Callvirt)) continue;
                var h = MetadataTokens.EntityHandle(BinaryPrimitives.ReadInt32LittleEndian(il.AsSpan(r.OperandOffset)));
                if (h.Kind == HandleKind.MethodDefinition && Netcode.IsRpc(a, a.MD.GetMethodDefinition((MethodDefinitionHandle)h))) return a.MemberId((MethodDefinitionHandle)h);
            }
        }
        return a.MemberId(handler);
    }

    /// What the one method of a start-up type of the post-processor calls, sorted; null when the
    /// type is not of that form.
    static List<string>? StartUpCalls(Asm a, Canon canon, TypeDefinitionHandle type, string method)
    {
        var methods = a.MD.GetTypeDefinition(type).GetMethods();
        if (methods.Count != 1) return null;
        var only = methods.First();
        return a.MD.GetString(a.MD.GetMethodDefinition(only).Name) == method ? Netcode.Calls(a, canon, only) : null;
    }

    /// A type of the namespace the post-processor writes its start-up registration into.
    void DiffGenerated(string name, TypeDefinitionHandle ot, TypeDefinitionHandle nt)
    {
        if (ot.IsNil || nt.IsNil)
        {
            // The assembly started to use Netcode, or stopped. The post-processor writes these
            // two types into every assembly it rewrites, also when there is nothing to register:
            // a call of the Netcode API in ordinary code is enough. Then nothing ran at start-up
            // that a Player with the other image lacks, or has too much of.
            var (a, canon, type) = nt.IsNil ? (o, co, ot) : (n, cn, nt);
            if (name == Netcode.Serialization && StartUpCalls(a, canon, type, Netcode.SerializationMethod) is { Count: 0 })
            {
                Add("netcode", name, Actions.None, message: "The start-up registration of the Netcode post-processor registers nothing in this assembly.");
                return;
            }
            // The bridge only calls the registration, which is judged where it stands.
            if (name == Netcode.Bridge && StartUpCalls(a, canon, type, "__UnityPlay_RuntimeInitialize_Netcode") is { Count: 1 } calls
                && calls[0] == Netcode.Serialization + "::" + Netcode.SerializationMethod + Netcode.Parameterless)
            {
                Add("netcode", name, Actions.None, message: "The bridge through which the Player runs the start-up registration of the Netcode post-processor; it has no code of its own.");
                return;
            }
            Refuse("netcode", name, Codes.NetcodeGeneratedCodeChanged, ot.IsNil
                ? $"{name} is start-up registration that the Netcode post-processor writes; the assembly started to use Netcode, and the Player runs that registration only when it starts."
                : $"{name} is start-up registration that the Netcode post-processor writes; the assembly no longer uses Netcode, and the Player has run that registration.");
            return;
        }
        var olds = GeneratedMethods(o, co, ot, all: true);
        var news = GeneratedMethods(n, cn, nt, all: true);
        if (olds.Count == news.Count && olds.All(p => news.TryGetValue(p.Key, out var x) && x.Hash == p.Value.Hash)) return;
        if (name != Netcode.Serialization)
        {
            Refuse("netcode", name, Codes.NetcodeGeneratedCodeChanged, $"{name} is start-up code that the Netcode post-processor writes, and it changed; the Player runs it only when it starts.");
            return;
        }
        // The same registrations in another order are the same registrations.
        if (StartUpCalls(o, co, ot, Netcode.SerializationMethod) is { } loaded && StartUpCalls(n, cn, nt, Netcode.SerializationMethod) is { } current && loaded.SequenceEqual(current)
            && olds.Keys.SequenceEqual(news.Keys) && olds.Values.Select(x => Flags(x.Hash)).SequenceEqual(news.Values.Select(x => Flags(x.Hash))))
        {
            Add("netcode", name + "::" + Netcode.SerializationMethod + "()", Actions.None, message: "The start-up registration lists the same serializers in another order.");
            return;
        }
        // Each call registers the serializer or the comparison of one type: `...<T>`.
        static List<string> Registered(Asm a, Canon canon, SortedDictionary<string, (MethodDefinitionHandle Handle, string Hash)> methods)
        {
            var list = new List<string>();
            foreach (var (_, method) in methods)
                foreach (var (_, text) in canon.Tokens(method.Handle))
                    if (text.EndsWith('>') && text.IndexOf(Netcode.Parameterless + "<", StringComparison.Ordinal) is > 0 and var at) list.Add(text[(at + Netcode.Parameterless.Length + 1)..^1]);
            return list;
        }
        var before = Registered(o, co, olds);
        var after = Registered(n, cn, news);
        var parts = new List<string>();
        if (Without(after, before).Distinct().ToList() is { Count: > 0 } added) parts.Add("added for " + string.Join(", ", added));
        if (Without(before, after).Distinct().ToList() is { Count: > 0 } removed) parts.Add("removed for " + string.Join(", ", removed));
        Refuse("netcode", name + "::" + Netcode.SerializationMethod + "()", Codes.NetcodeSerializationChanged,
            $"The serializers Netcode registers for the value types of network variables changed{(parts.Count > 0 ? " (" + string.Join("; ", parts) + ")" : "")}; they are registered once, when the Player starts.");
    }

    /// The flags of a generated method, from what `GeneratedMethods` keeps of it.
    static string Flags(string hash) => hash[..hash.LastIndexOf(':')];

    /// What `__initializeRpcs` registers: one entry per `__registerRpc` call, sorted, and what
    /// follows the last one (the call of the base class). Null when the body is not of that form.
    static List<string>? Registrations(Canon canon, MethodDefinitionHandle method)
    {
        if (canon.Statements(method, "::__registerRpc(") is not { } statements) return null;
        var entries = statements.GetRange(0, statements.Count - 1);
        entries.Sort(StringComparer.Ordinal);
        entries.Add(statements[^1]);
        return entries;
    }

    /// The network variables `__initializeVariables` lists, in its order: `name:type`.
    List<string> Variables(Asm a, Canon canon, TypeDefinitionHandle type, MethodDefinitionHandle method)
    {
        string prefix = a.TypeName(type);
        var list = new List<string>();
        foreach (var (opcode, text) in canon.Tokens(method))
        {
            if (opcode != IL.Ldfld) continue;
            // A field of a generic class is named through its instantiation: `T<!0>::f:type = T::f:type`.
            string field = text.LastIndexOf(" = ", StringComparison.Ordinal) is >= 0 and var at ? text[(at + 3)..] : text;
            if (!field.StartsWith(prefix + "::", StringComparison.Ordinal)) continue;
            field = field[(prefix.Length + 2)..];
            if (!list.Contains(field)) list.Add(field);
        }
        return list;
    }

    /// What the post-processor generated for one NetworkBehaviour, compared as a whole.
    void DiffNetworked(TypeDefinitionHandle ot, TypeDefinitionHandle nt, string type)
    {
        bool was = Netcode.Networked(o, ot), now = Netcode.Networked(n, nt);
        if (!was && !now) return;
        if (was != now)
        {
            Refuse("netcode", type, Codes.NetcodeGeneratedCodeChanged, $"{type} {(now ? "became" : "is no longer")} a NetworkBehaviour; its RPCs and network variables are registered when its objects are initialized.");
            return;
        }
        var olds = GeneratedMethods(o, co, ot, all: false);
        var news = GeneratedMethods(n, cn, nt, all: false);
        int reported = Changes.Count;
        var keys = new SortedSet<string>(olds.Keys, StringComparer.Ordinal);
        keys.UnionWith(news.Keys);

        // The handlers: one per RPC, named after its id.
        foreach (string key in keys)
        {
            if (!key.StartsWith(Netcode.Handler, StringComparison.Ordinal)) continue;
            bool inOld = olds.TryGetValue(key, out var before), inNew = news.TryGetValue(key, out var after);
            if (inOld && inNew && before.Hash == after.Hash) continue;
            string rpc = inNew ? HandlerTarget(n, after.Handle) : HandlerTarget(o, before.Handle);
            if (!rpcReported.Add(rpc)) continue;
            Refuse("netcode_rpc", rpc, Codes.NetcodeRpcChanged, inOld && inNew
                ? $"The code that receives the RPC {rpc} changed (how a parameter is read); both ends of a connection must agree on it."
                : inNew ? $"The RPC {rpc} is new to the Player; the RPCs of {type} were registered when its first object was initialized."
                : $"The RPC {rpc} is registered in the Player and the new code no longer has it.");
        }
        bool rpcs = Changes.Skip(reported).Any(c => c.Code == Codes.NetcodeRpcChanged) || rpcReported.Any(id => id.StartsWith(type + "::", StringComparison.Ordinal));

        foreach (string key in keys)
        {
            if (key.StartsWith(Netcode.Handler, StringComparison.Ordinal)) continue;
            bool inOld = olds.TryGetValue(key, out var before), inNew = news.TryGetValue(key, out var after);
            if (inOld && inNew && before.Hash == after.Hash) continue;
            string name = key[..key.IndexOf('(')], id = inNew ? n.MemberId(after.Handle) : o.MemberId(before.Handle);
            if (name == Netcode.InitializeRpcs)
            {
                // The table follows the RPCs; alone, it differs in a name or in who may invoke one,
                // or it lists the same RPCs in another order: the order of their declarations,
                // which carries nothing, since the table is looked up by id.
                if (rpcs) continue;
                if (inOld && inNew && Flags(before.Hash) == Flags(after.Hash) && Registrations(co, before.Handle) is { } loaded && Registrations(cn, after.Handle) is { } current && loaded.SequenceEqual(current))
                    Add("netcode", id, Actions.None, message: $"{id} registers the same RPCs in another order.");
                else Refuse("netcode_rpc", id, Codes.NetcodeRpcChanged, $"The registration of the RPCs of {type} changed (who may invoke one of them); it ran when the first object of the class was initialized.");
            }
            else if (name == Netcode.InitializeVariables && inOld && inNew) DiffVariables(ot, nt, type, id, before.Handle, after.Handle);
            else Refuse("netcode", id, Codes.NetcodeGeneratedCodeChanged, name == Netcode.TypeName || name == Netcode.InitializeVariables
                ? $"{id} is code that the Netcode post-processor generates, and it {(inOld && inNew ? "changed" : inNew ? "is new" : "is gone")}; the Player took what it says when the objects of {type} were initialized."
                : $"{id} {(inOld && inNew ? "changed" : inNew ? "is new" : "is gone")}; in a NetworkBehaviour a method whose name starts with two underscores is taken for code of the Netcode post-processor that runs when the Player starts or an object is initialized, and the patch builder does not know this one.");
        }
        DiffReadPermissions(ot, nt, type);
    }

    const string ReadPermission = "Unity.Netcode.NetworkVariableReadPermission";

    /// The read permission each network variable of a type is constructed with, by the field
    /// that stores it: the code of that argument of every `newobj` of a constructor that takes a
    /// NetworkVariableReadPermission (NetworkVariable<T>, NetworkList<T>), in the methods of the
    /// type. A construction whose argument cannot be cut out of the code around it is kept whole.
    static Dictionary<string, List<string>> ReadPermissions(Asm a, Canon canon, TypeDefinitionHandle type)
    {
        var found = new Dictionary<string, List<string>>(StringComparer.Ordinal);
        foreach (var m in a.MD.GetTypeDefinition(type).GetMethods())
        {
            var d = a.MD.GetMethodDefinition(m);
            if (d.RelativeVirtualAddress == 0 || Netcode.Generated(a.MD.GetString(d.Name))) continue;
            var code = canon.Instructions(m);
            for (int i = 0; i < code.Count; i++)
            {
                if (code[i].OpCode != IL.Newobj) continue;
                var parameters = ConstructorParameters(code[i].Text);
                // A type of another assembly is written with its scope: `[Unity.Netcode.Runtime]Unity...`.
                int position = parameters.FindIndex(p => p == ReadPermission || p.EndsWith("]" + ReadPermission, StringComparison.Ordinal));
                if (position < 0) continue;
                string field = i + 1 < code.Count && code[i + 1].OpCode == IL.Stfld ? FieldName(code[i + 1].Text) : "?" + a.MethodKey(m);
                // Without a part of its own, the argument is compared with all that comes before it.
                string text = Argument(code, i, parameters.Count, position) ?? "whole:" + string.Join("\n", code.Take(i + 1).Select(x => x.Text));
                if (!found.TryGetValue(field, out var list)) found[field] = list = new List<string>();
                list.Add(text);
            }
        }
        foreach (var list in found.Values) list.Sort(StringComparer.Ordinal);
        return found;
    }

    /// The parameter types of the constructor a `newobj` text names: `Type::.ctor(A,B):System.Void`.
    static List<string> ConstructorParameters(string text)
    {
        int open = text.IndexOf("::.ctor(", StringComparison.Ordinal);
        if (open < 0) return new List<string>();
        open += "::.ctor(".Length;
        // The signature ends at the parenthesis that closes it; type arguments hold commas.
        var list = new List<string>();
        int depth = 0, start = open;
        for (int i = open; i < text.Length; i++)
        {
            char c = text[i];
            if (c is '<' or '[' or '(') depth++;
            else if (c is '>' or ']') depth--;
            else if (c == ')' && depth-- == 0)
            {
                if (i > start) list.Add(text[start..i].Trim());
                break;
            }
            else if (c == ',' && depth == 0) { list.Add(text[start..i].Trim()); start = i + 1; }
        }
        return list;
    }

    /// The name of the field a `stfld` text stores into: `Type::name:FieldType`.
    static string FieldName(string text)
    {
        string field = text.LastIndexOf(" = ", StringComparison.Ordinal) is >= 0 and var at ? text[(at + 3)..] : text[(text.IndexOf(' ') + 1)..];
        int colons = field.IndexOf("::", StringComparison.Ordinal);
        if (colons < 0) return field;
        string rest = field[(colons + 2)..];
        return rest.IndexOf(':') is >= 0 and var end ? rest[..end] : rest;
    }

    /// The code of argument `position` of the call at `call` that takes `count` arguments: walked
    /// back from the call while every instruction says what it takes and leaves and none branches.
    /// Null when the argument cannot be cut out that way.
    static string? Argument(List<Canon.Instruction> code, int call, int count, int position)
    {
        // Where the code of the arguments from `first` to the last begins.
        int? Start(int first)
        {
            int need = count - first, i = call;
            while (need > 0)
            {
                if (--i < 0) return null;
                var x = code[i];
                if (x.Branches || x.Pops < 0 || x.Pushes < 0) return null;
                need += x.Pops - x.Pushes;
                if (need < 0) return null;
            }
            return i;
        }
        if (Start(position) is not { } begin) return null;
        int end = position + 1 < count ? Start(position + 1) ?? -1 : call;
        if (end < begin) return null;
        return string.Join("\n", code.Skip(begin).Take(end - begin).Select(x => x.Text));
    }

    /// A network variable whose read permission changed: NGO writes a variable into the spawn
    /// and synchronization payload of an object only for a client its own copy says may read it,
    /// and a reader reads it only when its copy says so. The objects that exist keep the old
    /// permission, every Player started on the new code builds them with the new one, and the two
    /// read each other's payload out of step (EnsureNetworkVariableLengthSafety is off by
    /// default). The initial value and the write permission are not on the wire that way and
    /// are patched as constructor code.
    void DiffReadPermissions(TypeDefinitionHandle ot, TypeDefinitionHandle nt, string type)
    {
        var olds = ReadPermissions(o, co, ot);
        var news = ReadPermissions(n, cn, nt);
        var fields = new SortedSet<string>(olds.Keys, StringComparer.Ordinal);
        fields.UnionWith(news.Keys);
        foreach (string field in fields)
        {
            var before = olds.GetValueOrDefault(field) ?? new List<string>();
            var after = news.GetValueOrDefault(field) ?? new List<string>();
            if (before.SequenceEqual(after)) continue;
            string member = field.StartsWith('?') ? type : type + "::" + field;
            // An added, removed or renamed variable has its reason already.
            if (Changes.Any(c => c.Code == Codes.NetcodeVariableChanged && c.Member == member)) continue;
            Refuse("netcode_variable", member, Codes.NetcodeVariableChanged,
                $"The read permission with which the network variable {member} is constructed changed; each end of a connection decides from its own copy whether the variable is in an object's spawn and synchronization data, the objects that exist keep the old permission and a Player started on the new code uses the new one, so the two would read that data out of step.");
        }
    }

    void DiffVariables(TypeDefinitionHandle ot, TypeDefinitionHandle nt, string type, string id, MethodDefinitionHandle before, MethodDefinitionHandle after)
    {
        var olds = Variables(o, co, ot, before);
        var news = Variables(n, cn, nt, after);
        static string Name(string field) => field[..field.IndexOf(':')];
        const string why = "every object lists its network variables once, when it is initialized, and both ends of a connection replicate them by their position in that list";
        int reported = Changes.Count;
        foreach (string field in news)
        {
            if (olds.Contains(field)) continue;
            string name = Name(field);
            Refuse("netcode_variable", type + "::" + name, Codes.NetcodeVariableChanged, olds.Any(x => Name(x) == name)
                ? $"The network variable {type}::{name} changed its type; {why}."
                : $"The network variable {type}::{name} was added; {why}.");
        }
        foreach (string field in olds)
            if (!news.Any(x => Name(x) == Name(field)))
                Refuse("netcode_variable", type + "::" + Name(field), Codes.NetcodeVariableChanged, $"The network variable {type}::{Name(field)} was removed or renamed; {why}.");
        if (Changes.Count == reported)
            Refuse("netcode_variable", id, Codes.NetcodeVariableChanged, $"The network variables of {type} changed their order or their initialization; {why}.");
    }

    /// An RPC is patched like any method while the code that sends it is as the Player has it.
    /// False when the method is refused here and its body is not to be compared.
    bool NetcodeRpc(MethodDefinitionHandle oh, MethodDefinitionHandle nh, MethodDefinition od, MethodDefinition nd, string id)
    {
        bool was = Netcode.IsRpc(o, od), now = Netcode.IsRpc(n, nd);
        if (!was && !now) return true;
        if (was != now)
        {
            rpcReported.Add(id);
            Refuse("netcode_rpc", id, Codes.NetcodeRpcChanged, now
                ? $"{id} became an RPC; the RPCs of its class were registered when its first object was initialized."
                : $"{id} is no longer an RPC; the Player has it registered, and other peers may still send it.");
            return false;
        }
        string? before = co.Head(oh, Netcode.ExecStage), after = cn.Head(nh, Netcode.ExecStage);
        if (before != null && before == after) return true;
        rpcReported.Add(id);
        Refuse("netcode_rpc", id, Codes.NetcodeRpcChanged, $"The code that sends the RPC {id} changed (its target, its delivery, who may invoke it or how a parameter is written); both ends of a connection must agree on it.");
        return false;
    }
}
