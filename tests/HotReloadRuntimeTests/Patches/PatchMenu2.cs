// Game.HotReload.Menu2: one of two generations with the same members in the same order.
// Build differs in its lambda, Rebuild in the helper it calls, Count in the iterator it runs, Walk
// in a method that calls itself, Wait in an async method; Keep, Enable and Steady, whose helpers
// call themselves and each other, are the same code in both. Stroll differs only in the method
// that Walk names as well.
namespace Patch.M2
{
    public static class Menus
    {
        public static void Build(Game.Menu self) { System.Func<string> click = () => "click2"; self.Shown = click(); }
        public static void Rebuild() { Game.Menu.Rebuilt += Step(); }
        private static int Step() { return 100; }
        public static void Keep() { System.Func<int> one = () => Unit(); Game.Menu.Kept += one(); }
        private static int Unit() { return 1; }
        public static void Count() { foreach (int value in Numbers()) Game.Menu.Counted += value; }
        private static System.Collections.Generic.IEnumerable<int> Numbers() { yield return 5; }
        public static void Enable(Game.Menu self) { self.Enabled = "enabled1"; }
        public static void Steady() { Game.Menu.Steadied += Factorial(3) + (Even(4) ? 1 : 0); }
        private static int Factorial(int value) { return value <= 1 ? 1 : value * Factorial(value - 1); }
        private static bool Even(int value) { return value == 0 || Odd(value - 1); }
        private static bool Odd(int value) { return value != 0 && Even(value - 1); }
        public static void Walk() { Game.Menu.Walked += Depth(3); }
        private static int Depth(int steps) { return steps == 0 ? 0 : 10 + Depth(steps - 1); }
        public static void Stroll() { Game.Menu.Strolled += Depth(1); }
        public static void Wait() { Game.Menu.Waited += Later().Result; }
        private static async System.Threading.Tasks.Task<int> Later() { await System.Threading.Tasks.Task.CompletedTask; return 100; }
    }
}
