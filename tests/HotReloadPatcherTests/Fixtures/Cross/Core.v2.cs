namespace Core
{
    public class Counter
    {
        private int value;
        public int Next() => ++value;
        public int Value => value;
        public static int Scale(int x) => x * 2;
        // added in this change, and used by the other assembly at once
        public static int Triple(int x) => x * 3;
        public int extra;
    }

    public class Timer
    {
        public int Elapsed;
    }
}
