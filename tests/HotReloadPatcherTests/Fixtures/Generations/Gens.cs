// Fixture of the tests with two patches in a row. One assembly in three versions: without a symbol
// it is what the Player loaded, GEN_1 is the first edit and GEN_2 the second. Every patch is built
// against the loaded version, and the second replaces the first.
using System;
using System.Collections.Generic;

namespace Gens
{
    public class Thing
    {
        public static event Action Fired;
        public static readonly List<string> Log = new List<string>();
        public static void Fire() { if (Fired != null) Fired(); }

        // Edited by the first patch and back as it was in the second.
#if GEN_1
        public string Edit() => "e2";
#else
        public string Edit() => "e1";
#endif

        // An added field keeps its value from one patch to the next.
#if GEN_1 || GEN_2
        private int count;
        public string Count() { count++; return "count=" + count; }
#else
        public string Count() => "none";
#endif

        // An added method that both patches have, with another body: a handler subscribed under the
        // first patch is that patch's copy.
#if GEN_1
        private void Handler() { Log.Add("handler-1"); }
        public void Subscribe() { Fired += Handler; }
        public void Unsubscribe() { Fired -= Handler; }
#elif GEN_2
        private void Handler() { Log.Add("handler-2"); }
        public void Subscribe() { Fired += Handler; }
        public void Unsubscribe() { Fired -= Handler; }
#else
        public void Subscribe() { }
        public void Unsubscribe() { }
#endif

        // An added method that is the same in both patches.
#if GEN_1 || GEN_2
        private int Steady(int x) => x + 1;
        public int UseSteady() => Steady(1);
#else
        public int UseSteady() => 0;
#endif

        // A lambda edited by both patches.
        public Func<int> Scaled(int k)
        {
#if GEN_1
            return () => k * 2;
#elif GEN_2
            return () => k * 3;
#else
            return () => k;
#endif
        }

        // The method changes, so each patch brings a copy of its lambda; the lambda never changes.
        public Func<int> Kept(int k)
        {
#if GEN_1
            Log.Add("kept-1");
#elif GEN_2
            Log.Add("kept-2");
#endif
            return () => k + 100;
        }

        // An iterator edited by both patches: one that the first patch started finishes on it.
        public IEnumerable<int> Steps()
        {
#if GEN_1
            yield return 10;
            yield return 11;
#elif GEN_2
            yield return 20;
            yield return 21;
#else
            yield return 0;
            yield return 1;
#endif
        }
    }
}
