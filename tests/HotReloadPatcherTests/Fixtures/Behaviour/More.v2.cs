// Second source file of Game.dll after the edit. Every member here is one a patch carries.
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
        // `with` on a base reference: the clone method of the loaded record, called virtually
        public static string Grow(Figure f)
        {
            var g = f with { Side = f.Side + 10 };
            return g.Kind() + ":" + g.GetType().Name + ":" + g.Side + ":" + (g is Quad q ? q.Corners : -1);
        }
    }

    public class Wiring
    {
        public static readonly List<string> Lines = new List<string>();
        public static void Emit(string s) => Lines.Add(s);

        // async lambda: the edit is in its state machine, the stub that starts it is redirected
        public Func<Task<string>> Greeter(string name)
        {
            return async () =>
            {
                await Task.Yield();
                return "hello again " + name;
            };
        }

        // local function that is an iterator
        public Func<IEnumerable<int>> Numbers(int n)
        {
            IEnumerable<int> Run()
            {
                for (int i = 0; i < n; i++) yield return i * 10;
            }
            return Run;
        }

        // two lambdas with the same body, the second one edited
        public void Twins(List<Action> first, List<Action> second)
        {
            first.Add(() => Emit("hit"));
            second.Add(() => Emit("healed"));
        }

        // the bodies of two lambdas exchanged, the code around them as it was
        public void Swapped(List<Action> damage, List<Action> heal)
        {
            damage.Add(() => Emit("heal"));
            heal.Add(() => Emit("damage"));
        }

        // a lambda replaced by one for another list: the old subscription has no partner
        public void Replaced(List<Action> damage, List<Action> heal, List<Action> death)
        {
            damage.Add(() => Emit("flash"));
            death.Add(() => Emit("explode"));
        }

        // the method changes around a lambda that stays
        public Func<int, int> Around(int k)
        {
            Emit("around, edited");
            return x => x * k;
        }

        // lambdas that an iterator creates
        public IEnumerable<Func<int>> Makers()
        {
            yield return () => 10;
            yield return () => 20;
        }

        // an edited lambda that creates another one over its own closure
        public Func<int, Func<int, int>> Nested(int a)
        {
            return b =>
            {
                int c = a + b + 5000;
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
        // a lambda of a closure class reaches the base class through a helper method, which changed
        public Func<string> Caller(int k) => () => base.Shout() + "#" + k;

        // a lambda calls a local function; only the local function is edited
        public Func<int, int> Scaler()
        {
            static int Twice(int v) => v * 20;
            return x => Twice(x) + 1;
        }

        // an iterator whose lambdas capture its locals: their closure is made by the state machine
        public IEnumerable<Func<int>> Counters(int start)
        {
            for (int i = 0; i < 2; i++)
            {
                int j = i + start;
                yield return () => j * 100 + own;
            }
        }
    }

    public class Shifting
    {
        // a plain method becomes an iterator
        public IEnumerable<int> ToIterator(int n) { yield return n * 2; yield return n * 3; }
        // a plain method becomes an async method
        public async Task<int> ToAsync(int n) { await Task.Yield(); return n * 2; }
        // an iterator becomes a plain method
        public IEnumerable<int> FromIterator(int n) { return new[] { n * 5 }; }
        // an async method becomes a plain method
        public Task<int> FromAsync(int n) { return Task.FromResult(n * 7); }
    }

    public class Frozen
    {
        // `readonly` removed from instance fields, which a method now writes
        public int size = 3;
        public string label = "three";
        public Vec at;
        public void Grow() { size += 10; label += "+"; at.X += 2; }
        public string Read() => size + label + at.X;

        // `readonly` added to an instance field
        public readonly int depth = 4;
        public void Dig() { }
        public string Depth() => "depth" + depth;

        // `readonly` added to a static field
        public static readonly int cap = 5;
        public static void Raise() { }
        public static string Cap() => "cap" + cap;
    }

    public class Optional
    {
        public int uses;
        // added instance methods: a null receiver throws at the call, as `callvirt` does
        private string Fresh(int a) => "fresh" + a;
        private string Counted(int a)
        {
            try { if (a < 0) throw new ArgumentException("negative"); return "counted" + a; }
            catch (ArgumentException e) when (e.Message == "negative") { return "caught"; }
            finally { uses++; }
        }
        private string Thrower()
        {
            int marker = uses;
            throw new InvalidOperationException("added" + marker); // THROW-LINE-ADDED
        }
        public static string Call(Optional x, int a)
        {
            try { return x.Fresh(a) + x.Counted(a) + x.Counted(-a) + x.uses; }
            catch (NullReferenceException) { return "NRE"; }
        }
        public string Trace() { try { return Thrower(); } catch (Exception e) { return Util.Frame(e); } }
    }
}
