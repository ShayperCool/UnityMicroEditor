namespace Core
{
    public class Counter
    {
        private int value;
        public int Next() => ++value;
        public int Value => value;
        public static int Scale(int x) => x * 2;
    }

    // A plain class another assembly derives from.
    public class Timer
    {
        public int Elapsed;
    }
}
