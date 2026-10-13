// Game.dll after the edit: every kind of change a patch carries, and nothing a patch refuses.
// The comments name the kind; unmarked members are unchanged (their generated members renumber).
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
        // added instance method of a struct, called from a changed one
        public float Dot(Vec o) => X * o.X + Y * o.Y;
        // struct receiver, scalar return
        public float Length() => (float)Math.Sqrt(Dot(this)) + 1;
        // struct receiver, 8-byte struct return
        public Vec Scaled(float f) => new Vec(X * f + 1, Y * f + 1);
        // static method of a struct
        public static Vec One() => new Vec(1, 1).Scaled(3);
        public override string ToString() => "(" + X + ";" + Y + ")";
        // constrained calls on the receiver of a struct method, one to an override and one that boxes
        public string Text() => ToString() + "!" + (GetHashCode() == GetHashCode());
        public static float Sum(ref Vec v) => v.X + v.Y;
        // the receiver of a struct method passed on by reference
        public float Total() => Sum(ref this) * 2;
    }

    public struct Vec3
    {
        public float X, Y, Z;
        // constructor of a struct
        public Vec3(float x, float y, float z) { X = x; Y = y; Z = z + 0.5f; }
        // struct receiver, 12-byte argument and return (hidden return buffer)
        public Vec3 Plus(Vec3 o) => new Vec3(X + o.X + 1, Y + o.Y + 2, Z + o.Z + 3);
    }

    public struct Big
    {
        public double A, B, C, D;
        // added static field of a struct
        private static int twices;
        // struct receiver, 32-byte return
        public Big Twice() { twices++; Big r; r.A = A * 3; r.B = B * 3; r.C = C * 3; r.D = D * 3 + twices; return r; }
        public double Sum() => A + B + C + D + twices;
    }

    public interface IScored { int Score(); }

    public class Entity
    {
        protected int id = 7;
        // virtual body
        public virtual string Describe() => "Entity<" + id + ">";
        public virtual int Rank() => 1;
    }

    public class Inventory
    {
        public sealed class Slot
        {
            public int Count;
            public string Label = "slot";
            // method of a nested class
            public string Show() => "[" + Label + "=" + Count + "]";
        }

        private readonly List<Slot> slots = new List<Slot>();
        // added auto-property with an initializer: a backing field, two accessors, a changed constructor
        public int Capacity { get; set; } = 10;
        // indexer setter
        public int this[int i] { get { return slots[i].Count; } set { slots[i].Count = value + 1; } }
        public Slot Add(string label, int count) { var s = new Slot { Label = label, Count = count }; slots.Add(s); return s; }
        // property getter
        public int Total { get { int t = Capacity; foreach (var s in slots) t += s.Count; return t; } }
        // operators
        public static Inventory operator +(Inventory a, int n) { a.Add("bonus", n * 2); return a; }
        public static explicit operator int(Inventory a) => a.slots.Count + 100;
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
        // field retyped: stored anew
        private double speed = 2.5;
        private readonly List<int> scores = new List<int> { 3, 1, 2 };
        // added instance field with an initializer
        private int armor = 5;
        // added static field
        private static int bonusPool;
        // added field of a struct type: read and written through its address
        private Vec offset;
        public string Name { get; set; } = "hero";
        private bool disposed;

        public Player() { instances++; }
        // constructor body
        public Player(string name) : this() { Name = name + "!"; }

        // added before the others: the ordinal of every generated member below shifts
        private int Shield(int x) => scores.Sum(s => s * 2) + x;
        // added property
        public int Armor { get { return armor; } }

        // body: added fields, an added method, private members
        public int Damage(int amount) { health -= amount * 2 - armor; armor++; offset.X += 1.5f; if (health < 0) health = 0; return health + Shield(0) + armor + (int)offset.X; }
        private int Secret() => health * 2;
        // added event: its accessors reach the side table by address
        private event Action<int> Struck;
        private int struck;
        public string Report() { Struck += v => struck += v; Struck?.Invoke(2); return Name + "=" + Secret() + "/" + health + "/" + Armor + "/" + struck; }
        // signature changed: its caller was recompiled
        private int Heal(int amount, bool clamp) { health += amount; if (clamp && health > 100) health = 100; return health; }
        public int Turn() => Heal(Rules.Bonus, false) + Rules.Clamp(-1);
        // instance method made static
        private static int Mix(int a) => a * 2;
        public int Mixed(int a) => Mix(a);
        // renamed
        private int NewName() => 2;
        public int Named() => NewName();
        // lambda with a closure, and one more lambda
        public List<int> Above(int threshold) => scores.Where(s => s >= threshold).Select(s => s + threshold).ToList();
        public int Best() => scores.OrderByDescending(s => s).First();
        public int CountBelowHealth() => scores.Count(s => s < health);
        // lambda that captures `this`, and a delegate over an added method
        public int CountNear(int d) => scores.Count(s => s + d + 97 > health) + scores.Where(IsOdd).Count();
        private bool IsOdd(int v) => v % 2 == 1;

        // iterator
        public IEnumerator Countdown(int from)
        {
            for (int i = from; i > 0; i--) yield return i * 10;
            yield return "liftoff";
        }

        public IEnumerable<int> Evens(int max)
        {
            for (int i = 0; i <= max; i += 2) yield return i;
        }

        // async method
        public async Task<int> LoadAsync(int delay)
        {
            await Task.Delay(delay);
            await Task.Yield();
            return health + 1;
        }

        // override with a base call
        public override string Describe() => "[" + base.Describe() + "|" + Name + "]";
        // static method, added static field
        public static int Count() { bonusPool += 3; return instances * 100 + bonusPool; }

        // array initializer data and a local function that captures more
        public int Table(int i)
        {
            int[] table = { 11, 21, 31, 41, 51, 61, 71, 81 };
            int Pick(int k) => table[k % table.Length] + i;
            return Pick(i);
        }

        // class receiver, struct returns of 32 and 12 bytes
        public Big MakeBig(double seed) { Big b; b.A = seed * 10; b.B = seed + 10; b.C = seed + 20; b.D = health + 0.5; return b; }
        public Vec3 Corner(float s) => new Vec3(s + 1, s * 3, health - 1);
        public static Big StaticBig(double seed) { Big b; b.A = seed + 0.25; b.B = 1; b.C = 2; b.D = instances + 3; return b; }
        public int Health => health;
        // property accessors over the retyped field
        public float Speed { get { return (float)speed; } set { speed = value * 2; } }
        // explicit interface implementations
        int IScored.Score() => health * 2 + scores.Count;
        void IDisposable.Dispose() { disposed = health > 0; }
        public bool Disposed => disposed;

        // lambda edited in place: delegates made before the patch run the new body
        public Func<int, int> MakeAdder(int n) => x => x + n + 1000;
        // lambda that captures another variable: delegates made before the patch keep the old body
        public Func<int> MakeCounter(int start) { int c = start, step = 5; return () => c += step; }
        // lambda that captures only `this` (a method of Player itself), edited in place
        public Func<int, bool> BelowHealth() => s => s < health - 98;
        // lambda without captures (a method of the shared <>c class), edited in place
        public Func<int, int> Doubler() => x => x * 3;

        // local function of an iterator: it takes the iterator's state by reference, so an iterator
        // that is already running keeps calling the old one
        public IEnumerator Ticker(int n)
        {
            int Step(int k) => k + n * 10;
            int i = 0;
            while (i < 1000) { i = Step(i); yield return i; }
        }

        // switch over strings (a helper of <PrivateImplementationDetails>)
        public string Kind(string key)
        {
            switch (key)
            {
                case "a": return "alpha";
                case "b": return "beta";
                case "c": return "gamma";
                case "d": return "DELTA";
                case "e": return "epsilon";
                case "f": return "zeta";
                case "g": return "eta";
                case "h": return "theta";
                default: return "unknown";
            }
        }

        public object MakePair() => new { Name = Name, Value = health };
        // the anonymous type of the loaded assembly, reached from patched code
        public string ReadPair(object pair) { var p = Util.As(pair, new { Name = "", Value = 0 }); return p.Value + "@" + p.Name; }

        // exception filter
        public int Guarded(int x)
        {
            try { if (x < 0) throw new ArgumentException("neg"); return 100 / x; }
            catch (DivideByZeroException) { return -1; }
            catch (ArgumentException e) when (e.Message == "neg") { return -2; }
            finally { health += 2; }
        }

        public string Where() { try { Fail(); return "no"; } catch (Exception e) { return Util.Frame(e); } }

        private void Fail()
        {
            int marker = health;
            throw new InvalidOperationException("boom" + marker); // THROW-LINE
        }

        public int Stored(Inventory bag) { bag[0] = bag[0] + 2; return bag.Total + bag.Capacity; }
    }

    public class Spinner : MonoBehaviour
    {
        public int Ticks;
        // Update was removed: the engine keeps calling the loaded one, which must do nothing now
        private void LateUpdate() { Ticks += 20; }
    }
}
