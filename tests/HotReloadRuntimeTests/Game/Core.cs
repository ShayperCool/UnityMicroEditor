// A second game assembly: a descriptor lists one patch assembly per game assembly.
namespace Game.Core
{
    public static class Rules
    {
        public static int Reloaded;

        public static int Limit() { return 5; }

        [UnityMicroEditor.InvokeOnHotReload]
        public static void OnReload() { Reloaded++; }
    }
}
