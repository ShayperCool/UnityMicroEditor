// Fixture of the hot reload SDK tests: the assembly a Player "loaded".
namespace Game
{
    public class Counter
    {
        private int value;
        public int Next() { value += 1; return value; }
        public static string Label() { return "one"; }
    }
}
