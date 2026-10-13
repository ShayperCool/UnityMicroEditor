using System;
using System.Collections.Generic;
using System.Reflection;
using System.Reflection.Emit;

namespace UnityMicroEditor.HotReload
{
    // Generations are cumulative: a method that was edited once is listed by every later descriptor
    // and compiled into every later patch assembly. Whether its body changed with the generation
    // being applied is read off the IL of the two replacements: the same instructions naming the
    // same members, where a member of one patch assembly matches its namesake in the other.
    // What only the patches define belongs to the body that names it: a lambda, a local function,
    // an added method or a state machine is compared by its own code as well, because nothing
    // else tells its edit from the generation before. Only callbacks depend on the answer, and
    // whatever cannot be decided counts as changed.
    internal static class Bodies
    {
        /// <summary>The two patch assemblies of the comparisons of one applied generation.</summary>
        private sealed class Pair
        {
            internal Assembly Left, Right;
            // The patch builder names the types of a generation after it. The one name segment
            // in which corresponding types differ must be the same for every type compared.
            internal string LeftMark, RightMark;
            // Methods and state machines of the left patch that are compared or being compared.
            // A second visit adds nothing, and it ends the recursion of code that names itself.
            internal readonly HashSet<MethodBase> Methods = new HashSet<MethodBase>();
            internal readonly HashSet<Type> Machines = new HashSet<Type>();
            // Those of earlier comparisons that came out the same. Many redirected methods can
            // name one method of the patch, and it is compared once for all of them.
            internal readonly HashSet<MethodBase> SameMethods = new HashSet<MethodBase>();
            internal readonly HashSet<Type> SameMachines = new HashSet<Type>();
        }

        /// <summary>What the comparisons of one applied generation have established so far.</summary>
        internal sealed class Pass
        {
            internal readonly List<object> Pairs = new List<object>();
        }

        private const BindingFlags Declared = BindingFlags.Public | BindingFlags.NonPublic | BindingFlags.Static | BindingFlags.Instance | BindingFlags.DeclaredOnly;

        private static Dictionary<short, OperandType> operands;

        internal static bool Same(MethodBase left, MethodBase right, Pass pass)
        {
            try
            {
                Assembly one = left.Module.Assembly, other = right.Module.Assembly;
                Pair pair = null;
                foreach (object known in pass.Pairs)
                    if (((Pair)known).Left == one && ((Pair)known).Right == other) pair = (Pair)known;
                if (pair == null) pass.Pairs.Add(pair = new Pair { Left = one, Right = other });
                pair.Methods.Clear();
                pair.Machines.Clear();
                if (!Compare(left, right, pair)) return false;
                // Whatever this comparison visited is the same in both patches: one difference
                // anywhere would have ended it.
                pair.SameMethods.UnionWith(pair.Methods);
                pair.SameMachines.UnionWith(pair.Machines);
                return true;
            }
            catch (Exception) { return false; }
        }

        private static bool Compare(MethodBase left, MethodBase right, Pair pair)
        {
            MethodBody first = left.GetMethodBody(), second = right.GetMethodBody();
            if (first == null || second == null || first.InitLocals != second.InitLocals || first.LocalVariables.Count != second.LocalVariables.Count
                || first.ExceptionHandlingClauses.Count != second.ExceptionHandlingClauses.Count) return false;
            for (int index = 0; index < first.LocalVariables.Count; index++)
                if (first.LocalVariables[index].IsPinned != second.LocalVariables[index].IsPinned
                    || !SameType(first.LocalVariables[index].LocalType, second.LocalVariables[index].LocalType, pair)) return false;
            for (int index = 0; index < first.ExceptionHandlingClauses.Count; index++)
            {
                ExceptionHandlingClause one = first.ExceptionHandlingClauses[index], other = second.ExceptionHandlingClauses[index];
                if (one.Flags != other.Flags || one.TryOffset != other.TryOffset || one.TryLength != other.TryLength
                    || one.HandlerOffset != other.HandlerOffset || one.HandlerLength != other.HandlerLength) return false;
                if (one.Flags == ExceptionHandlingClauseOptions.Clause && !SameType(one.CatchType, other.CatchType, pair)) return false;
                if (one.Flags == ExceptionHandlingClauseOptions.Filter && one.FilterOffset != other.FilterOffset) return false;
            }
            byte[] code = first.GetILAsByteArray(), otherCode = second.GetILAsByteArray();
            if (code.Length != otherCode.Length) return false;
            if (operands == null)
            {
                var table = new Dictionary<short, OperandType>();
                foreach (FieldInfo field in typeof(OpCodes).GetFields(BindingFlags.Public | BindingFlags.Static))
                    if (field.GetValue(null) is OpCode opcode) table[opcode.Value] = opcode.OperandType;
                operands = table;
            }
            int at = 0;
            while (at < code.Length)
            {
                int start = at;
                short value = code[at++];
                if (value == 0xFE) value = unchecked((short)(0xFE00 | code[at++]));
                int size;
                switch (operands[value])
                {
                    case OperandType.InlineNone: size = 0; break;
                    case OperandType.ShortInlineBrTarget: case OperandType.ShortInlineI: case OperandType.ShortInlineVar: size = 1; break;
                    case OperandType.InlineVar: size = 2; break;
                    case OperandType.InlineI8: case OperandType.InlineR: size = 8; break;
                    case OperandType.InlineSwitch: size = 4 + 4 * BitConverter.ToInt32(code, at); break;
                    case OperandType.InlineString:
                        if (left.Module.ResolveString(BitConverter.ToInt32(code, at)) != right.Module.ResolveString(BitConverter.ToInt32(otherCode, at))) return false;
                        size = -4; break;
                    case OperandType.InlineField: case OperandType.InlineMethod: case OperandType.InlineType: case OperandType.InlineTok:
                        if (!SameMember(left.Module.ResolveMember(BitConverter.ToInt32(code, at)), right.Module.ResolveMember(BitConverter.ToInt32(otherCode, at)), pair)) return false;
                        size = -4; break;
                    // A signature blob names types by tokens of its own module.
                    case OperandType.InlineSig: return false;
                    default: size = 4; break;
                }
                // Opcode and plain operand bytes must be equal; a token (negative size) was compared by what it names.
                int end = size < 0 ? at : at + size;
                for (int index = start; index < end; index++) if (code[index] != otherCode[index]) return false;
                at += Math.Abs(size);
            }
            return true;
        }

