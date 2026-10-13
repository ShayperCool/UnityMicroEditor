// Game.HotReload.Islands: replacements of methods whose compiled bodies start with the one-byte
// push rbp. Only a redirect through an island may replace such a prologue under other threads.
namespace PatchIslands
{
    public static class Framed
    {
        public static int Seven(int a, int b, int c, int d, int e, int f, int g) { return a + g + 100; }
        public static int CallsSeven() { return 1; }
    }
}
