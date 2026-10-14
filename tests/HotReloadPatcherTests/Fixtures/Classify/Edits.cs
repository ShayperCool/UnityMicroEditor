// Fixture of the classification tests. Compiled once without a symbol (what the Player loaded)
// and once per CASE_* symbol (the edit under test). Every case touches its own small type, and all
// of them share one assembly, so an edit also meets the renumbering it causes elsewhere.
using System;
using System.Collections;
using System.Collections.Generic;
using System.Linq;
using System.Linq.Expressions;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using System.Threading.Tasks;
using UnityEngine;

#if CASE_ASSEMBLY_ATTRIBUTE
[assembly: InternalsVisibleTo("Somebody")]
#endif
#if CASE_ASSEMBLY_VERSION
[assembly: System.Reflection.AssemblyVersion("2.0.0.0")]
#endif

// The patch of generation 7 names its holder of Edits.Bodies exactly like this type: the
// verification must refuse to publish it.
namespace HotReload.G7.Edits
{
    public class Bodies { }
}

namespace Edits
{
    public interface IMarker { }
    public interface IWork { int Work(); }

    // ---- body edits ----------------------------------------------------------------------

    public class Bodies : IWork, IDisposable
    {
        private int value = 1;
        private Action changed;

        public int Instance()
        {
#if CASE_BODY
            return value + 2;
#else
            return value + 1;
#endif
        }

        public static int Static(int x)
        {
#if CASE_BODY_STATIC
            return x * 3;
#else
            return x * 2;
#endif
        }

#if CASE_BODY_ACCESSOR
        public int Accessed { get { return value * 10; } set { this.value = value / 10; } }
        public int this[int i] { get { return value - i; } }
        public event Action Changed { add { changed = value; } remove { changed = null; } }
#else
        public int Accessed { get { return value; } set { this.value = value; } }
        public int this[int i] { get { return value + i; } }
        public event Action Changed { add { changed += value; } remove { changed -= value; } }
#endif

#if CASE_BODY_OPERATOR
        public static Bodies operator +(Bodies a, int n) { a.value += n * 2; return a; }
        public static implicit operator int(Bodies b) => b.value + 1;
#else
        public static Bodies operator +(Bodies a, int n) { a.value += n; return a; }
        public static implicit operator int(Bodies b) => b.value;
#endif

#if CASE_BODY_EXPLICIT_INTERFACE
        int IWork.Work() => value * 2;
        void IDisposable.Dispose() { value = -1; }
#else
        int IWork.Work() => value;
        void IDisposable.Dispose() { value = 0; }
#endif

#if CASE_BODY_FINALIZER
        ~Bodies() { value = -2; }
#else
        ~Bodies() { value = 0; }
#endif
    }

    public class Constructed
    {
#if CASE_BODY_CTOR
        private int a = 2;
        public Constructed() { a += 2; }
#else
        private int a = 1;
        public Constructed() { a++; }
#endif
        public Constructed(int x) { a = x; }
        public int A => a;
    }

    public class Animal
    {
#if CASE_BODY_VIRTUAL
        public virtual string Speak() => "....";
#else
        public virtual string Speak() => "...";
#endif
    }

    public class Dog : Animal
    {
#if CASE_BODY_VIRTUAL
        public override string Speak() => "WOOF" + base.Speak();
#else
        public override string Speak() => "woof" + base.Speak();
#endif
    }

    public struct Point
    {
        public float X, Y;
#if CASE_BODY_STRUCT
        public float Len() => X * Y;
        public Point Flip() => new Point { X = -Y, Y = -X };
#else
        public float Len() => X + Y;
        public Point Flip() => new Point { X = Y, Y = X };
#endif
    }

    public class Lambdas
    {
        private readonly List<int> items = new List<int> { 1, 2, 3 };
        private int factor = 2;

#if CASE_INSERT_MEMBER
        // Declared first: shifts the ordinal in the name of every generated member below.
        public int Inserted() => items.Count(i => i > 0);
#endif

#if CASE_BODY_LAMBDA
        public Func<int, int> Capturing(int n) => x => x + n + 1;
        public Func<int, int> Static() => x => x * 3;
        public Func<int, int> This() => x => x * factor + 1;
#else
        public Func<int, int> Capturing(int n) => x => x + n;
        public Func<int, int> Static() => x => x * 2;
        public Func<int, int> This() => x => x * factor;
#endif

        public Func<int, int> Recaptured(int n)
        {
#if CASE_LAMBDA_NOT_REMAPPED
            int m = n * 2;
            return x => x + n + m;
#else
            return x => x + n;
#endif
        }

        // The lambda lives in the closure of the loop body, which holds the closure of the method.
        public Func<int, int> Nested(int a)
        {
#if CASE_LAMBDA_OUTER_CAPTURE
            int extra = a * 3;
#endif
            Func<int, int> result = null;
            for (int i = 0; i < 1; i++)
            {
                int b = i;
#if CASE_LAMBDA_OUTER_CAPTURE
                result = x => x + a + b + extra;
#elif CASE_LAMBDA_NESTED
                result = x => x + a + b + 1;
#else
                result = x => x + a + b;
#endif
            }
            return result;
        }

        public int Local(int v)
        {
#if CASE_BODY_LOCAL_FUNCTION
            int Twice(int k) => k * 3;
#else
            int Twice(int k) => k * 2;
#endif
            return Twice(v);
        }

