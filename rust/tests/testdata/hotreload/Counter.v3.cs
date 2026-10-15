// An added type next to the changed bodies: refused as a whole.
namespace Game
{
    public class Counter
    {
        private int value;
        public int Next() { value += 3; return value; }
        public static string Label() { return "three"; }
    }
    public class Added { }
}
