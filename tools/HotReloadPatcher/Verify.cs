// Reads an emitted patch back and checks what the Player would otherwise find out the hard way.
// Any finding fails the patch closed: nothing is published.
using System.Buffers.Binary;
using System.Reflection;
using System.Reflection.Emit;
using System.Reflection.Metadata;
using System.Reflection.Metadata.Ecma335;

internal static class Verifier
{
    /// The problems of a patch; empty when it may be published.
    public static List<string> Check(Workspace workspace, Entry entry, Patch patch)
    {
        var problems = new List<string>();
        try
        {
            using var p = new Asm(patch.AssemblyName + ".dll", new MemoryStream(patch.Image));
            if (p.Name != patch.AssemblyName) problems.Add($"The image is named {p.Name}, not {patch.AssemblyName}.");
            Security(p, problems);
            Names(workspace, p, problems);
            References(workspace, p, problems);
            Bodies(p, problems);
            Detours(entry.Loaded, p, patch, problems);
        }
        catch (Exception e) when (e is BadImageFormatException or InvalidDataException or InvalidOperationException or IndexOutOfRangeException or ArgumentException)
        {
            problems.Add("The image cannot be read back: " + e.Message);
        }
        return problems;
    }

    /// Without the permission set every access to a private member of the game throws in the Player.
    static void Security(Asm p, List<string> problems)
    {
        bool permission = false, unverifiable = false;
        byte[] expected = Emitter.SkipVerificationPermissionSet();
        foreach (var h in p.MD.DeclarativeSecurityAttributes)
        {
            var d = p.MD.GetDeclarativeSecurityAttribute(h);
            permission |= d.Parent.Kind == HandleKind.AssemblyDefinition && d.Action == DeclarativeSecurityAction.RequestMinimum &&
                p.MD.GetBlobBytes(d.PermissionSet).AsSpan().SequenceEqual(expected);
        }
        foreach (var h in p.MD.GetModuleDefinition().GetCustomAttributes())
            unverifiable |= p.AttributeTypeName(p.MD.GetCustomAttribute(h)) == "System.Security.UnverifiableCodeAttribute";
        if (!permission) problems.Add("The SkipVerification permission set is missing.");
        if (!unverifiable) problems.Add("The UnverifiableCode module attribute is missing.");
    }

    /// The Player's loader refuses a second type of a full name that a game assembly already has.
    static void Names(Workspace workspace, Asm p, List<string> problems)
    {
        foreach (var name in p.Types.Keys)
        {
            if (name == "<Module>") continue;
            foreach (var e in workspace.Entries)
                if (e.Loaded.Types.ContainsKey(name)) problems.Add($"Type {name} of the patch also exists in the loaded {e.Name}.");
        }
    }

    static EntityHandle GenericDefinition(Asm p, TypeSpecificationHandle h)
    {
        var blob = p.MD.GetBlobReader(p.MD.GetTypeSpecification(h).Signature);
        if (blob.ReadByte() != 0x15) return default;
        blob.ReadByte();
        return blob.ReadTypeHandle();
    }

    /// Every type and member of a game assembly that the patch names must exist in what the Player loaded.
    static void References(Workspace workspace, Asm p, List<string> problems)
    {
        foreach (var h in p.MD.TypeReferences)
        {
            if (workspace.Find(p.TypeRefAssembly(h)) is not { } owner) continue;
            string name = p.TypeRefName(h, bare: true);
            if (!owner.Loaded.Types.ContainsKey(name)) problems.Add($"Type {name} is not part of the loaded {owner.Name}.");
        }
        foreach (var h in p.MD.MemberReferences)
        {
            var mr = p.MD.GetMemberReference(h);
            EntityHandle parent = mr.Parent;
            if (parent.Kind == HandleKind.TypeSpecification) parent = GenericDefinition(p, (TypeSpecificationHandle)parent);
            if (parent.Kind != HandleKind.TypeReference) continue;
            var reference = (TypeReferenceHandle)parent;
            if (workspace.Find(p.TypeRefAssembly(reference)) is not { } owner) continue;
            string type = p.TypeRefName(reference, bare: true), member = p.MD.GetString(mr.Name);
            bool found;
            if (mr.GetKind() == MemberReferenceKind.Method) found = owner.Loaded.HasMember(type, member + Asm.SigText(mr.DecodeMethodSignature(p.Bare, null)));
            else
            {
                string field = member + ":" + mr.DecodeFieldSignature(p.Bare, null);
                found = owner.Loaded.HasMember(type, field) || owner.Loaded.HasMember(type, field + " static");
            }
            if (!found) problems.Add($"{type}::{member} is not part of the loaded {owner.Name}.");
        }
    }

