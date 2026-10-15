// Game.HotReload.Eager: an edit of a type the game has not used yet, whose static initializer
// succeeds. It runs when the request compiles the pair, also when the request is then refused.
namespace PatchEager
{
    public static class Eagers
    {
        public static int Value() { return Game.Eager.seed + 1; }
    }
}
