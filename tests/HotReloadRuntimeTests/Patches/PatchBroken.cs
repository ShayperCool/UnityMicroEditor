// Game.HotReload.Broken: its replacement compiles, but a method the replacement would call later
// does not. The applier compiles the whole patch before it writes anything.
namespace PatchBroken
{
    public static class Players
    {
        public static int Twice(int value) { return value + 7; }
        private static int Helper(Game.Player self) { return self.Absent(); }
    }
}
