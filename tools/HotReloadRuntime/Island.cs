using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Threading;

namespace UnityMicroEditor.HotReload
{
    // A first redirect must not change a byte another thread may be about to execute. Turning
    // only the first byte of a compiled body into E9 does not: bytes 1 to 4 stay what they were
    // and are now read as the distance of that jump, so it lands at an address the old prologue
    // decides. An island is the code mapped exactly there: jmp [rip+disp] through a pointer slot
    // beside it. Retargeting stores the slot, restoring puts the first byte back, and a later
    // first redirect of the same method finds its island again.
    //
    // Where that address cannot be mapped, a body of the form sub rsp, imm8; mov r32, imm32 is
    // left through its second instruction, which is as long as a jump and can be replaced whole.
    // The island of such a jump is a relay at an address of our choice: it first gives back the
    // stack the first instruction took.
    //
    // A thread may be inside an island at any time after its first use, so an island is never
    // unmapped. Only pages that a failed attempt mapped itself are returned: no jump leads there.
    internal sealed unsafe class Island
    {
        private const long PageSize = 4096;
        private const int CodeSize = 6, SlotSize = 8;
        // add rsp, imm8 and the jump, padded to the slot: three aligned words.
        private const int RelayCode = 10, RelaySize = 24;
        // PROT_READ | PROT_WRITE | PROT_EXEC, and MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE.
        private const int Protection = 0x1 | 0x2 | 0x4, Flags = 0x02 | 0x20 | 0x100000;
        // What a user process can map on x86-64: above vm.mmap_min_addr, below the 47-bit limit.
        private const long Lowest = 0x10000, Highest = 0x7FFFFFFFF000L;

        // The C library of the process. Its symbols are global, so the process itself has them;
        // asked for by name from an assembly that was loaded from bytes, the library is found as
        // well, but Mono then logs a failed assertion about the missing file name.
        private static class Global
        {
            [DllImport("__Internal")]
            internal static extern IntPtr mmap(IntPtr address, UIntPtr length, int protection, int flags, int file, IntPtr offset);

            [DllImport("__Internal")]
            internal static extern int munmap(IntPtr address, UIntPtr length);
        }

        private static class Named
        {
            [DllImport("libc")]
            internal static extern IntPtr mmap(IntPtr address, UIntPtr length, int protection, int flags, int file, IntPtr offset);

            [DllImport("libc")]
            internal static extern int munmap(IntPtr address, UIntPtr length);
        }

        private static bool named;

        // The pages mapped here, each with the islands that have bytes in it.
        private static readonly Dictionary<long, List<Island>> pages = new Dictionary<long, List<Island>>();
        // Those of them that were mapped for relays, which share a page while it has room.
        private static readonly List<long> relays = new List<long>();

        private readonly long code, slot;
        private readonly int length;

        private Island(long code, int length, long slot) { this.code = code; this.length = length; this.slot = slot; }

        /// <summary>Where a jump to the island goes.</summary>
        internal long Code { get { return code; } }

        /// <summary>
        /// Maps the island of the method whose body starts at the entry and still has the bytes the
        /// JIT gave it. Null with the reason when that cannot be done; nothing stays mapped then.
        /// </summary>
        internal static Island Map(IntPtr entry, out string problem)
        {
            long code = entry.ToInt64() + 5 + *(int*)(entry.ToInt64() + 1);
            if (code < Lowest + PageSize || code > Highest - 2 * PageSize) { problem = "the address lies outside the address space"; return null; }
            if (!Free(code, CodeSize)) { problem = "it overlaps the island of another method"; return null; }
            var mapped = new List<long>();
            try
            {
                if (!Reserve(code, CodeSize, mapped)) problem = "the address is occupied by another mapping";
                else
                {
                    // The aligned word after the jump, or the one before it when the island of a
                    // body 16 bytes further on is in the way.
                    foreach (long slot in new[] { (code + CodeSize + SlotSize - 1) & ~(SlotSize - 1L), (code - SlotSize) & ~(SlotSize - 1L) })
                    {
                        if (!Free(slot, SlotSize) || !Reserve(slot, SlotSize, mapped)) continue;
                        var island = new Island(code, CodeSize, slot);
                        byte* jump = (byte*)code;
                        jump[0] = 0xFF; jump[1] = 0x25;
                        *(int*)(jump + 2) = (int)(slot - (code + CodeSize));
                        foreach (long page in mapped) pages.Add(page, new List<Island>());
                        island.Register(code, CodeSize);
                        island.Register(slot, SlotSize);
                        problem = null;
                        return island;
                    }
                    problem = "no pointer slot is free beside it";
                }
            }
            catch (Exception error) when (error is DllNotFoundException || error is EntryPointNotFoundException)
            {
                problem = "mmap of the C library cannot be called (" + error.Message + ")";
            }
            // Mapped by this attempt only, and no jump was ever written that leads there.
            foreach (long page in mapped) Release(page);
            return null;
        }

