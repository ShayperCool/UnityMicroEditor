// Game.HotReload.Stale: built against another build of the game. One replacement names a field the
// loaded assembly does not have, so the JIT refuses it; the good one beside it must not be applied.
namespace PatchStale
{
    public static class Players
    {
        public static int Twice(int value) { return value + 5; }
        public static int Health(Game.Player self) { return self.absent; }
    }
}