        public int LocalCapture(int v)
        {
#if CASE_BODY_LOCAL_CAPTURE
            int Add(int k) => k + v + 1;
#else
            int Add(int k) => k + v;
#endif
            return Add(1);
        }

        public IEnumerator Iterate()
        {
            yield return 1;
#if CASE_BODY_ITERATOR
            yield return 2;
#endif
        }

        public async Task<int> Wait()
        {
            await Task.Yield();
#if CASE_BODY_ASYNC
            return 2;
#else
            return 1;
#endif
        }

        // Never edited: their generated members are renumbered by the cases above.
        public int Unchanged() => items.Where(i => i > 1).Select(i => i * factor).Sum();
        public IEnumerable<int> UnchangedIterator() { foreach (var i in items) yield return i; }
        public async Task<int> UnchangedAsync() { await Task.Yield(); return items.Count; }
    }

    // Which old lambda became which new one is only known when the code that creates them is the
    // same in both compilations.
    public class Wiring
    {
        public static readonly List<string> Lines = new List<string>();
        public static void Emit(string s) => Lines.Add(s);

        // Two lambdas with the same body, the second one edited.
        public void Twins(List<Action> first, List<Action> second)
        {
            first.Add(() => Emit("hit"));
#if CASE_LAMBDA_TWIN
            second.Add(() => Emit("healed"));
#else
            second.Add(() => Emit("hit"));
#endif
        }

        // The bodies of two lambdas are exchanged; the code around them stays.
        public void Swapped(List<Action> damage, List<Action> heal)
        {
#if CASE_LAMBDA_BODIES_SWAPPED
            damage.Add(() => Emit("heal"));
            heal.Add(() => Emit("damage"));
#else
            damage.Add(() => Emit("damage"));
            heal.Add(() => Emit("heal"));
#endif
        }

        // A lambda is replaced by another one of the same shape at the same position, for another list.
        public void Replaced(List<Action> damage, List<Action> heal, List<Action> death)
        {
            damage.Add(() => Emit("flash"));
#if CASE_LAMBDA_REPLACED
            death.Add(() => Emit("explode"));
#else
            heal.Add(() => Emit("sparkle"));
#endif
        }

        // The method changes around a lambda that stays as it is.
        public Func<int, int> Around(int k)
        {
#if CASE_LAMBDA_AROUND
            Emit("around, edited");
#else
            Emit("around");
#endif
            return x => x * k;
        }

        // A lambda is copied to a second list and the original edited: the old body is still there,
        // as another lambda.
        public void Copied(List<Action> first, List<Action> second)
        {
#if CASE_LAMBDA_COPIED
            first.Add(() => Emit("open, edited"));
            second.Add(() => Emit("open"));
#else
            first.Add(() => Emit("open"));
#endif
        }

        // A second lambda is added behind the first one, which stays.
        public void Appended(List<Action> first, List<Action> second)
        {
            first.Add(() => Emit("first"));
#if CASE_LAMBDA_APPENDED
            second.Add(() => Emit("second"));
#endif
        }

        // An async lambda is a stub; its code is in a state machine.
        public Func<Task<string>> Greeter(string name)
        {
            return async () =>
            {
                await Task.Yield();
#if CASE_LAMBDA_ASYNC
                return "hello again " + name;
#else
                return "hello " + name;
#endif
            };
        }

        // The same for a local function that is an iterator.
        public Func<IEnumerable<int>> Numbers(int n)
        {
            IEnumerable<int> Run()
            {
#if CASE_LAMBDA_ITERATOR_LOCAL
                for (int i = 0; i < n; i++) yield return i * 10;
#else
                for (int i = 0; i < n; i++) yield return i;
#endif
            }
            return Run;
        }

        // A lambda that an iterator creates: it is paired through the state machine's MoveNext.
        public IEnumerable<Func<int>> Makers()
        {
#if CASE_LAMBDA_IN_ITERATOR
            yield return () => 10;
#else
            yield return () => 1;
#endif
            yield return () => 2;
        }
    }