    static bool Valid(Asm p, int token, OperandType operand)
    {
        int table = token >>> 24, row = token & 0xFFFFFF;
        if (operand == OperandType.InlineString) return table == 0x70 && row < p.MD.GetHeapSize(HeapIndex.UserString);
        bool type = table is 0x01 or 0x02 or 0x1B, method = table is 0x06 or 0x0A or 0x2B, field = table is 0x04 or 0x0A;
        bool kind = operand switch
        {
            OperandType.InlineType => type,
            OperandType.InlineMethod => method,
            OperandType.InlineField => field,
            OperandType.InlineSig => table == 0x11,
            _ => type || method || field,
        };
        return kind && row >= 1 && row <= p.MD.GetTableRowCount((TableIndex)table);
    }

    /// Every signature decodes, and every body is a sequence of whole instructions whose tokens and
    /// branch targets exist.
    static void Bodies(Asm p, List<string> problems)
    {
        for (int row = 1; row <= p.MD.GetTableRowCount(TableIndex.TypeSpec); row++)
            p.MD.GetTypeSpecification(MetadataTokens.TypeSpecificationHandle(row)).DecodeSignature(p.Plain, null);
        foreach (var h in p.MD.FieldDefinitions) p.MD.GetFieldDefinition(h).DecodeSignature(p.Plain, null);
        foreach (var h in p.MD.MemberReferences)
        {
            var mr = p.MD.GetMemberReference(h);
            if (mr.GetKind() == MemberReferenceKind.Method) mr.DecodeMethodSignature(p.Plain, null); else mr.DecodeFieldSignature(p.Plain, null);
        }
        for (int row = 1; row <= p.MD.GetTableRowCount(TableIndex.MethodSpec); row++)
            p.MD.GetMethodSpecification(MetadataTokens.MethodSpecificationHandle(row)).DecodeSignature(p.Plain, null);

        foreach (var h in p.MD.MethodDefinitions)
        {
            var d = p.MD.GetMethodDefinition(h);
            string id = p.MemberId(h);
            d.DecodeSignature(p.Plain, null);
            if (d.RelativeVirtualAddress == 0)
            {
                if ((d.Attributes & MethodAttributes.Abstract) == 0) problems.Add($"{id} has no body.");
                continue;
            }
            try
            {
                var body = p.PE.GetMethodBody(d.RelativeVirtualAddress);
                if (!body.LocalSignature.IsNil) p.MD.GetStandaloneSignature(body.LocalSignature).DecodeLocalSignature(p.Plain, null);
                byte[] il = body.GetILBytes() ?? Array.Empty<byte>();
                var starts = new HashSet<int>();
                var targets = new List<int>();
                var r = new IL.Reader(il);
                while (r.Read())
                {
                    starts.Add(r.Offset);
                    if (IL.IsToken(r.Operand))
                    {
                        int token = BinaryPrimitives.ReadInt32LittleEndian(il.AsSpan(r.OperandOffset));
                        if (!Valid(p, token, r.Operand)) problems.Add($"{id} has the invalid token 0x{token:X8} at IL_{r.Offset:X4}.");
                    }
                    else if (r.Operand == OperandType.ShortInlineBrTarget) targets.Add(r.Next + (sbyte)il[r.OperandOffset]);
                    else if (r.Operand == OperandType.InlineBrTarget) targets.Add(r.Next + BinaryPrimitives.ReadInt32LittleEndian(il.AsSpan(r.OperandOffset)));
                    else if (r.Operand == OperandType.InlineSwitch)
                    {
                        int count = BinaryPrimitives.ReadInt32LittleEndian(il.AsSpan(r.OperandOffset));
                        for (int i = 0; i < count; i++) targets.Add(r.Next + BinaryPrimitives.ReadInt32LittleEndian(il.AsSpan(r.OperandOffset + 4 + 4 * i)));
                    }
                }
                foreach (int target in targets)
                    if (!starts.Contains(target)) { problems.Add($"{id} branches to IL_{target:X4}, which is not an instruction."); break; }
                foreach (var region in body.ExceptionRegions)
                {
                    if (!starts.Contains(region.TryOffset) || !starts.Contains(region.HandlerOffset) || region.HandlerOffset + region.HandlerLength > il.Length)
                        problems.Add($"{id} has an exception region outside its instructions.");
                    if (!region.CatchType.IsNil && !Valid(p, MetadataTokens.GetToken(region.CatchType), OperandType.InlineType))
                        problems.Add($"{id} has an invalid catch type.");
                }
            }
            catch (Exception e) when (e is BadImageFormatException or InvalidDataException)
            {
                problems.Add($"{id} does not decode: {e.Message}");
            }
        }
    }

