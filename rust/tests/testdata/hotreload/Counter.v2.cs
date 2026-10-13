// Two changed method bodies: a patch.
namespace Game
{
    public class Counter
    {
        private int value;
        public int Next() { value += 2; return value; }
        public static string Label() { return "two"; }
    }
}
