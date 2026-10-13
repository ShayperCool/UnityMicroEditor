// Second source file of Game.dll as the Player loaded it: records, lambdas whose pairing must be
// proven, methods that become state machines, `readonly` fields and a null receiver.
using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;

namespace Game
{
    public record Figure
    {
        public int Side { get; set; } = 1;
        public virtual string Kind() => "figure";
    }

    public record Quad : Figure
    {
        public int Corners { get; set; } = 4;
        public override string Kind() => "quad";
    }

    public static class Figures
    {
        public static string Grow(Figure f)
        {
            var g = f with { Side = f.Side + 1 };
            return g.Kind() + ":" + g.GetType().Name + ":" + g.Side + ":" + (g is Quad q ? q.Corners : -1);
        }
    }

    public class Wiring
    {
        public static readonly List<string> Lines = new List<string>();
        public static void Emit(string s) => Lines.Add(s);

        public Func<Task<string>> Greeter(string name)
        {
            return async () =>
            {
                await Task.Yield();
                return "hello " + name;
            };
        }

        public Func<IEnumerable<int>> Numbers(int n)
        {
            IEnumerable<int> Run()
            {
                for (int i = 0; i < n; i++) yield return i;
            }
            return Run;
        }

        public void Twins(List<Action> first, List<Action> second)
        {
            first.Add(() => Emit("hit"));
            second.Add(() => Emit("hit"));
        }

        public void Swapped(List<Action> damage, List<Action> heal)
        {
            damage.Add(() => Emit("damage"));
            heal.Add(() => Emit("heal"));
        }

        public void Replaced(List<Action> damage, List<Action> heal, List<Action> death)
        {
            damage.Add(() => Emit("flash"));
            heal.Add(() => Emit("sparkle"));
        }

        public Func<int, int> Around(int k)
        {
            Emit("around");
            return x => x * k;
        }

        public IEnumerable<Func<int>> Makers()
        {
            yield return () => 1;
            yield return () => 2;
        }

        public Func<int, Func<int, int>> Nested(int a)
        {
            return b =>
            {
                int c = a + b;
                return d => a + c + d;
            };
        }
    }

    public class Voice
    {
        public virtual string Speak() => "voice";
        public virtual string Shout() => "VOICE";
    }

    public class Echo : Voice
    {
        private int own = 7;
        public override string Speak() => "echo";
        public override string Shout() => "ECHO";
        public Func<string> Caller(int k) => () => base.Speak() + "#" + k;

        public Func<int, int> Scaler()
        {
            static int Twice(int v) => v * 2;
            return x => Twice(x) + 1;
        }

        public IEnumerable<Func<int>> Counters(int start)
        {
            for (int i = 0; i < 2; i++)
            {
                int j = i + start;
                yield return () => j + own;
            }
        }
    }

    public class Shifting
    {
        public IEnumerable<int> ToIterator(int n) { return new[] { n }; }
        public Task<int> ToAsync(int n) { return Task.FromResult(n); }
        public IEnumerable<int> FromIterator(int n) { yield return n; yield return n + 1; }
        public async Task<int> FromAsync(int n) { await Task.Yield(); return n; }
    }

    public class Frozen
    {
        public readonly int size = 3;
        public readonly string label = "three";
        public readonly Vec at;
        public void Grow() { }
        public string Read() => size + label + at.X;

        public int depth = 4;
        public void Dig() { depth += 10; }
        public string Depth() => "depth" + depth;

        public static int cap = 5;
        public static void Raise() { cap += 10; }
        public static string Cap() => "cap" + cap;
    }

    public class Optional
    {
        public int uses;
        public static string Call(Optional x, int a)
        {
            try { return x.ToString().Length > 0 ? "old" + a : ""; }
            catch (NullReferenceException) { return "NRE"; }
        }
        public string Trace() => "none";
    }
}
