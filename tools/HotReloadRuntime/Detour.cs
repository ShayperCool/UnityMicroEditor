// The redirect a code patch is made of. A method of the project's own assemblies is replaced by
// turning the start of its JIT-compiled body into a jump to the body compiled from the patch
// assembly. Both addresses come from the public RuntimeMethodHandle.GetFunctionPointer, and a
// patch reaches the project's non-public members through the declarative SkipVerification
// permission its assembly carries. No data structure of Mono or of the engine is read or written,
// and no foreign code is redirected: the repository rule against reaching into foreign private
// members is kept for everything that is not the project's own code.
//
// Verified for Unity 6000.6.3f1, Mono, Linux x86-64: JIT code is mapped writable and executable
// and bodies start 16-byte aligned. Other threads are not stopped, and one of them may be about
// to execute any instruction of a body. So a first redirect only ever replaces bytes of one
// instruction and leaves the first byte of the next one alone: the first byte of the body, which
// then leads to an island (Island.cs); or the whole second instruction, where the first one is
// sub rsp, imm8 and the second a mov of five bytes; or a whole first instruction of seven bytes.
using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Reflection;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using System.Threading;

namespace UnityMicroEditor.HotReload
{
    internal static unsafe class Detour
    {
        // E9 rel32 occupies bytes 0-4 of the first word; bytes 5-7 stay the method's own.
        internal const ulong JumpBytes = 0xFFFFFFFFFFUL;
        // The whole first instruction of a sub rsp, imm8 body: bytes 0-3.
        private const ulong FirstInstruction = 0xFFFFFFFFUL;
        private const ulong GateBytes = 0xFFFFUL;
        // Bytes 2 and 3, the middle of the displacement a gated island write puts behind the gate.
        private const ulong MiddleBytes = 0xFFFF0000UL;
        private const ulong Gate = 0xFEEBUL;

        // Writable and executable mappings as of the last read of the process map.
        private static readonly List<long> mappings = new List<long>();

        /// <summary>Why this process cannot be patched at all, or null.</summary>
        internal static string Unsupported()
        {
            if (Type.GetType("Mono.Runtime") == null) return "Code patches need the Mono JIT.";
            if (IntPtr.Size != 8 || !BitConverter.IsLittleEndian || RuntimeInformation.ProcessArchitecture != Architecture.X64)
                return "Code patches are implemented for x86-64 only.";
            return null;
        }

        /// <summary>
        /// Start of the method's compiled body. Compiles the method and runs the static constructor of
        /// its type, so access and IL errors of a replacement surface here, before anything is written.
        /// </summary>
        internal static IntPtr Entry(MethodBase method)
        {
            // Asking the runtime for the entry of an open generic method aborts the process.
            if (method.ContainsGenericParameters || (method.DeclaringType != null && method.DeclaringType.ContainsGenericParameters))
                throw new NotSupportedException("A generic definition has no compiled entry.");
            RuntimeMethodHandle handle = method.MethodHandle;
            RuntimeHelpers.PrepareMethod(handle);
            return handle.GetFunctionPointer();
        }

        /// <summary>
        /// Why the start of the body at one entry cannot be rewritten to reach the other, or null.
        /// Ask only when everything the request compiles is compiled: the JIT maps a new chunk of
        /// code whenever one fills, and the process map is read again at most once per request,
        /// for the first entry the last read does not cover.
        /// </summary>
        internal static string Refusal(IntPtr from, IntPtr to, ref bool mapRead)
        {
            long source = from.ToInt64(), target = to.ToInt64();
            if (source == 0 || target == 0) return "The runtime returned no compiled entry.";
            if (source == target) return "The replacement is the method itself.";
            if ((source & 7) != 0) return "The compiled entry is not 8-byte aligned.";
            if (!Writable(source, ref mapRead)) return "The compiled entry is not in a writable and executable mapping of /proc/self/maps.";
            return null;
        }

        /// <summary>Whether a five-byte jump at the entry reaches the target. An island needs no reach.</summary>
        internal static bool Reaches(IntPtr from, IntPtr to)
        {
            long distance = to.ToInt64() - (from.ToInt64() + 5);
            return distance >= int.MinValue && distance <= int.MaxValue;
        }

        internal static ulong Read(IntPtr from)
        {
            return Volatile.Read(ref *(ulong*)from);
        }

        /// <summary>The ninth byte of the body. No body is shorter: sub rsp, add rsp and ret are nine.</summary>
        internal static byte Rest(IntPtr from)
        {
            return Volatile.Read(ref *((byte*)from + 8));
        }

