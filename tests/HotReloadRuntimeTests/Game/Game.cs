// The "game": the assembly the Player loaded at its start. Compiled twice by run.sh, once as it is
// (the loaded original) and once with every non-public member made public, which is the reference
// the hand-written patches compile against. Keep those three access keywords out of strings and
// identifiers: the reference is made by replacing the words.
using System;
using System.Collections.Generic;
using UnityEngine;
using UnityMicroEditor;

namespace Game
{
    public struct Wide { public long A, B, C, D; }

    public class Player
    {
        private int health = 10;
        private static int created;
        public string Name;

        public Player() { created++; Name = "ctor0"; }

        // Seven bytes of IL: a JIT that inlines copies these two into their callers.
        public int Health() { return health; }
        public int Level { get { return 1; } }

        public string Describe(string prefix, int times) { return "orig:" + prefix + times + ":" + health; }
        private int Secret(int add) { return health + add; }
        public static int Twice(int value) { return value * 2; }
        public static int Created { get { return created; } }

        // A class receiver with a result returned through a hidden buffer.
        public Wide Stats(long seed) { return new Wide { A = seed, B = health, C = 3, D = 4 }; }

#if REFERENCE
        // Only in the reference the patches compile against: a patch built for another build of the game.
        public int absent;
        public int Absent() { return 0; }
#endif
    }

    public struct Vec3
    {
        private float x, y, z;

        public Vec3(float x, float y, float z) { this.x = x; this.y = y; this.z = z; }

        // Struct receivers. Twelve and 32 bytes come back through a hidden buffer, a float in a register.
        public Vec3 Scaled(float factor) { return new Vec3(x * factor, y * factor, z * factor); }
        public Wide Spread(long seed) { return new Wide { A = seed, B = (long)x, C = (long)y, D = (long)z }; }
        public float Sum() { return x + y + z; }
        public void Shift(float by) { x += by; }
        public override string ToString() { return x + "," + y + "," + z; }
    }

    public class Animal
    {
        public virtual string Speak() { return "animal"; }
        public string Twice() { return Speak() + Speak(); }
    }

    public class Dog : Animal
    {
        public override string Speak() { return "dog"; }
    }

    public class Puppy : Dog
    {
        public override string Speak() { return "puppy+" + base.Speak(); }
    }

    public interface IGreeter { string Greet(string name); }

    public class Greeter : IGreeter
    {
        public string Greet(string name) { return "hello " + name; }
    }

    public class Formal : IGreeter
    {
        string IGreeter.Greet(string name) { return "good day " + name; }
    }

    public struct Terse : IGreeter
    {
        public int Count;
        public string Greet(string name) { return "hi " + name + Count; }
    }

    public static class Hot
    {
        // Called from other threads while it is patched, patched again and restored.
        public static long Work(long value)
        {
            long result = value * 3;
            return result + 1;
        }

        // A try block makes the compiled body start with a one-byte instruction (push rbp), which only
        // a redirect through an island may replace under other threads.
        public static long Guarded(long value)
        {
            try { return value * 5; }
            finally { Leave(); }
        }

        [System.Runtime.CompilerServices.MethodImpl(System.Runtime.CompilerServices.MethodImplOptions.NoInlining)]
        public static void Leave() { }

        // Loads a constant first: sub rsp, imm8; mov eax, imm32. The island of its first byte would
        // lie below address zero, so it is left through its second instruction.
        public static long Flat(long value) { return 22; }

        // More than 127 bytes of locals make the body start with sub rsp, imm32: seven bytes, the
        // one first instruction a whole jump fits into.
        public static long Large(long value)
        {
            Wide a = new Wide { A = value }, b = new Wide { B = value }, c = new Wide { C = value }, d = new Wide { D = 1 }, e = new Wide { A = 0 };
            return a.A + b.B + c.C + d.D + e.A;
        }
    }

    public static class Framed
    {
        // An argument on the stack makes the JIT keep a frame pointer, as a try block does: the
        // compiled bodies of these two start with the one-byte push rbp.
        public static int Seven(int a, int b, int c, int d, int e, int f, int g) { return a + g; }
        public static int CallsSeven() { return Seven(1, 2, 3, 4, 5, 6, 7); }
    }

    // What the game sets when it has reached the state the initializer of Lazy needs.
    public static class Ready
    {
        public static bool Now;
        public static int Lazy, Eager;
    }

    // Compiling a method runs the pending static initializer of its type. This one fails until
    // the game is ready, and a failed initializer is final.
    public class Lazy
    {
        private static readonly int seed = Init();
        private static int Init() { Ready.Lazy++; if (!Ready.Now) throw new InvalidOperationException("the game is not ready"); return 5; }
        public static int Value() { return seed; }
    }

    public class Eager
    {
        private static readonly int seed = Init();
        private static int Init() { Ready.Eager++; return 7; }
        public static int Value() { return seed; }
    }

    public class Box<T>
    {
        public T Value;
        public T Get() { return Value; }
    }

    public static class Generic
    {
        public static T Echo<T>(T value) { return value; }
    }

    public class Ticker : MonoBehaviour
    {
        public int Refreshed, Ticks, Updates;

        [InvokeOnHotReload]
        private void Refresh() { Refreshed++; }

        [InvokeOnHotReloadLocal]
        public void Tick() { Ticks++; }

        public void Update() { Updates++; }
    }

    public class Plain
    {
        // No way to find live instances of a plain class: skipped and counted.
        [InvokeOnHotReload]
        private void Refresh() { Reloads.Unreachable++; }
    }

    public static class Reloads
    {
        public static int Simple, Listed, Foreign, Named, Thrown, Unreachable, Local;
        public static List<MethodPatch> Last;

        [InvokeOnHotReload]
        private static void OnSimple() { Simple++; }

        [InvokeOnHotReload]
        public static void OnListed(List<MethodPatch> patches) { Listed++; Last = patches; }

        [Product.InvokeOnHotReload]
        private static void OnForeign() { Foreign++; }

        [Product.OnHotReload]
        private static void OnNamed() { Named++; }

        [InvokeOnHotReload]
        private static void OnThrowing() { Thrown++; throw new InvalidOperationException("callback failure"); }

        [InvokeOnHotReload]
        private static void OnWrong(int value) { Unreachable++; }

        [InvokeOnHotReloadLocal]
        public static string LocalText() { Local++; return "local0"; }
    }
}

// Attributes of another tool: matched by simple name, whatever their namespace.
namespace Product
{
    public class InvokeOnHotReload : Attribute { }
    public class OnHotReloadAttribute : Attribute { }
}