    // ---- records -------------------------------------------------------------------------

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
        // `with` calls the virtual clone method of the record, which the compiler names <Clone>$.
        public static Figure Grow(Figure f)
        {
#if CASE_RECORD_WITH
            return f with { Side = f.Side + 10 };
#else
            return f with { Side = f.Side + 1 };
#endif
        }
    }

    // ---- added, removed and renamed members ----------------------------------------------

    public class AddMethod
    {
        private int total = 1;
#if CASE_ADD_METHOD
        private int Helper(int x) => x + total;
        private static int StaticHelper(int x) => x * 2;
        public int Use() => Helper(1) + StaticHelper(2);
#else
        public int Use() => total;
#endif
    }

    public class Members
    {
        private int v = 1;
#if CASE_ADD_MEMBERS
        public int Prop { get; set; }
        public event Action Evt;
        public int this[int i] => v + i;
        public static Members operator -(Members m) => m;
        public int Use() { Evt?.Invoke(); return Prop + this[1]; }
#else
        public int Use() => v;
#endif
    }

    public class Signatures
    {
#if CASE_SIGNATURE_CHANGE
        private int Heal(int amount, bool clamp) => clamp ? amount : -amount;
        public int Turn() => Heal(5, true);
#elif CASE_RENAME_METHOD
        private int Mend(int amount) => amount;
        public int Turn() => Mend(5);
#elif CASE_STATIC_TOGGLE
        private static int Heal(int amount) => amount;
        public int Turn() => Heal(5);
#else
        private int Heal(int amount) => amount;
        public int Turn() => Heal(5);
#endif
    }

    public class Removing
    {
        public int Keep() => 1;
#if !CASE_REMOVE_METHOD
        public int Gone() => 2;
#endif
    }

    public class Tools
    {
        public string Name() => "Launch";
#if CASE_ADD_ATTRIBUTED_METHOD
        [Obsolete("later")]
        public void Launch() { }
#endif
    }

    public class Locks
    {
#if CASE_ADD_SYNCHRONIZED
        [MethodImpl(MethodImplOptions.Synchronized)]
        public void Locked() { }
#endif
    }

    public class Variadic
    {
        public int Count(__arglist)
        {
#if CASE_VARARGS_BODY
            return new ArgIterator(__arglist).GetRemainingCount() + 1;
#else
            return new ArgIterator(__arglist).GetRemainingCount();
#endif
        }

        public int Use() =>
#if CASE_ARGLIST_CALLER
            Count(__arglist(1, 2, 3));
#else
            Count(__arglist(1, 2));
#endif

        // Without extra arguments the call names the method itself.
        public int UseNone(int x) =>
#if CASE_ARGLIST_CALLER
            Count(__arglist()) + x + 1;
#else
            Count(__arglist()) + x;
#endif
    }

    public class Params
    {
#if CASE_PARAMETER_RENAME
        public int Sum(int x, int y) => x + y;
#else
        public int Sum(int a, int b) => a + b;
#endif
    }

    public class Hints
    {
#if CASE_INLINING_HINT
        [MethodImpl(MethodImplOptions.NoInlining)]
#endif
        public int M() => 1;
    }

    public class Access
    {
#if CASE_ACCESSIBILITY
        public int field;
        public int Method() => field;
#else
        private int field;
        private int Method() => field;
#endif
        public int Use() => Method();
    }

    public class Exposure : MonoBehaviour
    {
#if CASE_ACCESSIBILITY_SERIALIZED
        public int tuned;
        private int shown;
#else
        private int tuned;
        public int shown;
#endif
#if CASE_ACCESSIBILITY_NOT_SERIALIZED
        public static int shared;
        public readonly int frozen = 1;
        [SerializeField] public int marked;
#else
        private static int shared;
        private readonly int frozen = 1;
        [SerializeField] private int marked;
#endif
        public int Sum() => tuned + shown + shared + frozen + marked;
    }

    [Serializable]
    public class ExposureData
    {
#if CASE_ACCESSIBILITY_SERIALIZED
        public int value;
#else
        private int value;
#endif
        public int Get() => value;
    }

    public class Starting : MonoBehaviour
    {
        private int n;
#if CASE_START_BODY
        private void Start() { n = 2; }
#else
        private void Start() { n = 1; }
#endif
    }

    public class PlainStarting
    {
        private int n;
#if CASE_START_BODY
        public void Start() { n = 2; }
#else
        public void Start() { n = 1; }
#endif
    }

    public class Reordered
    {
#if CASE_REORDER
        public int Second() => items.Count(i => i > 1);
        private int b;
        public IEnumerable<int> Third() { yield return a; }
        private int a;
        public int First() => items.Where(i => i > 0).Count() + b;
        private readonly List<int> items = new List<int>();
#else
        private readonly List<int> items = new List<int>();
        private int a;
        private int b;
        public int First() => items.Where(i => i > 0).Count() + b;
        public int Second() => items.Count(i => i > 1);
        public IEnumerable<int> Third() { yield return a; }
#endif
    }

    // ---- fields --------------------------------------------------------------------------

    public class AddField
    {
        private int total = 1;
#if CASE_ADD_INSTANCE_FIELD
        private int extra;
        private int primed = 5;
        public int Use() => total + extra + primed;
#elif CASE_ADD_STATIC_FIELD
        private static int shared;
        public int Use() => total + shared++;
#else
        public int Use() => total;
#endif
    }

    public class Fields
    {
#if CASE_FIELD_RENAME
        private int renamed;
        public int Use() => renamed + (int)speed;
#else
        private int kept;
        public int Use() => kept + (int)speed;
#endif
#if CASE_FIELD_TYPE_CHANGE
        private double speed = 1.0;
#else
        private float speed = 1f;
#endif
#if !CASE_REMOVE_FIELD
        private int doomed;
#endif
    }

    public class Consts
    {
#if CASE_CHANGE_CONST
        private const int Base = 11;
#else
        private const int Base = 10;
#endif
#if CASE_ADD_CONST
        private const int Extra = 1;
#endif
        public int Use() => Base;
    }

    public class Special
    {
#if CASE_ADD_SPECIAL_FIELD
        [ThreadStatic] private static int perThread;
        private volatile int flag;
        public int Use() => perThread + flag;
#else
        public int Use() => 0;
#endif
    }

    public class Frozen
    {
#if CASE_STATIC_READONLY_REMOVED
        public static int limit = 5;
        public static void Raise() { limit += 10; }
#else
        public static readonly int limit = 5;
        public static void Raise() { }
#endif
        public static int Limit => limit;
    }

    // `readonly` added to a static field, and added to or removed from an instance field.
    public class Thawed
    {
#if CASE_READONLY_BENIGN
        public static readonly int cap = 5;
        public static void Raise() { }
        public int size = 3;
        public void Grow() { size += 10; }
        public readonly int depth = 4;
        public void Dig() { }
#else
        public static int cap = 5;
        public static void Raise() { cap += 10; }
        public readonly int size = 3;
        public void Grow() { }
        public int depth = 4;
        public void Dig() { depth += 10; }
#endif
    }

    [Serializable]
    public class Data
    {
#if CASE_FIELD_NONSERIALIZED
        [NonSerialized]
#endif
        public int cache;
    }

    // ---- generics ------------------------------------------------------------------------

    public class Box<T>
    {
        private T item;
#if CASE_GENERIC_TYPE_BODY
        public T Get() => default(T);
#else
        public T Get() => item;
#endif
#if CASE_GENERIC_TYPE_ADD
        public int counter;
        public int Count() => 1;
#endif
    }

    public class BoxUser
    {
#if CASE_GENERIC_TYPE_ADD
        public int Use(Box<int> box) => box.Count() + box.counter;
#else
        public int Use(Box<int> box) => 0;
#endif
    }

    public class Generics
    {
#if CASE_GENERIC_METHOD_BODY
        public T Echo<T>(T v) => default(T);
#else
        public T Echo<T>(T v) => v;
#endif
#if CASE_GENERIC_METHOD_ADD
        public T Make<T>() where T : new() => new T();
#endif
    }

    public class LocalGeneric
    {
        public int Run(int v)
        {
#if CASE_GENERIC_LOCAL_FUNCTION
            T Same<T>(T x) => x;
            return Same(v);
#else
            return v;
#endif
        }

        // The lambda becomes a generic method beside the local function.
        public int Deep(int v)
        {
            int Twice<T>(T x)
            {
#if CASE_GENERIC_LOCAL_LAMBDA
                Func<int> f = () => 2;
#else
                Func<int> f = () => 1;
#endif
                return f() + f();
            }
            return Twice(v);
        }
    }

    // Generic code names its own lambdas and local functions through an instantiation of the
    // type that holds them. Only the generated member differs in each of these edits.
    public class GenericLambdas<T>
    {
        private int seed;
#if CASE_GENERIC_TYPE_LAMBDA
        public int Pooled() { Func<int> f = () => 2; return f(); }
#else
        public int Pooled() { Func<int> f = () => 1; return f(); }
#endif
#if CASE_GENERIC_TYPE_THIS_LAMBDA
        public int Bound() { Func<int> f = () => seed + 2; return f(); }
#else
        public int Bound() { Func<int> f = () => seed + 1; return f(); }
#endif
#if CASE_GENERIC_TYPE_LOCAL_FUNCTION
        public int Local() { int L() => 2; return L(); }
#else
        public int Local() { int L() => 1; return L(); }
#endif
        public class Inner
        {
#if CASE_GENERIC_NESTED_LAMBDA
            public int Pooled() { Func<int> f = () => 2; return f(); }
#else
            public int Pooled() { Func<int> f = () => 1; return f(); }
#endif
        }
    }

    public class GenericMethodLambdas
    {
#if CASE_GENERIC_METHOD_LAMBDA
        public int Pool<T>(T v) { Func<int> f = () => 2; return f(); }
        public int Query<T>(T v) => new[] { 1, 2, 3 }.Count(x => x > 2);
#else
        public int Pool<T>(T v) { Func<int> f = () => 1; return f(); }
        public int Query<T>(T v) => new[] { 1, 2, 3 }.Count(x => x > 1);
#endif
    }

    public class Anonymous
    {
        private readonly List<int> data = new List<int> { 1, 2, 3 };
#if CASE_LAMBDA_ANONYMOUS_PARAMETER
        public string Existing() => string.Join(",", data.Select(v => new { Value = v, Text = "v" + v }).Select(p => p.Value + p.Text));
#else
        public string Existing() => string.Join(",", data.Select(v => new { Value = v, Text = "v" + v }).Select(p => p.Text + p.Value));
#endif
#if CASE_ANONYMOUS_TYPE_EXISTING
        public int Reuse() => data.Select(v => new { Value = v * 2, Text = "" }).Sum(p => p.Value);
#else
        public int Reuse() => data.Sum();
#endif
#if CASE_NEW_ANONYMOUS_TYPE
        public int Fresh() => data.Select(v => new { Square = v * v }).Sum(p => p.Square);
#else
        public int Fresh() => data.Sum();
#endif
    }

    // ---- types ---------------------------------------------------------------------------

