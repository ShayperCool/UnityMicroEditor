// Game.HotReload.Denied: compiled WITHOUT -unsafe+, so it lacks the SkipVerification declaration.
// The JIT would refuse its field access, but compile its call of a non-public method and throw
// MethodAccessException only when the call runs. The applier refuses such an assembly unloaded.
namespace PatchDenied
{
    public static class Players
    {
        public static int Twice(int value) { return value + 5; }
        public static int Health(Game.Player self) { return self.health + self.Secret(1); }
    }
}