        /// <summary>
        /// Maps a relay for a jump out of the body at the entry, after an instruction that took `frame`
        /// bytes of stack. Anywhere a 32-bit jump from the body reaches will do. Null with the reason
        /// when no page can be had.
        /// </summary>
        internal static Island Relay(IntPtr entry, byte frame, out string problem)
        {
            long from = entry.ToInt64(), cell = 0;
            var mapped = new List<long>();
            try
            {
                foreach (long page in relays)
                    if (Near(from, page) && (cell = Room(page)) != 0) break;
                // No page of ours has room in reach: a new one, well above the body and away from the JIT.
                for (int attempt = 0; cell == 0 && attempt < 64; attempt++)
                {
                    long page = ((from + 0x70000000L) & ~(PageSize - 1)) - attempt * 0x100000L;
                    if (page < Lowest || page > Highest - PageSize || pages.ContainsKey(page) || !Reserve(page, 1, mapped)) continue;
                    pages.Add(page, new List<Island>());
                    relays.Add(page);
                    cell = page;
                }
            }
            catch (Exception error) when (error is DllNotFoundException || error is EntryPointNotFoundException)
            {
                problem = "mmap of the C library cannot be called (" + error.Message + ")";
                return null;
            }
            if (cell == 0) { problem = "no page can be mapped in reach of the body"; return null; }
            var island = new Island(cell, RelayCode, cell + RelaySize - SlotSize);
            byte* code = (byte*)cell;
            // add rsp, imm8; jmp [rip+disp]
            code[0] = 0x48; code[1] = 0x83; code[2] = 0xC4; code[3] = frame;
            code[4] = 0xFF; code[5] = 0x25;
            *(int*)(code + 6) = RelaySize - SlotSize - RelayCode;
            island.Register(cell, RelaySize);
            problem = null;
            return island;
        }

        /// <summary>Makes the island lead to the target: one aligned store, old or new for a thread inside it.</summary>
        internal void Aim(IntPtr target)
        {
            Volatile.Write(ref *(long*)slot, target.ToInt64());
        }

        private void Register(long address, int size)
        {
            for (long page = address & ~(PageSize - 1); page < address + size; page += PageSize)
                if (!pages[page].Contains(this)) pages[page].Add(this);
        }

        private static bool Free(long address, int size)
        {
            for (long page = address & ~(PageSize - 1); page < address + size; page += PageSize)
            {
                if (!pages.TryGetValue(page, out List<Island> islands)) continue;
                foreach (Island island in islands)
                    if ((address < island.code + island.length && island.code < address + size) || (address < island.slot + SlotSize && island.slot < address + size))
                        return false;
            }
            return true;
        }

        // Makes every page of the range ours. What it mapped is added to the list, also when a
        // later page of the range belongs to something else and the answer is false.
        private static bool Reserve(long address, int size, List<long> mapped)
        {
            for (long page = address & ~(PageSize - 1); page < address + size; page += PageSize)
            {
                if (pages.ContainsKey(page) || mapped.Contains(page)) continue;
                long got = Allocate(page);
                if (got != page)
                {
                    // A kernel that does not know MAP_FIXED_NOREPLACE takes the address as a hint.
                    if (got != -1) Release(got);
                    return false;
                }
                // Filled with int3: a jump that went astray would stop at once.
                for (ulong* word = (ulong*)page; word < (ulong*)(page + PageSize); word++) *word = 0xCCCCCCCCCCCCCCCCUL;
                mapped.Add(page);
            }
            return true;
        }

        // Within reach of a 32-bit jump from anywhere in the first bytes of the body, with room to spare.
        private static bool Near(long from, long page)
        {
            return Math.Abs(page - from) < 0x7FF00000L;
        }

        /// <summary>The first free cell for a relay in the page, or zero.</summary>
        private static long Room(long page)
        {
            for (long cell = page; cell + RelaySize <= page + PageSize; cell += RelaySize)
                if (Free(cell, RelaySize)) return cell;
            return 0;
        }

        private static long Allocate(long page)
        {
            if (!named)
            {
                try { return Global.mmap((IntPtr)page, (UIntPtr)PageSize, Protection, Flags, -1, IntPtr.Zero).ToInt64(); }
                catch (EntryPointNotFoundException) { named = true; }
                catch (DllNotFoundException) { named = true; }
            }
            return Named.mmap((IntPtr)page, (UIntPtr)PageSize, Protection, Flags, -1, IntPtr.Zero).ToInt64();
        }

        private static void Release(long page)
        {
            if (named) Named.munmap((IntPtr)page, (UIntPtr)PageSize);
            else Global.munmap((IntPtr)page, (UIntPtr)PageSize);
        }
    }
}