#if CASE_ADD_TYPES
    public class AddedClass { public int X; }
    public struct AddedStruct { public int X; }
    public enum AddedEnum { A, B }
    public interface IAdded { }
    public delegate void AddedDelegate();
#endif

#if !CASE_REMOVE_TYPE
    public class Doomed { }
#endif

#if CASE_RENAME_TYPE
    public class Renamed { }
#else
    public class Renamable { }
#endif

    public enum Mode
    {
#if CASE_ENUM_ATTRIBUTE
        [Obsolete("off")]
#endif
        Off, On,
#if CASE_ENUM_MEMBER
        Auto,
#endif
    }

    public enum Level
    {
        Low = 1,
#if CASE_ENUM_VALUE
        High = 3,
#else
        High = 2,
#endif
    }

    public class Shape { }
    public class OtherShape { }
#if CASE_BASE_TYPE
    public class Circle : OtherShape { }
#else
    public class Circle : Shape { }
#endif

#if CASE_INTERFACE_LIST
    public class Marked : IMarker { }
#else
    public class Marked { }
#endif

#if CASE_TYPE_FLAGS
    public sealed class Flagged { }
#else
    public class Flagged { }
#endif

    public struct Layout
    {
        public int A;
#if CASE_STRUCT_FIELD_ADD
        public int Added;
#endif
        public int B;
    }

    public struct Ordered
    {
#if CASE_STRUCT_FIELD_REORDER
        public long B;
        public int A;
#else
        public int A;
        public long B;
#endif
    }

    [StructLayout(LayoutKind.Explicit)]
    public struct Overlay
    {
        [FieldOffset(0)] public int A;
#if CASE_EXPLICIT_LAYOUT
        [FieldOffset(8)] public int B;
#else
        [FieldOffset(4)] public int B;
#endif
    }

    public interface IGrowing
    {
        int A();
#if CASE_INTERFACE_MEMBER_ADD
        int B();
#endif
    }

    // ---- virtual methods, constructors, attributes ----------------------------------------

    public class Virtuals : Animal
    {
#if CASE_ADD_VIRTUAL
        public virtual int Fresh() => 1;
#endif
#if CASE_ADD_OVERRIDE
        public override string Speak() => "v";
#endif
#if !CASE_REMOVE_OVERRIDE
        public override string ToString() => "virtuals";
#endif
#if CASE_METHOD_FLAGS
        public virtual int Plain() => 1;
#else
        public int Plain() => 1;
#endif
    }

    public abstract class Contract
    {
        public abstract int Must();
#if CASE_ADD_ABSTRACT
        public abstract int Also();
#endif
    }

    public class Implementer : IWork
    {
        public int Work() => 1;
#if CASE_ADD_INTERFACE_IMPLEMENTATION
        int IWork.Work() => 2;
#endif
    }

    public class Statics
    {
#if CASE_STATIC_CTOR_CHANGE
        private static int seed = 2;
#else
        private static int seed = 1;
#endif
        public static int Seed => seed;
    }

    public class NoStatics
    {
        public int x;
#if CASE_STATIC_CTOR_ADD
        private static int primed = 5;
        public int Use() => x + primed;
#else
        public int Use() => x;
#endif
    }

    public class Built
    {
        public Built() { }
#if CASE_ADD_CTOR
        public Built(int x) { }
#endif
    }

    public class Native
    {
#if CASE_EXTERN_CHANGED
        [DllImport("libc", EntryPoint = "geteuid")] private static extern int Id();
#else
        [DllImport("libc", EntryPoint = "getuid")] private static extern int Id();
#endif
#if CASE_ADD_EXTERN
        [DllImport("libc")] private static extern int getpid();
        public int Pid() => getpid();
#else
        public int Pid() => 0;
#endif
#if CASE_MARSHAL
        [DllImport("libc")] private static extern int strlen([MarshalAs(UnmanagedType.LPWStr)] string s);
        [DllImport("libc")] private static extern void memset([Out] char[] buffer, int value, IntPtr count);
#else
        [DllImport("libc")] private static extern int strlen([MarshalAs(UnmanagedType.LPStr)] string s);
        [DllImport("libc")] private static extern void memset([In] char[] buffer, int value, IntPtr count);
#endif
    }

    // The runtime writes the Invoke of a delegate; native code calls it through that stub.
