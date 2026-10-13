// The code around the game assemblies in the behaviour test: compiled once against the LOADED
// versions and never patched, like the engine and like assemblies that did not change. It runs
// the same script on the new compilation and on the loaded one with the patch applied.
//
// Labels that start with `existing.`, `stale.` or `running.` describe state from before the patch;
// there the patched Player legitimately differs from a fresh start, and the test names both values.
using System;
using System.Collections;
using System.Collections.Generic;
using System.Globalization;
using System.Linq;
using System.Reflection;
using System.Text;
using System.Threading.Tasks;
using Game;
using Game.Core;

public static class Driver
{
    static Player existing;
    static Func<int, int> adder;
    static Func<int> counter;
    static Func<int, bool> below;
    static Func<int, int> doubler;
    static IEnumerator ticker;
    static IEnumerator running;
    static Spinner spinner;
    static object pair;
    static Counter existingCounter;
    static Func<Task<string>> greeter;
    static Func<IEnumerable<int>> numbers;
    static readonly List<Action> twinFirst = new List<Action>(), twinSecond = new List<Action>(), swapDamage = new List<Action>(), swapHeal = new List<Action>(),
        wireDamage = new List<Action>(), wireHeal = new List<Action>(), wireDeath = new List<Action>();
    static Func<int, int> around, nestedInner;
    static List<Func<int>> makers;
    static Func<int, Func<int, int>> nested;
    static Frozen frozen;
    static IEnumerator<int> fromIterator;
    static Func<string> caller;
    static Func<int, int> scaler;
    static List<Func<int>> counters;
    static readonly StringBuilder log = new StringBuilder();

    static string Show(object value)
    {
        if (value == null) return "null";
        if (value is string text) return text;
        if (value is bool flag) return flag ? "true" : "false";
        if (value is float single) return single.ToString("R", CultureInfo.InvariantCulture);
        if (value is double number) return number.ToString("R", CultureInfo.InvariantCulture);
        if (value is int integer) return integer.ToString(CultureInfo.InvariantCulture);
        if (value is Task task) { task.Wait(); return Show(task.GetType().GetProperty("Result").GetValue(task, null)); }
        // An iterator object of an IEnumerable method is both; it only runs through GetEnumerator.
        if (value is IEnumerable sequence) return string.Join(",", sequence.Cast<object>().Select(Show));
        if (value is IEnumerator enumerator) { var items = new List<string>(); while (enumerator.MoveNext()) items.Add(Show(enumerator.Current)); return string.Join(",", items); }
        Type type = value.GetType();
        if (type.IsValueType) return "{" + string.Join(" ", type.GetFields(BindingFlags.Public | BindingFlags.Instance).Select(f => f.Name + "=" + Show(f.GetValue(value)))) + "}";
        return type.Name;
    }

    static void Line(string label, object value) => log.Append(label).Append('=').Append(Show(value)).Append('\n');

    static void Try(string label, Func<object> call)
    {
        try { Line(label, call()); }
        catch (Exception e) { Line(label, "THROWS " + e.GetType().Name + ": " + e.Message); }
    }

    /// What a list of subscribers does when it is raised.
    static string Emitted(IEnumerable<Action> handlers)
    {
        Wiring.Lines.Clear();
        foreach (Action handler in handlers) handler();
        return string.Join(",", Wiring.Lines);
    }

    /// What the engine does with a message: call it by name if the class has it.
    static void Message(object target, string name)
    {
        MethodInfo method = target.GetType().GetMethod(name, BindingFlags.Instance | BindingFlags.Public | BindingFlags.NonPublic);
        if (method != null) method.Invoke(target, null);
    }

