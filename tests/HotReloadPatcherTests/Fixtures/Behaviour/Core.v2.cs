// Game.Core.dll after the edit. Everything here must be carried by a patch.
namespace Game.Core
{
    public static class Rules
    {
        // const changed: the methods that use it, here and in Game.dll, were recompiled
        public const int Bonus = 4;
        public static int Clamp(int v) => v < 1 ? 1 : v;
        // calls an added method and the first extension method of this class
        public static int Scale(int v) => Twice(v) + 1.Doubled() + Bonus;
        private static int Twice(int v) => v * 2;
        private static int Doubled(this int v) => v + v;
    }

    public class Counter
    {
        private int value;
        // added instance field with an initializer
        private int step = 2;
        public int Next() => value += step;
        public int Value => value;
    }
}