#if CASE_MARSHAL
    public delegate int NativeCallback([MarshalAs(UnmanagedType.LPWStr)] string text);
#else
    public delegate int NativeCallback([MarshalAs(UnmanagedType.LPStr)] string text);
#endif

#if CASE_ATTRIBUTE_TYPE
    [Obsolete("type")]
#endif
    public class AttributedType { }

    public class Attributed
    {
#if CASE_ATTRIBUTE_METHOD
        [Obsolete("method")]
#endif
        public int M() => 1;

#if CASE_ATTRIBUTE_FIELD
        [Obsolete("field")]
#endif
        public int f;

#if CASE_ATTRIBUTE_PARAMETER
        public string P([CallerMemberName] string name = "") => name;
#else
        public string P(string name = "") => name;
#endif
    }

    // A method becomes an async method or an iterator, and the other way round.
    public class Becoming
    {
#if CASE_ASYNC_ADDED
        public async Task Run() { await Task.Yield(); }
#else
        public Task Run() { return Task.CompletedTask; }
#endif
#if CASE_ITERATOR_ADDED
        public IEnumerable<int> List() { yield return 1; }
#else
        public IEnumerable<int> List() { return new[] { 1 }; }
#endif
#if CASE_ASYNC_REMOVED
        public Task<int> Was() { return Task.FromResult(2); }
#else
        public async Task<int> Was() { await Task.Yield(); return 1; }
#endif
#if CASE_ITERATOR_REMOVED
        public IEnumerable<int> WasList() { return new[] { 2 }; }
#else
        public IEnumerable<int> WasList() { yield return 1; }
#endif
    }

    public class Trees
    {
        private int value = 3;
#if CASE_EXPRESSION_TREE
        private int Added() => value;
        public Expression<Func<int>> Build() => () => Added();
#else
        public Expression<Func<int>> Build() => () => value;
#endif
    }

    // ---- Unity messages and the lifecycle rewrite -------------------------------------------

    public class Behaviour1 : MonoBehaviour
    {
        public int ticks;

        // What the lifecycle rewrite makes of a user's Awake: the wrapper, and the body in a helper.
        private void Awake() { __UnityPlay_Awake(); }
        private void __UnityPlay_Awake()
        {
#if CASE_LIFECYCLE_BODY
            ticks = 2;
#else
            ticks = 1;
#endif
        }

#if !CASE_REMOVE_UNITY_MESSAGE
        private void Update() { ticks++; }
#endif
#if CASE_ADD_UNITY_MESSAGE
        private void FixedUpdate() { ticks += 2; }
#endif
#if CASE_ADD_LIFECYCLE
        private void OnEnable() { __UnityPlay_OnEnable(); }
        private void __UnityPlay_OnEnable() { ticks += 3; }
#endif
#if CASE_ADD_ENGINE_METHOD
        // Not a message of the engine's list, and no string of this assembly names it: an input
        // component, an animation event or a UnityEvent of a scene would.
        private void OnJump() { ticks += 5; }
        public int Level { set { ticks = value; } }
        private static void Helper() { }
#endif
    }

    // Messages whose existence alone changes what the engine does.
    public class Rooted : MonoBehaviour
    {
#if !CASE_REMOVE_PRESENCE_MESSAGE
        private void OnAnimatorMove() { }
        private void OnGUI() { }
#endif
    }

    public class Starter : MonoBehaviour
    {
#if !CASE_REMOVE_UNITY_MESSAGE_NONVOID
        private IEnumerator Start() { yield return null; }
#endif
    }

    public class BaseBehaviour : MonoBehaviour
    {
        private void LateUpdate() { }
    }

    public class DerivedBehaviour : BaseBehaviour
    {
#if !CASE_REMOVE_UNITY_MESSAGE_BASE
        private void LateUpdate() { }
#endif
    }

    // Not an engine object: its Update is a method like any other.
    public class PlainTicker
    {
        public int n;
#if CASE_ADD_UPDATE_PLAIN
        public void Update() { n++; }
#endif
    }

    public class Serialized : MonoBehaviour
    {
#if CASE_ADD_SERIALIZED_FIELD
        public int exposed;
        [SerializeField] private int hidden;
        private int plain;
        public int Sum() => exposed + hidden + plain;
#else
        public int Sum() => 0;
#endif
    }

    // What the lifecycle rewrite appends for [RuntimeInitializeOnLoadMethod]: bridges numbered
    // across the assembly.
    public static class BootA
    {
#if CASE_RUNTIME_INITIALIZER_ADD
        public static void Extra() { }
        public static void __UnityPlay_RuntimeInitialize_0() { Extra(); }
#endif
    }

    public static class BootB
    {
        public static void Boot() { }
#if CASE_RUNTIME_INITIALIZER_ADD
        public static void __UnityPlay_RuntimeInitialize_1() { Boot(); }
#elif !CASE_RUNTIME_INITIALIZER_REMOVE
        public static void __UnityPlay_RuntimeInitialize_0() { Boot(); }
#endif
    }

    // ---- methods that run once for what exists ----------------------------------------------

    // A [RuntimeInitializeOnLoadMethod] method, its bridge and a helper only it calls: they ran
    // when the Player started and do not run again.
    public static class BootC
    {
        public static int mode;
        [RuntimeInitializeOnLoadMethod]
        private static void Init()
        {
#if CASE_ONCE_INITIALIZER
            mode = 2;
#else
            mode = 1;
#endif
            Register();
        }
        private static void Register()
        {
#if CASE_ONCE_INITIALIZER
            mode += 20;
#else
            mode += 10;
#endif
        }
        public static void __UnityPlay_RuntimeInitialize_9() { Init(); }
    }

    // What the lifecycle rewrite makes of OnEnable and OnDisable. Enabled objects do not run
    // OnEnable again; a changed OnDisable runs, and does not remove what the old OnEnable added.
    public class Listener : MonoBehaviour
    {
        public static event Action Scored;
        public int hits;
        private void OnEnable() { __UnityPlay_OnEnable(); }
        private void __UnityPlay_OnEnable()
        {
#if CASE_ONCE_ENABLE
            Scored += OnBonus;
#else
            Scored += OnScored;
#endif
        }
        private void OnDisable() { __UnityPlay_OnDisable(); }
        private void __UnityPlay_OnDisable()
        {
#if CASE_ONCE_ENABLE
            Scored -= OnBonus;
#else
            Scored -= OnScored;
#endif
        }
        private void OnScored() { hits++; }
        private void OnBonus() { hits += 2; }
    }

    // OnEnable of an asset runs when it loads; OnAfterDeserialize when an object is deserialized.
    public class Settings : ScriptableObject
    {
        public int limit;
        private void OnEnable() { __UnityPlay_OnEnable(); }
        private void __UnityPlay_OnEnable()
        {
#if CASE_ONCE_ASSET
            limit = 9;
#else
            limit = 5;
#endif
        }
    }

    public class Table : ISerializationCallbackReceiver
    {
        public string key = "";
        public void OnBeforeSerialize() { }
        public void OnAfterDeserialize()
        {
#if CASE_ONCE_DESERIALIZED
            key = key.ToLowerInvariant();
#else
            key = key.ToUpperInvariant();
#endif
        }
    }

    // A hook that only Awake calls (the template method of a base class), a helper only Awake
    // calls, and one that Update calls too.
    public abstract class Actor : MonoBehaviour
    {
        protected int health;
        private void Awake() { __UnityPlay_Awake(); }
        private void __UnityPlay_Awake() { OnSetup(); Prepare(); Rebuild(); }
        protected abstract void OnSetup();
        private void Prepare()
        {
#if CASE_ONCE_HOOK
            health += Bonus() * 2;
#else
            health += Bonus();
#endif
        }
#if CASE_ONCE_HOOK
        private int Bonus() => 2;
#else
        private int Bonus() => 1;
#endif
        private void Update() { health -= Bonus(); }
        // A framework calls it as well: the bridge runs an [InvokeOnHotReloadLocal] method again.
        [Rebuilt]
        private void Rebuild()
        {
#if CASE_ONCE_HOOK
            health += 3;
#else
            health += 2;
#endif
        }
    }

    [AttributeUsage(AttributeTargets.Method)]
    public sealed class RebuiltAttribute : Attribute { }

    public class Enemy : Actor
    {
        protected override void OnSetup()
        {
#if CASE_ONCE_HOOK
            health = 250;
#else
            health = 100;
#endif
        }
    }

    // A coroutine method that is renamed while instances of it run.
    public class Blinker : MonoBehaviour
    {
        private IEnumerator running;
        private void Update()
        {
#if CASE_COROUTINE_REMOVED
            running = Pulse();
#else
            running = Blink();
#endif
        }
#if CASE_COROUTINE_REMOVED
        private IEnumerator Pulse() { while (true) yield return 2; }
#else
        private IEnumerator Blink() { while (true) yield return 1; }
#endif
    }

    // ---- generated code that nothing names -------------------------------------------------

    // The compiler emits the method of a lambda or a local function also when no code leads to
    // it. These are the edits of work in progress: a method switched off for a moment, a debug
    // flag, a helper saved before its first call.
    public class Dead : MonoBehaviour
    {
        private const bool Verbose =
#if CASE_DEAD_FLAG_ON
            true;
#else
            false;
#endif
        private const bool Loud =
#if CASE_DEAD_FLAG_OFF
            false;
#else
            true;
#endif
        private int speed;
        private readonly List<int> seen = new List<int>();

        // A `return` in front of code with lambdas.
        public int Early()
        {
#if CASE_DEAD_EARLY_RETURN
            return 0;
#endif
            Func<int, int> twice = x => x * 2;
            return twice(speed) + seen.Count(x => x > speed);
        }

        // The same edit taken back: the loaded code has the lambda behind the `return`.
        public int Late()
        {
#if !CASE_DEAD_RETURN_REMOVED
            return 1;
#endif
            Func<int, int> thrice = x => x * 3;
            return thrice(speed);
        }

        // `throw` in front of a block with a lambda that captures a variable of the block. No
        // code makes an object of its closure class any more, but the class stays the type of a
        // local of the method, so it is compared with the method as before.
        public int Thrown(int from)
        {
#if CASE_DEAD_THROW
            throw new NotImplementedException();
#endif
            {
                int local = from + speed;
                Func<int, int> add = x => x + local;
                return add(1);
            }
        }

        // A debug flag: the lambda behind it is emitted whether the flag is on or off.
        public int Flagged()
        {
            if (Verbose) seen.ForEach(x => Console.WriteLine(x));
            if (Loud) seen.ForEach(x => Console.WriteLine(x + 1));
            return speed;
        }

        // An edit inside code that no flag switches on.
        public int Silenced()
        {
            const bool never = false;
#if CASE_DEAD_LAMBDA_EDITED
            if (never) seen.ForEach(x => Console.WriteLine(x + 2));
#else
            if (never) seen.ForEach(x => Console.WriteLine(x + 1));
#endif
            return speed;
        }

        // A local function written before anything calls it, and one edited while nothing does.
        public int Helper()
        {
#if CASE_DEAD_LOCAL_ADDED
            int Clamp(int v) { return v > 10 ? 10 : v; }
#endif
#if CASE_DEAD_LOCAL_EDITED
            int Unused(int v) { return v + 2; }
#else
            int Unused(int v) { return v + 1; }
#endif
            return speed + 1;
        }

        // The only call of a local iterator with a lambda inside is commented out.
        public int Called()
        {
            IEnumerable<int> Numbers() { yield return speed; yield return seen.Count(x => x > 0); }
#if CASE_DEAD_CALL_REMOVED
            return 0;
#else
            return Numbers().Sum();
#endif
        }
    }

    // ---- what Unity serializes ---------------------------------------------------------------

    public enum Pace { Idle, Busy }
    public struct Unlisted { public int x; }
    public delegate void Tick();
    public class Loose { public int x; }

    // The accessibility of a field decides what Unity serializes only for a type it can serialize.
    public class ExposureTypes : MonoBehaviour
    {
#if CASE_ACCESSIBILITY_TYPES_SERIALIZED
        public string title;
        public Pace mode;
        public int[] numbers;
        public List<float> weights;
        public ExposureData data;
        public Starting other;
        public List<ExposureData> all;
#else
        private string title;
        private Pace mode;
        private int[] numbers;
        private List<float> weights;
        private ExposureData data;
        private Starting other;
        private List<ExposureData> all;
#endif
#if CASE_ACCESSIBILITY_TYPES_NOT_SERIALIZED
        public Dictionary<string, int> table;
        public Action onChanged;
        public Tick onTick;
        public object tag;
        public IMarker marker;
        public int[,] grid;
        public int[][] rows;
        public List<List<int>> nested;
        public int? maybe;
        public (int, int) pair;
        public Loose loose;
        public Unlisted unlisted;
        public DateTime when;
        public HashSet<int> set;
        public Task task;
        public IEnumerator routine;
#else
        private Dictionary<string, int> table;
        private Action onChanged;
        private Tick onTick;
        private object tag;
        private IMarker marker;
        private int[,] grid;
        private int[][] rows;
        private List<List<int>> nested;
        private int? maybe;
        private (int, int) pair;
        private Loose loose;
        private Unlisted unlisted;
        private DateTime when;
        private HashSet<int> set;
        private Task task;
        private IEnumerator routine;
#endif
    }

    // Unity serializes no read-only field, so `readonly` decides it like the accessibility does.
    public class Frozen2 : MonoBehaviour
    {
#if CASE_READONLY_SERIALIZED
        public int max = 5;
        public readonly int min = 1;
        [SerializeField] private int marked = 2;
        private int hidden = 3;
        public Action handler;
#else
        public readonly int max = 5;
        public int min = 1;
        [SerializeField] private readonly int marked = 2;
        private readonly int hidden = 3;
        public readonly Action handler;
#endif
        public int Sum() => max + min + marked + hidden;
    }

    // ---- Start and Awake that the engine does not call ---------------------------------------

    public class StartOverload : MonoBehaviour
    {
        private int n;
#if CASE_START_NOT_A_MESSAGE
        public void Start(int from) { n = from + 1; }
        public void Awake(int from) { n = from + 2; }
#else
        public void Start(int from) { n = from; }
        public void Awake(int from) { n = from; }
#endif
    }

    public class Quest : ScriptableObject
    {
        private int n;
#if CASE_START_NOT_A_MESSAGE
        public void Start() { n = 2; }
        private void Awake() { n = 2; }
#else
        public void Start() { n = 1; }
        private void Awake() { n = 1; }
#endif
    }

    // ---- attributes only a compiler reads -----------------------------------------------------

    public struct Meter
    {
        public int value;
#if CASE_COMPILER_ATTRIBUTES
        public readonly int Read() => value;
        public (int low, int high) Range() => (value, value);
        public static int Sum(params int[] values) => values.Length;
        public (int first, int second) held;
#else
        public int Read() => value;
        public (int min, int max) Range() => (value, value);
        public static int Sum(int[] values) => values.Length;
        public (int a, int b) held;
#endif
    }

    // ---- an added method that a string names ---------------------------------------------------

    public class Caller : MonoBehaviour
    {
        private int n;
#if CASE_ADD_METHOD_UNNAMED || CASE_ADD_METHOD_NAMED || CASE_ADD_METHOD_NAMED_TWICE
        private void Blast() { n++; }
#endif
        public void Tick()
        {
#if CASE_ADD_METHOD_NAMED || CASE_ADD_METHOD_NAMED_TWICE
            Invoke("Blast", 0.1f);
#else
            n += 2;
#endif
        }
        public void Tock()
        {
#if CASE_ADD_METHOD_NAMED_TWICE
            Invoke("Blast", 0.2f);
#else
            n += 3;
#endif
        }
    }

    // ---- methods that run once through a constructor, a static constructor, a public method or
    // a call by a name the code writes (stage 6: builder6-2, builder6-3, netcode6-1) -------------

    // A table a static field initializer builds once, when the type is first used.
    public class Tables
    {
        static readonly int[] Damage = BuildDamage();
#if CASE_ONCE_CCTOR_HELPER
        static int[] BuildDamage() { return new[] { 10, 20, 30 }; }
#else
        static int[] BuildDamage() { return new[] { 1, 2, 3 }; }
#endif
        public int Get(int i) => Damage[i];
    }

    // Values the field initializers of another class take once per object: a public property and
    // a public helper of a class of their own.
    public static class Rules
    {
#if CASE_ONCE_CTOR_HELPER
        public static int Level => 2;
        public static List<float> Weights() => new List<float> { 0.25f, 0.75f };
#else
        public static int Level => 1;
        public static List<float> Weights() => new List<float> { 0.5f, 0.5f };
#endif
    }
    public class Spawner
    {
        readonly int level = Rules.Level;
        readonly List<float> weights = Rules.Weights();
        public float First() => weights[0] + level;
    }

    // A public method that only Awake calls in the game's code.
    public class Brute : MonoBehaviour
    {
        public int health;
        private void Awake() { Init(); }
#if CASE_ONCE_PUBLIC_INIT
        public void Init() { health = 250; }
#else
        public void Init() { health = 100; }
#endif
    }

    // A method Start calls by the name it writes.
    public class Delayed : MonoBehaviour
    {
        private int ready;
        private void Start() { Invoke(nameof(LateInit), 0.5f); }
#if CASE_ONCE_INVOKE_NAMEOF
        private void LateInit() { ready = 2; }
#else
        private void LateInit() { ready = 1; }
#endif
    }

    // An edit only inside the lambda OnEnable subscribes: the delegate is redirected, and the edit
    // is live for the objects that exist (builder6-8).
    public class Jumper : MonoBehaviour
    {
        public event Action Jumped;
        private readonly List<string> log = new List<string>();
        private void OnEnable()
        {
#if CASE_ONCE_LAMBDA_ONLY
            Jumped += () => log.Add("jump B");
#else
            Jumped += () => log.Add("jump A");
#endif
        }
        public void Jump() => Jumped?.Invoke();
    }

    // ---- an added method that a string only equals (builder6-9) ----------------------------

    public class Hopper : MonoBehaviour
    {
        public string button = "Hop";
#if CASE_ADD_METHOD_STRING_ONLY
        private void Hop() { }
#endif
    }

    // ---- a renamed field that keeps its data with [FormerlySerializedAs] (stage 5 runner5-2) --

    public class Dimmer : MonoBehaviour
    {
#if CASE_FIELD_FORMERLY
        [UnityEngine.Serialization.FormerlySerializedAs("dim")] public float dimLevel = 0.5f;
#else
        public float dim = 0.5f;
#endif
    }
}