    /// Runs before the patch is applied: the state a running game has.
    public static void Before()
    {
        existing = new Player();
        adder = existing.MakeAdder(5);
        counter = existing.MakeCounter(10);
        below = existing.BelowHealth();
        doubler = existing.Doubler();
        ticker = existing.Ticker(2);
        ticker.MoveNext();
        running = existing.Countdown(3);
        running.MoveNext();
        spinner = new Spinner();
        pair = new Player().MakePair();
        existingCounter = new Counter();
        existingCounter.Next();

        var wiring = new Wiring();
        greeter = wiring.Greeter("old");
        numbers = wiring.Numbers(3);
        wiring.Twins(twinFirst, twinSecond);
        wiring.Swapped(swapDamage, swapHeal);
        wiring.Replaced(wireDamage, wireHeal, wireDeath);
        around = wiring.Around(3);
        makers = wiring.Makers().ToList();
        nested = wiring.Nested(1);
        nestedInner = nested(2);
        var echo = new Echo();
        caller = echo.Caller(5);
        scaler = echo.Scaler();
        counters = echo.Counters(10).ToList();
        frozen = new Frozen();
        fromIterator = new Shifting().FromIterator(5).GetEnumerator();
        fromIterator.MoveNext();
    }

    public static string After()
    {
        log.Length = 0;
        var p = new Player();
        Try("Damage", () => p.Damage(10));
        Try("Damage again", () => p.Damage(10));
        Try("Report", () => p.Report());
        Try("Turn", () => p.Turn());
        Try("Mixed", () => p.Mixed(3));
        Try("Named", () => p.Named());
        Try("Above", () => p.Above(2));
        Try("Best", () => p.Best());
        Try("CountBelowHealth", () => p.CountBelowHealth());
        Try("CountNear", () => p.CountNear(1));
        Try("Countdown", () => p.Countdown(3));
        Try("Evens", () => p.Evens(6));
        Try("LoadAsync", () => p.LoadAsync(1));
        Try("Describe", () => p.Describe());
        Try("Describe through base", () => ((Entity)p).Describe());
        Try("Entity.Describe", () => new Entity().Describe());
        Try("Count", () => Player.Count());
        Try("Count again", () => Player.Count());
        Try("Table", () => p.Table(3));
        Try("MakeBig", () => p.MakeBig(2));
        Try("Corner", () => p.Corner(4));
        Try("StaticBig", () => Player.StaticBig(8));
        Try("Health", () => p.Health);
        Try("Speed", () => p.Speed);
        Try("Speed set", () => { p.Speed = 3; return p.Speed; });
        Try("Score", () => ((IScored)p).Score());
        Try("Dispose", () => { ((IDisposable)p).Dispose(); return p.Disposed; });
        Try("Kind d", () => p.Kind("d"));
        Try("Kind zz", () => p.Kind("zz"));
        Try("Guarded 5", () => p.Guarded(5));
        Try("Guarded 0", () => p.Guarded(0));
        Try("Guarded -1", () => p.Guarded(-1));
        Try("Health after Guarded", () => p.Health);
        Try("Where", () => p.Where());
        Try("Named player", () => new Player("zed").Report());
        Try("Adder made after", () => p.MakeAdder(5)(1));
        Try("Counter made after", () => { var c = p.MakeCounter(10); c(); return c(); });
        Try("Below made after", () => p.BelowHealth()(3));
        Try("Doubler made after", () => p.Doubler()(5));

        // An anonymous object made by loaded code, read by patched code.
        Try("ReadPair", () => p.ReadPair(pair));
        Try("ReadPair own", () => p.ReadPair(p.MakePair()));

        Try("Vec.Length", () => new Vec(3, 4).Length());
        Try("Vec.Scaled", () => new Vec(3, 4).Scaled(2));
        Try("Vec.One", () => Vec.One());
        Try("Vec.Text", () => new Vec(3, 4).Text());
        Try("Vec.Total", () => new Vec(3, 4).Total());
        Try("Vec3.Plus", () => new Vec3(1, 2, 3).Plus(new Vec3(10, 20, 30)));
        Try("Big.Twice", () => { Big b; b.A = 1.5; b.B = 2.5; b.C = 3.5; b.D = 4.5; return b.Twice(); });
        Try("Big.Sum", () => { Big b; b.A = 1; b.B = 2; b.C = 3; b.D = 4; return b.Twice().Sum(); });

        var bag = new Inventory();
        var slot = bag.Add("a", 2);
        bag.Add("b", 3);
        Try("Indexer", () => { bag[1] = 7; return bag[1]; });
        Try("Operator +", () => { bag += 4; return bag.Total; });
        Try("Conversion", () => (int)bag);
        Try("Slot.Show", () => slot.Show());
        Try("Stored", () => p.Stored(bag));

        Try("Rules.Clamp", () => Rules.Clamp(-5));
        Try("Rules.Scale", () => Rules.Scale(4));
        Try("Counter", () => { var c = new Counter(); c.Next(); return c.Next(); });

        // Delegates created before the patch from lambdas that were edited in place: one over a
        // closure class, one over a method of Player, one over the shared <>c class.
        Try("Adder made before", () => adder(1));
        Try("Below made before", () => below(3));
        Try("Doubler made before", () => doubler(5));
        // The engine keeps sending Update to an object whose class no longer declares it.
        Try("Spinner", () => { Message(spinner, "Update"); Message(spinner, "LateUpdate"); return spinner.Ticks; });

        Try("Figure.Grow", () => Figures.Grow(new Figure()));
        // A derived record behind a base reference stays what it is.
        Try("Quad.Grow", () => Figures.Grow(new Quad()));

        var wiring = new Wiring();
        Try("Greeter", () => wiring.Greeter("new")());
        Try("Numbers", () => wiring.Numbers(3)());
        Try("Around", () => wiring.Around(3)(5));
        Try("Makers", () => string.Join(",", wiring.Makers().Select(make => make())));
        Try("Nested", () => wiring.Nested(1)(2)(3));
        // Delegates from before the patch whose lambda has a proven new version: an async lambda,
        // an iterator local function, the second of two identical lambdas, two lambdas whose
        // bodies were exchanged, lambdas an iterator made, a lambda that creates another one.
        Try("Greeter made before", () => greeter());
        Try("Numbers made before", () => numbers());
        Try("Twins made before", () => Emitted(twinFirst.Concat(twinSecond)));
        Try("Swapped made before", () => Emitted(swapDamage) + "|" + Emitted(swapHeal));
        Try("Around made before", () => around(5));
        Try("Makers made before", () => string.Join(",", makers.Select(make => make())));
        Try("Nested made before", () => nested(2)(3));
        // ... a lambda whose base call helper changed, a lambda whose local function changed, and
        // lambdas over a closure that a state machine made.
        Try("Caller made before", () => caller());
        Try("Scaler made before", () => scaler(3));
        Try("Counters made before", () => string.Join(",", counters.Select(count => count())));

        var shifting = new Shifting();
        Try("ToIterator", () => shifting.ToIterator(2));
        Try("ToAsync", () => shifting.ToAsync(2));
        Try("FromIterator", () => shifting.FromIterator(2));
        Try("FromAsync", () => shifting.FromAsync(2));

        // Fields that lost or gained `readonly`, in an object from before the patch and in a new one.
        Try("Frozen.Grow before", () => { frozen.Grow(); frozen.Grow(); return frozen.Read(); });
        Try("Frozen.Grow", () => { var f = new Frozen(); f.Grow(); return f.Read(); });
        Try("Frozen.Dig", () => { var f = new Frozen(); f.Dig(); return f.Depth(); });
        Try("Frozen.Cap", () => { Frozen.Raise(); return Frozen.Cap(); });

        Try("Optional.Call", () => Optional.Call(new Optional(), 1));
        Try("Optional.Call null", () => Optional.Call(null, 1));
        Try("Optional.Trace", () => new Optional().Trace());

        Try("existing.Damage", () => existing.Damage(10));
        Try("existing.Speed", () => existing.Speed);
        Try("existing.Counter", () => existingCounter.Next());
        Try("stale.Counter", () => counter());
        Try("running.Countdown", () => running);
        Try("running.Ticker", () => { ticker.MoveNext(); object first = ticker.Current; ticker.MoveNext(); return first + "," + ticker.Current; });
        Try("running.FromIterator", () => fromIterator.MoveNext() ? fromIterator.Current.ToString() : "end");
        Try("existing.Nested inner", () => nestedInner(3));
        Try("stale.Replaced.heal", () => Emitted(wireHeal));
        Try("stale.Replaced.death", () => Emitted(wireDeath));
        return log.ToString();
    }
}
