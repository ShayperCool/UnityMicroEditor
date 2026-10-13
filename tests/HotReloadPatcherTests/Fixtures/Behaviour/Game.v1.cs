// The upper game assembly of the behaviour test (Game.dll), as the Player loaded it. Game.v2.cs is
// the same file after every kind of edit a patch carries; Driver.cs calls both the same way.
using System;
using System.Collections;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;
using Game.Core;
using UnityEngine;

namespace Game
{
    public struct Vec
    {
        public float X, Y;
        public Vec(float x, float y) { X = x; Y = y; }
        public float Length() => (float)Math.Sqrt(X * X + Y * Y);
        public Vec Scaled(float f) => new Vec(X * f, Y * f);
        public static Vec One() => new Vec(1, 1);
        public override string ToString() => "(" + X + ";" + Y + ")";
        public string Text() => "v" + ToString();
        public static float Sum(ref Vec v) => v.X + v.Y;
        public float Total() => Sum(ref this);
    }

    public struct Vec3
    {
        public float X, Y, Z;
        public Vec3(float x, float y, float z) { X = x; Y = y; Z = z; }
        public Vec3 Plus(Vec3 o) => new Vec3(X + o.X, Y + o.Y, Z + o.Z);
    }

    public struct Big
    {
        public double A, B, C, D;
        public Big Twice() { Big r; r.A = A * 2; r.B = B * 2; r.C = C * 2; r.D = D * 2; return r; }
        public double Sum() => A + B + C + D;
    }

    public interface IScored { int Score(); }

    public class Entity
    {
        protected int id = 7;
        public virtual string Describe() => "entity#" + id;
        public virtual int Rank() => 1;
    }

    public class Inventory
    {
        public sealed class Slot
        {
            public int Count;
            public string Label = "slot";
            public string Show() => Label + ":" + Count;
        }

        private readonly List<Slot> slots = new List<Slot>();
        public int this[int i] { get { return slots[i].Count; } set { slots[i].Count = value; } }
        public Slot Add(string label, int count) { var s = new Slot { Label = label, Count = count }; slots.Add(s); return s; }
        public int Total { get { int t = 0; foreach (var s in slots) t += s.Count; return t; } }
        public static Inventory operator +(Inventory a, int n) { a.Add("bonus", n); return a; }
        public static explicit operator int(Inventory a) => a.slots.Count;
    }

    public static class Util
    {
        public static T As<T>(object o, T example) => (T)o;

        /// File and line of the frame that threw.
        public static string Frame(Exception e)
        {
            string first = (e.StackTrace ?? "").Split('\n')[0];
            int at = first.LastIndexOf(" in ", StringComparison.Ordinal);
            if (at < 0) return "no-line";
            string location = first.Substring(at + 4).Trim();
            return location.Substring(location.LastIndexOfAny(new[] { '/', '\\' }) + 1);
        }
    }

    public class Player : Entity, IScored, IDisposable
    {
        private int health = 100;
        private static int instances;
        private float speed = 1.5f;
        private readonly List<int> scores = new List<int> { 3, 1, 2 };
        public string Name { get; set; } = "hero";
        private bool disposed;

        public Player() { instances++; }
        public Player(string name) : this() { Name = name; }

        public int Damage(int amount) { health -= amount; return health; }
        private int Secret() => health * 2;
        public string Report() => Name + ":" + Secret();
        private int Heal(int amount) { health += amount; return health; }
        public int Turn() => Heal(Rules.Bonus);
        private int Mix(int a) => a + health;
        public int Mixed(int a) => Mix(a);
        private int OldName() => 1;
        public int Named() => OldName();
        public List<int> Above(int threshold) => scores.Where(s => s > threshold).ToList();
        public int Best() => scores.OrderByDescending(s => s).First();
        public int CountBelowHealth() => scores.Count(s => s < health);
        public int CountNear(int d) => scores.Count(s => s + d > health);

        public IEnumerator Countdown(int from)
        {
            for (int i = from; i > 0; i--) yield return i;
        }

        public IEnumerable<int> Evens(int max)
        {
            for (int i = 0; i <= max; i += 2) yield return i;
        }

        public async Task<int> LoadAsync(int delay)
        {
            await Task.Delay(delay);
            return health;
        }

        public override string Describe() => base.Describe() + "/" + Name;
        public static int Count() => instances;

        public int Table(int i)
        {
            int[] table = { 10, 20, 30, 40, 50, 60, 70, 80 };
            int Pick(int k) => table[k % table.Length];
            return Pick(i);
        }

        public Big MakeBig(double seed) { Big b; b.A = seed; b.B = seed + 1; b.C = seed + 2; b.D = health; return b; }
        public Vec3 Corner(float s) => new Vec3(s, s * 2, health);
        public static Big StaticBig(double seed) { Big b; b.A = seed; b.B = 0; b.C = 0; b.D = instances; return b; }
        public int Health => health;
        public float Speed { get { return speed; } set { speed = value; } }
        int IScored.Score() => health + scores.Count;
        void IDisposable.Dispose() { disposed = true; }
        public bool Disposed => disposed;

        public Func<int, int> MakeAdder(int n) => x => x + n;
        public Func<int> MakeCounter(int start) { int c = start; return () => ++c; }
        public Func<int, bool> BelowHealth() => s => s < health;
        public Func<int, int> Doubler() => x => x * 2;

        public IEnumerator Ticker(int n)
        {
            int Step(int k) => k + n;
            int i = 0;
            while (i < 1000) { i = Step(i); yield return i; }
        }

        public string Kind(string key)
        {
            switch (key)
            {
                case "a": return "alpha";
                case "b": return "beta";
                case "c": return "gamma";
                case "d": return "delta";
                case "e": return "epsilon";
                case "f": return "zeta";
                case "g": return "eta";
                case "h": return "theta";
                default: return "?";
            }
        }

        public object MakePair() => new { Name = Name, Value = health };
        public string ReadPair(object pair) { var p = Util.As(pair, new { Name = "", Value = 0 }); return p.Name + "#" + p.Value; }

        public int Guarded(int x)
        {
            try { if (x < 0) throw new ArgumentException("neg"); return 100 / x; }
            catch (DivideByZeroException) { return -1; }
            finally { health += 1; }
        }

        public string Where() { try { Fail(); return "no"; } catch (Exception e) { return Util.Frame(e); } }
        private void Fail() { throw new InvalidOperationException("boom"); }
        public int Stored(Inventory bag) { bag[0] = bag[0] + 1; return bag.Total; }
    }

    public class Spinner : MonoBehaviour
    {
        public int Ticks;
        private void Update() { Ticks++; }
        private void LateUpdate() { Ticks += 10; }
    }
}
