namespace App
{
    public class Shop
    {
        public int Total(Core.Counter c) => Core.Counter.Scale(c.Next());
        public int Extra(Core.Counter c) => c.Value;
    }

    public class Stopwatch : Core.Timer
    {
        public int Laps;
    }
}