    /// A jump from the original into the replacement is only sound when both take their arguments
    /// the same way: same parameters and return type, and a receiver that is a struct pointer on
    /// both sides or an object reference on both sides (as `this` or as the first parameter).
    static void Detours(Asm o, Asm p, Patch patch, List<string> problems)
    {
        // Parameter and return types must be the very same types on both sides: the applier compares
        // them, and a type of the patch (the copy of a closure) is never the loaded one. Only the
        // receiver of a lambda differs, a closure object of the same layout.
        var patchText = new SymbolProvider(p, bare: true) { DefHook = h => "$patch:" + p.TypeName(h) };
        var seen = new HashSet<int>();
        foreach (var detour in patch.Detours)
        {
            string member = detour.Member;
            int originalRow = detour.OriginalToken & 0xFFFFFF, replacementRow = detour.ReplacementToken & 0xFFFFFF;
            if (detour.OriginalToken >>> 24 != 0x06 || originalRow < 1 || originalRow > o.MD.MethodDefinitions.Count ||
                detour.ReplacementToken >>> 24 != 0x06 || replacementRow < 1 || replacementRow > p.MD.MethodDefinitions.Count)
            {
                problems.Add($"The redirect of {member} names a method that does not exist.");
                continue;
            }
            if (!seen.Add(detour.OriginalToken)) problems.Add($"{member} is redirected twice.");
            var original = MetadataTokens.MethodDefinitionHandle(originalRow);
            var replacement = MetadataTokens.MethodDefinitionHandle(replacementRow);
            var od = o.MD.GetMethodDefinition(original);
            var pd = p.MD.GetMethodDefinition(replacement);
            if (od.RelativeVirtualAddress == 0 || pd.RelativeVirtualAddress == 0) { problems.Add($"The redirect of {member} has a side without a body."); continue; }
            if (od.GetGenericParameters().Count > 0 || o.IsGenericType(od.GetDeclaringType()) || pd.GetGenericParameters().Count > 0 || p.IsGenericType(pd.GetDeclaringType()))
            {
                problems.Add($"The redirect of {member} involves generic code.");
                continue;
            }
            var so = od.DecodeSignature(o.Bare, null);
            var sp = pd.DecodeSignature(patchText, null);
            var expected = new List<string>(so.ParameterTypes);
            bool receiver = so.Header.IsInstance, structReceiver = receiver && o.IsValueType(od.GetDeclaringType());
            bool shape;
            if (!receiver) shape = !sp.Header.IsInstance;
            else if (structReceiver) shape = sp.Header.IsInstance && p.IsValueType(pd.GetDeclaringType());
            else if (sp.Header.IsInstance) shape = !p.IsValueType(pd.GetDeclaringType());
            else
            {
                shape = true;
                expected.Insert(0, o.TypeName(od.GetDeclaringType()));
            }
            if (!shape || so.ReturnType != sp.ReturnType || !expected.SequenceEqual(sp.ParameterTypes) ||
                so.Header.CallingConvention != sp.Header.CallingConvention)
                problems.Add($"The replacement of {member} has another shape: ({string.Join(",", sp.ParameterTypes)}):{sp.ReturnType} for ({string.Join(",", expected)}):{so.ReturnType}.");
        }
    }
}