        /// <summary>
        /// First redirect through an island: byte 0 becomes E9 and nothing else changes. One byte is
        /// stored atomically and no thread can be inside it; a thread that is past it keeps running
        /// the old body, whose bytes are all still there.
        /// </summary>
        internal static void Enter(IntPtr from)
        {
            Volatile.Write(ref *(byte*)from, (byte)0xE9);
        }

        /// <summary>Puts the first byte back; the island stays where it is.</summary>
        internal static void Leave(IntPtr from, ulong original)
        {
            Volatile.Write(ref *(byte*)from, (byte)original);
        }

        /// <summary>
        /// Whether the body starts with sub rsp, imm8 followed by mov r32, imm32 (B8 to BF): what a
        /// method begins with that first loads a constant. Its second instruction, bytes 4 to 8, is
        /// as long as a jump.
        /// </summary>
        internal static bool LoadsConstant(ulong original)
        {
            return (original & 0xFFFFFF) == 0xEC8348 && ((original >> 32) & 0xF8) == 0xB8;
        }

        /// <summary>
        /// First redirect of such a body: its second instruction becomes a jump to the relay. A
        /// thread is before that instruction or after it, never inside. While the jump is put
        /// together, a thread that arrives there spins on a two-byte self jump; both ends of that
        /// are single aligned stores, and nothing has to be waited for.
        /// </summary>
        internal static void EnterSecond(IntPtr from, long relay)
        {
            byte* second = (byte*)from + 4;
            int distance = (int)(relay - (from.ToInt64() + 9));
            Volatile.Write(ref *(ushort*)second, (ushort)Gate);
            second[2] = (byte)(distance >> 8); second[3] = (byte)(distance >> 16); second[4] = (byte)(distance >> 24);
            Volatile.Write(ref *(ushort*)second, (ushort)(0xE9 | (distance & 0xFF) << 8));
        }

        /// <summary>Puts the second instruction back the same way. `rest` is byte 8 of the body, the last of it.</summary>
        internal static void LeaveSecond(IntPtr from, ulong original, byte rest)
        {
            byte* second = (byte*)from + 4;
            Volatile.Write(ref *(ushort*)second, (ushort)Gate);
            second[2] = (byte)(original >> 48); second[3] = (byte)(original >> 56); second[4] = rest;
            Volatile.Write(ref *(ushort*)second, (ushort)(original >> 32));
        }

        /// <summary>
        /// Whether a whole jump may be written over the start of this body: when its first
        /// instruction contains all five bytes, as sub rsp, imm32 does with its seven. Over a
        /// shorter one the jump would end in the next instruction, and a thread that is about to
        /// execute that one would continue in the middle of the jump, however long it was waited for.
        /// </summary>
        internal static bool Gateable(ulong original)
        {
            return (original & 0xFFFFFF) == 0xEC8148;
        }

        /// <summary>
        /// Whether the body starts with sub rsp, imm8 (bytes 0-3 a four-byte first instruction).
        /// LoadsConstant is the subset of these whose second instruction is mov r32, imm32.
        /// </summary>
        internal static bool SubImm8(ulong original)
        {
            return (original & 0xFFFFFF) == 0xEC8348;
        }

        /// <summary>
        /// First redirect of a sub rsp, imm8 body whose fixed island address is taken, to an island
        /// chosen in the 16 MiB window whose top displacement byte is the body's fifth byte. Only
        /// bytes 0 to 3, the whole four-byte first instruction, change; byte 4, the first byte of
        /// the second instruction, stays. Written behind the gate like the whole jump: EB FE over
        /// bytes 0 and 1 (Close), one drain for the batch, then the displacement middle (bytes 2
        /// and 3), then E9 and the first displacement byte in one aligned store. No thread can be
        /// inside bytes 1 to 3, which lie within the first instruction, once the gate holds byte 0.
        /// </summary>
        internal static void OpenIsland(IntPtr from, long island)
        {
            ulong* word = (ulong*)from;
            ulong jump = IslandJump(from, island);
            // Bytes 2 and 3 of the displacement; the gate stays on bytes 0 and 1, byte 4 untouched.
            Volatile.Write(ref *word, (Volatile.Read(ref *word) & ~MiddleBytes) | (jump & MiddleBytes));
            // E9 and the first displacement byte in one aligned store; bytes 2 to 7 kept.
            Volatile.Write(ref *word, (Volatile.Read(ref *word) & ~GateBytes) | (jump & GateBytes));
        }

