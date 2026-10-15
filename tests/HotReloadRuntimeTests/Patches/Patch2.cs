// Game.HotReload.2: the next generation. It replaces generation 1 entirely; the methods it does not
// list get their original code back.
namespace Patch.G2
{
    public static class Players
    {
        public static int Health(Game.Player self) { return self.health + 200; }
        public static int Twice(int value) { return value * 2 + 2000; }
    }

    public static class Voices
    {
        // The same text in both generations: a lambda, a generic list and a try block, all compiled into
        // the patch. Only the helper it calls differs, so this body did not change with generation 2.
        public static string Animal(Game.Animal self)
        {
            try
            {
                var names = new System.Collections.Generic.List<string> { "animal" };
                System.Func<string, string> mark = name => name + Suffix();
                return mark(names[0]);
            }
            catch (System.InvalidOperationException) { return "failed"; }
        }

        private static string Suffix() { return "2"; }
        public static string Dog(Game.Dog self) { return "dog2"; }
    }

    public static class Others
    {
        public static long Work(long value)
        {
            long result = value * 3;
            return result + 3;
        }

        public static long Guarded(long value)
        {
            try { return value * 5 + 2; }
            finally { Game.Hot.Leave(); }
        }

        public static long Flat(long value) { return 24; }
        public static long Large(long value) { return value * 3 + 3; }

        public static void Tick(Game.Ticker self) { self.Ticks += 1000; }

        // The same body as in generation 1: this method did not change with generation 2.
        public static string LocalText() { Game.Reloads.Local += 100; return "local1"; }
    }

    // Not patched in generation 1: a first redirect in a batch that also retargets existing ones.
    public unsafe struct Vec3
    {
        public float Sum()
        {
            fixed (Vec3* self = &this) return ((Game.Vec3*)self)->x + 2000;
        }
    }
}
