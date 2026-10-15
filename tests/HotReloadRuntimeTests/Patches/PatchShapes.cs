// Game.HotReload.Shapes: replacements whose native signature differs from the original's. A jump
// keeps the caller's registers, so each of them must be refused before anything is compiled.
namespace PatchShapes
{
    public class Wrong
    {
        // A struct method as a static method taking the struct by reference: `this` and the hidden return buffer swap.
        public static Game.Vec3 Scaled(ref Game.Vec3 self, float factor) { return self; }
        public static int HealthMore(Game.Player self, int more) { return more; }
        public static string HealthText(Game.Player self) { return "text"; }
        public static int HealthOther(Game.Dog self) { return 0; }
        public static int TwiceLong(long value) { return 0; }
        public int TwiceInstance(int value) { return 0; }
        public static T Echo<T>(T value) { return value; }
        public static int Good(int value) { return value + 9; }
    }
}