        /// <summary>
        /// Puts the saved first instruction back over an island jump of ours: one aligned store of
        /// bytes 0 to 3, keeping bytes 4 to 7. The four-byte instruction flips whole, and byte 4,
        /// the second instruction the jump borrowed as its top displacement byte, never changed.
        /// </summary>
        internal static void RestoreIsland(IntPtr from, ulong original)
        {
            ulong* word = (ulong*)from;
            Volatile.Write(ref *word, (Volatile.Read(ref *word) & ~FirstInstruction) | (original & FirstInstruction));
        }

        // E9 at byte 0 and the low three displacement bytes at bytes 1 to 3. The top displacement
        // byte stays the body's own fifth byte, which the window the island was chosen in makes equal.
        private static ulong IslandJump(IntPtr from, long island)
        {
            uint distance = (uint)(int)(island - (from.ToInt64() + 5));
            return 0xE9UL | ((distance & 0xFFFFFFUL) << 8);
        }

        // The first redirect of such a body without an island: the gated write of the design
        // contract. The steps below are single aligned stores: a thread arriving at the entry
        // spins on the two-byte self jump, the batch is drained once, then the jump is completed
        // behind the gate. Bytes 5-7 are read again for every store and never change through us.
        internal static void Close(IntPtr from)
        {
            ulong* word = (ulong*)from;
            Volatile.Write(ref *word, (Volatile.Read(ref *word) & ~GateBytes) | Gate);
        }

        internal static void Drain()
        {
            // The contract's pause between gate and jump. With the gate confined to a first
            // instruction that contains the jump, no thread is left for it to wait for.
            try { Thread.Sleep(1); }
            catch (ThreadInterruptedException) { }
        }

        internal static void Open(IntPtr from, IntPtr to)
        {
            ulong* word = (ulong*)from;
            ulong jump = Jump(Volatile.Read(ref *word), from, to);
            Volatile.Write(ref *word, (jump & ~GateBytes) | Gate);
            Volatile.Write(ref *word, jump);
        }

        /// <summary>
        /// Writes a whole jump in one store, old or new for every caller: over a jump of ours, or
        /// over a prologue no thread can be inside.
        /// </summary>
        internal static void Retarget(IntPtr from, IntPtr to)
        {
            ulong* word = (ulong*)from;
            Volatile.Write(ref *word, Jump(Volatile.Read(ref *word), from, to));
        }

        /// <summary>Puts the saved prologue back over a whole jump: one store.</summary>
        internal static void Restore(IntPtr from, ulong original)
        {
            ulong* word = (ulong*)from;
            Volatile.Write(ref *word, (Volatile.Read(ref *word) & ~JumpBytes) | (original & JumpBytes));
        }

        private static ulong Jump(ulong current, IntPtr from, IntPtr to)
        {
            ulong distance = (uint)(int)(to.ToInt64() - (from.ToInt64() + 5));
            return (current & ~JumpBytes) | 0xE9UL | (distance << 8);
        }

        private static bool Writable(long address, ref bool mapRead)
        {
            if (Mapped(address)) return true;
            // Reading the map costs milliseconds. Whatever the request compiles exists by now, so
            // one read decides for every entry of it.
            if (mapRead) return false;
            mapRead = true;
            mappings.Clear();
            string[] lines;
            try { lines = File.ReadAllLines("/proc/self/maps"); }
            catch (Exception) { return false; }
            foreach (string line in lines)
            {
                int dash = line.IndexOf('-'), space = line.IndexOf(' ');
                if (dash <= 0 || space <= dash + 1 || space + 3 >= line.Length) continue;
                if (line[space + 1] != 'r' || line[space + 2] != 'w' || line[space + 3] != 'x') continue;
                if (!ulong.TryParse(line.Substring(0, dash), NumberStyles.AllowHexSpecifier, CultureInfo.InvariantCulture, out ulong low)
                    || !ulong.TryParse(line.Substring(dash + 1, space - dash - 1), NumberStyles.AllowHexSpecifier, CultureInfo.InvariantCulture, out ulong high)
                    || high > long.MaxValue) continue;
                mappings.Add((long)low); mappings.Add((long)high);
            }
            return Mapped(address);
        }

        private static bool Mapped(long address)
        {
            for (int index = 0; index < mappings.Count; index += 2)
                if (address >= mappings[index] && address + 9 <= mappings[index + 1]) return true;
            return false;
        }
    }
}
