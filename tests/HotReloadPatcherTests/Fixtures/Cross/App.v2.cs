namespace App
{
    public class Shop
    {
        public int Total(Core.Counter c) => Core.Counter.Triple(c.Next());
        public int Extra(Core.Counter c) => c.Value + c.extra;
    }

    public class Stopwatch : Core.Timer
    {
        public int Laps;
    }
}
