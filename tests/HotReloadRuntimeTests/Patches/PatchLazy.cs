// Game.HotReload.Lazy: an edit of a type the game has not used yet. Compiling the pair runs the
// static initializer of Game.Lazy at patch time, and that initializer fails before the game is ready.
namespace PatchLazy
{
    public static class Players
    {
        public static int Twice(int value) { return value + 3; }
    }

    public static class Lazies
    {
        public static int Value() { return Game.Lazy.seed + 1; }
    }
}
