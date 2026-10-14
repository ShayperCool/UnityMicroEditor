// What Unity 6000.6.3f1 serializes, against the real engine modules (CoreModule, PhysicsModule,
// InputLegacyModule), which lie next to the loaded assembly as in a Player's Managed directory.
// The facts come from the Editor of that version: SerializedObject of components that have every
// such field, and the warnings of its serialization analyzer (UAC1001, UAC1011, UAC1015).
// Compiled with unsafe code allowed, for the fixed buffer.
using System;
using System.Collections.Generic;
using UnityEngine;
using UnityEngine.Events;

namespace Serial
{
    public enum Wide : long { A, B = 5000000000 }
    public enum UWide : ulong { A, B = 1 }
    public enum Narrow : byte { A, B }

    // Fields of types Unity never serializes: an accessibility change is a note.
    public class Skipped : MonoBehaviour
    {
#if SER_ENGINE_ACCESS
        private Wide wide;
        private UWide uwide;
        private Coroutine routine;
        private WaitForSeconds wait;
        private Ray aim;
        private Ray2D aim2;
        private RaycastHit hit;
        private Touch touch;
        private Plane ground;
        private RangeInt range;
        private Unity.Collections.NativeArray<int> native;
        private UnityEngine.Pool.ObjectPool<GameObject> pool;
        private List<Coroutine> routines;
#else
        public Wide wide;
        public UWide uwide;
        public Coroutine routine;
        public WaitForSeconds wait;
        public Ray aim;
        public Ray2D aim2;
        public RaycastHit hit;
        public Touch touch;
        public Plane ground;
        public RangeInt range;
        public Unity.Collections.NativeArray<int> native;
        public UnityEngine.Pool.ObjectPool<GameObject> pool;
        public List<Coroutine> routines;
#endif
    }

    // Fields of types Unity serializes: an accessibility change is refused.
    public class Kept : MonoBehaviour
    {
#if SER_ENGINE_ACCESS
        private Narrow narrow;
        private Bounds bounds;
        private LayerMask mask;
        private Hash128 hash;
        private Matrix4x4 matrix;
        private Vector3Int cell;
        private Gradient gradient;
        private AnimationCurve curve;
        private RectOffset offset;
        private Rect rect;
        private Color32 tint;
        private PropertyName property;
        private Quaternion turn;
        private UnityEvent clicked;
        private UnityEvent<int> scored;
        private LazyLoadReference<Texture2D> lazy;
        private ExposedReference<Texture> exposed;
        private Texture2D texture;
        private UnityEngine.Object asset;
#else
        public Narrow narrow;
        public Bounds bounds;
        public LayerMask mask;
        public Hash128 hash;
        public Matrix4x4 matrix;
        public Vector3Int cell;
        public Gradient gradient;
        public AnimationCurve curve;
        public RectOffset offset;
        public Rect rect;
        public Color32 tint;
        public PropertyName property;
        public Quaternion turn;
        public UnityEvent clicked;
        public UnityEvent<int> scored;
        public LazyLoadReference<Texture2D> lazy;
        public ExposedReference<Texture> exposed;
        public Texture2D texture;
        public UnityEngine.Object asset;
#endif
    }

    // A fixed buffer of a [Serializable] struct is serialized as an array of its size.
    [Serializable]
    public unsafe struct Packed
    {
#if SER_FIXED
        fixed byte bytes[8];
#else
        public fixed byte bytes[8];
#endif
        public int after;
    }

    // Unity 6 serializes a Dictionary with [SerializeField]; without it (UAC1015), read-only or of
    // another kind it does not.
    public class Shop : MonoBehaviour
    {
#if SER_DICT_READONLY
        [SerializeField] Dictionary<string, int> prices = new Dictionary<string, int>();
        [SerializeField] SortedDictionary<string, int> sorted = new SortedDictionary<string, int>();
        public Dictionary<string, int> open = new Dictionary<string, int>();
#else
        [SerializeField] readonly Dictionary<string, int> prices = new Dictionary<string, int>();
        [SerializeField] readonly SortedDictionary<string, int> sorted = new SortedDictionary<string, int>();
        public readonly Dictionary<string, int> open = new Dictionary<string, int>();
#endif
#if SER_DICT_ADD
        [SerializeField] Dictionary<string, int> stock;
        [SerializeField] Dictionary<int, List<int>> lists;
        public Dictionary<int, int> unmarked;
        [SerializeField] HashSet<int> set;
        public int Count() => stock.Count + lists.Count + unmarked.Count + set.Count;
#else
        public int Count() => 0;
#endif
    }
}
