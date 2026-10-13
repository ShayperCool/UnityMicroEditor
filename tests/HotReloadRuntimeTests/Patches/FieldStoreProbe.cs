// FieldStore.Probe: what a patch assembly does with the fields it added. A patch reaches the store
// only through these two generic accessors with a literal id, so the harness goes the same way.
using System;
using System.Threading;
using UnityMicroEditor.HotReload;

public sealed class Tracked
{
    public static int Finalized;
    public readonly object Owner;   // the value refers to its key: the entry must still be collected
    public Tracked(object owner) { Owner = owner; }
    ~Tracked() { Interlocked.Increment(ref Finalized); }
}

public static class Probe
{
    public static int Bonus(object owner) { return FieldStore.Instance<int>(owner, "Game|Game.Player|bonus"); }
    public static void SetBonus(object owner, int value) { FieldStore.Instance<int>(owner, "Game|Game.Player|bonus") = value; }
    public static int AddBonus(object owner, int value) { return FieldStore.Instance<int>(owner, "Game|Game.Player|bonus") += value; }

    // The field changed its type: same id, another slot.
    public static string BonusText(object owner) { return FieldStore.Instance<string>(owner, "Game|Game.Player|bonus"); }
    public static void SetBonusText(object owner, string value) { FieldStore.Instance<string>(owner, "Game|Game.Player|bonus") = value; }

    public static string Title(object owner) { return FieldStore.Instance<string>(owner, "Game|Game.Player|title"); }
    public static void SetTitle(object owner, string value) { FieldStore.Instance<string>(owner, "Game|Game.Player|title") = value; }
    public static Game.Wide Wide(object owner) { return FieldStore.Instance<Game.Wide>(owner, "Game|Game.Player|wide"); }
    public static void SetWideC(object owner, long value) { FieldStore.Instance<Game.Wide>(owner, "Game|Game.Player|wide").C = value; }

    public static int Any(object owner, string id) { return FieldStore.Instance<int>(owner, id); }
    public static void SetAny(object owner, string id, int value) { FieldStore.Instance<int>(owner, id) = value; }
    public static int Count(object owner, string id) { return Interlocked.Increment(ref FieldStore.Instance<int>(owner, id)); }

    public static int Total() { return FieldStore.Static<int>("Game|Game.Player|total"); }
    public static int AddTotal(int value) { return Interlocked.Add(ref FieldStore.Static<int>("Game|Game.Player|total"), value); }
    public static string Label() { return FieldStore.Static<string>("Game|Game.Player|label"); }
    public static void SetLabel(string value) { FieldStore.Static<string>("Game|Game.Player|label") = value; }
    public static long TotalAsLong() { return FieldStore.Static<long>("Game|Game.Player|total"); }

    public static void Track(object owner) { FieldStore.Instance<Tracked>(owner, "Game|Game.Player|tracked") = new Tracked(owner); }

    public static long Spin(object owner, int count)
    {
        long sum = 0;
        for (int index = 0; index < count; index++) sum += FieldStore.Instance<int>(owner, "Game|Game.Player|bonus");
        return sum;
    }
}
