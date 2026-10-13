// Game.HotReload.Menu1: one of two generations with the same members in the same order.
// Build differs in its lambda, Rebuild in the helper it calls, Count in the iterator it runs; Keep
// and Enable are the same code in both.
namespace Patch.M1
{
    public static class Menus
    {
        public static void Build(Game.Menu self) { System.Func<string> click = () => "click1"; self.Shown = click(); }
        public static void Rebuild() { Game.Menu.Rebuilt += Step(); }
        private static int Step() { return 10; }
        public static void Keep() { System.Func<int> one = () => Unit(); Game.Menu.Kept += one(); }
        private static int Unit() { return 1; }
        public static void Count() { foreach (int value in Numbers()) Game.Menu.Counted += value; }
        private static System.Collections.Generic.IEnumerable<int> Numbers() { yield return 1; }
        public static void Enable(Game.Menu self) { self.Enabled = "enabled1"; }
    }
}
