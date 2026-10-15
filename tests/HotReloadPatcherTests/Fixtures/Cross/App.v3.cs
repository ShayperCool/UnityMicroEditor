namespace App
{
    public class Shop
    {
        public int Total(Core.Counter c) => Core.Counter.Scale(c.Next());
        public int Extra(Core.Counter c) => c.Value;
    }

    // Its base lives in an assembly that did not change: whether Update is a Unity message
    // depends on reading that assembly next to the loaded one.
    public class Stopwatch : Core.Timer
    {
        public int Laps;
        public void Update() { Laps++; }
    }
}
