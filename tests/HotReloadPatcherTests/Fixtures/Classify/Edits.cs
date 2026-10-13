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
}
