// Game.HotReload.Local: the static initializer that fails belongs to a type of the patch itself.
// The patch is refused and no type of the game is lost.
namespace PatchLocal
{
    public static class Players
    {
        private static readonly int seed = Fail();
        private static int Fail() { throw new System.InvalidOperationException("the patch initializer failed"); }
        public static int Twice(int value) { return value + seed; }
    }
}