        private static bool SameMember(MemberInfo left, MemberInfo right, Pair pair)
        {
            if (left is Type type) return right is Type other && SameType(type, other, pair);
            if (left.MemberType != right.MemberType || left.Name != right.Name || !SameType(left.DeclaringType, right.DeclaringType, pair)) return false;
            if (left is FieldInfo field) return SameType(field.FieldType, ((FieldInfo)right).FieldType, pair);
            MethodBase method = (MethodBase)left, match = (MethodBase)right;
            if (method.IsStatic != match.IsStatic || method.IsGenericMethod != match.IsGenericMethod) return false;
            if (method.IsGenericMethod && !SameTypes(method.GetGenericArguments(), match.GetGenericArguments(), pair)) return false;
            ParameterInfo[] parameters = method.GetParameters(), others = match.GetParameters();
            if (parameters.Length != others.Length) return false;
            for (int index = 0; index < parameters.Length; index++)
                if (!SameType(parameters[index].ParameterType, others[index].ParameterType, pair)) return false;
            if (method is MethodInfo info && !SameType(info.ReturnType, ((MethodInfo)match).ReturnType, pair)) return false;
            // A method of the game is redirected on its own and listed on its own. One that only
            // the patches define runs as part of the bodies that name it.
            if (method.Module.Assembly != pair.Left || match.Module.Assembly != pair.Right) return true;
            if (pair.SameMethods.Contains(method) || !pair.Methods.Add(method)) return true;
            return Compare(method, match, pair);
        }

        // An iterator or async method only makes its state machine: the code that was written in it
        // is MoveNext, which no instruction of the method names.
        private static bool SameMachine(Type left, Type right, Pair pair)
        {
            if (!typeof(System.Collections.IEnumerator).IsAssignableFrom(left) && !typeof(System.Runtime.CompilerServices.IAsyncStateMachine).IsAssignableFrom(left)) return true;
            if (pair.SameMachines.Contains(left) || !pair.Machines.Add(left)) return true;
            MethodInfo[] methods = left.GetMethods(Declared), others = right.GetMethods(Declared);
            if (methods.Length != others.Length) return false;
            for (int index = 0; index < methods.Length; index++)
                if (!SameMember(methods[index], others[index], pair)) return false;
            return true;
        }

        private static bool SameType(Type left, Type right, Pair pair)
        {
            if (left == right) return true;
            if (left == null || right == null) return false;
            if (left.HasElementType || right.HasElementType)
                return left.HasElementType && right.HasElementType && left.IsArray == right.IsArray && left.IsByRef == right.IsByRef && left.IsPointer == right.IsPointer
                    && (!left.IsArray || left.GetArrayRank() == right.GetArrayRank()) && SameType(left.GetElementType(), right.GetElementType(), pair);
            if (left.IsGenericType && !left.IsGenericTypeDefinition)
                return right.IsGenericType && !right.IsGenericTypeDefinition && SameType(left.GetGenericTypeDefinition(), right.GetGenericTypeDefinition(), pair)
                    && SameTypes(left.GetGenericArguments(), right.GetGenericArguments(), pair);
            // Two different types are the same only as the two copies of one patch type.
            if (left.Assembly != pair.Left || right.Assembly != pair.Right) return false;
            string[] names = left.FullName.Split('.'), others = right.FullName.Split('.');
            if (names.Length != others.Length) return false;
            int differing = -1;
            for (int index = 0; index < names.Length; index++)
            {
                if (names[index] == others[index]) continue;
                if (differing >= 0) return false;
                differing = index;
            }
            if (differing >= 0)
            {
                if (pair.LeftMark == null) { pair.LeftMark = names[differing]; pair.RightMark = others[differing]; }
                if (pair.LeftMark != names[differing] || pair.RightMark != others[differing]) return false;
            }
            return SameMachine(left, right, pair);
        }

        private static bool SameTypes(Type[] left, Type[] right, Pair pair)
        {
            if (left.Length != right.Length) return false;
            for (int index = 0; index < left.Length; index++) if (!SameType(left[index], right[index], pair)) return false;
            return true;
        }
    }
}
