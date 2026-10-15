// The lower of the two game assemblies of the behaviour test (Game.Core.dll), as the Player loaded it.
namespace Game.Core
{
    public static class Rules
    {
        public const int Bonus = 3;
        public static int Clamp(int v) => v < 0 ? 0 : v;
        public static int Scale(int v) => v * 2;
    }

    public class Counter
    {
        private int value;
        public int Next() => ++value;
        public int Value => value;
    }
}
