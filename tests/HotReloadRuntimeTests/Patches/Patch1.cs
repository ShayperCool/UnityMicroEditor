// Game.HotReload.1: hand-written in the forms the patch builder emits, in a namespace named after
// the generation. Compiled with -unsafe+, which gives the assembly the SkipVerification declaration
// that opens the game's non-public members to it.
namespace Patch.G1
{
    // Class receivers: a static method with the instance as its first parameter.
    public static class Players
    {
        public static int Health(Game.Player self) { return self.health + 100; }
        public static int Level(Game.Player self) { return 2; }
        public static string Describe(Game.Player self, string prefix, int times) { return "p1:" + prefix + times + ":" + self.Secret(1); }
        public static int Twice(int value) { return value * 2 + 1000; }
        public static void Construct(Game.Player self) { Game.Player.created += 100; self.Name = "ctor1"; self.health = 11; }
        public static Game.Wide Stats(Game.Player self, long seed) { return new Game.Wide { A = seed + 1, B = self.health, C = 30, D = 40 }; }
    }

    // Struct receivers: an instance method of a stand-in struct; `this` is the game's struct.
    public unsafe struct Vec3
    {
        public Game.Vec3 Scaled(float factor)
        {
            fixed (Vec3* self = &this)
            {
                Game.Vec3* vector = (Game.Vec3*)self;
                return new Game.Vec3(vector->x * factor + 1, vector->y * factor + 1, vector->z * factor + 1);
            }
        }

        public Game.Wide Spread(long seed)
        {
            fixed (Vec3* self = &this)
            {
                Game.Vec3* vector = (Game.Vec3*)self;
                return new Game.Wide { A = seed + 1, B = (long)vector->x, C = (long)vector->y, D = (long)vector->z + 1 };
            }
        }

        public void Shift(float by)
        {
            fixed (Vec3* self = &this) ((Game.Vec3*)self)->x += by * 10;
        }
    }

    // A stand-in with the layout of the original works as well.
    public struct Terse
    {
        public int Count;
        public string Greet(string name) { return "p1 hi " + name + Count; }
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

        private static string Suffix() { return "1"; }
        public static string Dog(Game.Dog self) { return "dog1"; }
        public static string Greeter(Game.Greeter self, string name) { return "p1 hello " + name; }
        public static string Formal(Game.Formal self, string name) { return "p1 good day " + name; }
    }

    public static class Others
    {
        public static long Work(long value)
        {
            long result = value * 3;
            return result + 2;
        }

        public static long Guarded(long value)
        {
            try { return value * 5 + 1; }
            finally { Game.Hot.Leave(); }
        }

        public static long Flat(long value) { return 23; }
        public static long Large(long value) { return value * 3 + 2; }

        // A removed Unity message is redirected to an empty method.
        public static void Update(Game.Ticker self) { }
        public static void Tick(Game.Ticker self) { self.Ticks += 10; }
        public static string LocalText() { Game.Reloads.Local += 100; return "local1"; }

        // Not a replacement: an added method that the replacements could call. It is compiled with the patch.
        public static int Added(Game.Player self) { return self.health * 2; }
    }
}
