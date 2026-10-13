using System;
using System.Reflection;
using System.Runtime.CompilerServices;

namespace UnityMicroEditor.HotReload
{
    // A JIT that inlines copies small methods into their callers, where no redirect reaches them:
    // the edit would be reported as applied and the old body would keep running. The canary
    // redirects three methods the JIT inlines whenever it may, then lets it compile their callers.
    internal static class Canary
    {
        // Must stay inlinable: no static state (a pending type initializer blocks inlining) and no attributes.
        private sealed class Probe
        {
            internal static int Small() { return 1; }
            internal int Value { get { return 1; } }

            // Above the size limit of the JIT; inlined on request even where the limit is zero.
            [MethodImpl(MethodImplOptions.AggressiveInlining)]
            internal static int Forced(int seed) { return seed * 3 + seed * 5 + seed * 7 + seed * 11 + seed * 13 + seed * 17 + 1; }

            internal static int SmallPatched() { return 2; }
            internal static int ValuePatched(Probe self) { return 2; }
            internal static int ForcedPatched(int seed) { return 2; }

            [MethodImpl(MethodImplOptions.NoInlining)]
            internal static bool Redirected() { return Small() == 2 && new Probe().Value == 2 && Forced(0) == 2; }
        }

        private static int state;

        /// <summary>
        /// True when redirects reach every caller. Decided once; the JIT options are fixed per process.
        /// Throws when the probes cannot be redirected: nothing is written and nothing is decided
        /// then, and the next call checks again.
        /// </summary>
        internal static bool InliningDisabled()
        {
            if (state == 0)
            {
                const BindingFlags Declared = BindingFlags.NonPublic | BindingFlags.Static | BindingFlags.Instance | BindingFlags.DeclaredOnly;
                string[] names = { "Small", "SmallPatched", "get_Value", "ValuePatched", "Forced", "ForcedPatched" };
                // All six are compiled before the first entry is looked up in the process map.
                var entries = new IntPtr[names.Length];
                for (int index = 0; index < names.Length; index++) entries[index] = Detour.Entry(typeof(Probe).GetMethod(names[index], Declared));
                bool mapRead = false;
                for (int index = 0; index < names.Length; index += 2)
                {
                    string refusal = Detour.Refusal(entries[index], entries[index + 1], ref mapRead);
                    if (refusal == null && !Detour.Reaches(entries[index], entries[index + 1])) refusal = "The replacement is out of reach of a 32-bit jump.";
                    if (refusal != null) throw new InvalidOperationException(refusal);
                }
                // Nothing has called the probes yet, so no thread is inside them and a whole jump is safe.
                for (int index = 0; index < names.Length; index += 2) Detour.Retarget(entries[index], entries[index + 1]);
                // Compiled only now: an inlining JIT has copied the original bodies into this caller.
                state = Probe.Redirected() ? 1 : 2;
            }
            return state == 1;
        }
    }
}
