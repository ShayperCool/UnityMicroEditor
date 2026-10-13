// Game.HotReload.Helpers: replacements for lifecycle helpers and for methods that only look like one.
namespace Patch.Helpers
{
    public static class Crowds
    {
        public static void Helper(Game.Crowded self) { self.Enabled = "crowded1"; }
        public static void Own(Game.Crowded self) { self.Own += 10; }
        public static void Later(Game.Crowded self) { self.Later += 10; }
        public static void FooHelper(Game.Crowded self) { self.Fooed += 100; }
        public static void PanelEnable(Game.Panel self) { self.Seen += "panel1;"; }
    }
}
