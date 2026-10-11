using System;
using System.Collections.Generic;
using UnityEngine;
using UnityEngine.Serialization;
public abstract class AbstractThing { public int x; }
[Serializable] public abstract class AbstractSerializable { public int x; }
[Serializable] public class ConcreteThing : AbstractSerializable { public int y; }
[Serializable] public class DerivedData : BaseData { public int dd; }
[Serializable] public class BaseData { public int bd; [SerializeField] private int bp; }
public class DerivedNoAttr : BaseData { public int q; }
[Serializable] public class DerivedOfPlain : PlainBase { public int own; }
public class PlainBase { public int plainBaseField; }
[Serializable] public sealed class SealedData { public int sd; }
[Serializable] public static class StaticData { public static int z; }
public class SmbRules : StateMachineBehaviour
{
    public int pub;
    [SerializeField] private int priv;
    private int privNo;
    [NonSerialized] public int nonSer;
    public int Prop { get; set; }
    [field: SerializeField] public int AutoProp { get; set; }
    public static int stat; public const int K = 3; public readonly int ro = 4;
    [SerializeField] static int statSer; [SerializeField] readonly int roSer = 5;
    protected int prot; [SerializeField] protected int protSer; internal int intern; [SerializeField] internal int internSer;
    [FormerlySerializedAs("oldName")] public int renamed;
    [HideInInspector] public int hidden;
    public volatile int vol;
    public Dictionary<string, int> dict; public object obj; public Action act; public int? nullable; public DateTime dt; public decimal dec; public Guid guid; public IntPtr ptr; public (int, int) tup;
    public HashSet<int> set; public Queue<int> queue; public KeyValuePair<int, int> kvp; public Type type; public Tuple<int, int> tuple; public nint ni; public TimeSpan ts;
    public AbstractThing abs; public AbstractSerializable absSer; public ConcreteThing concrete; public IThing iface;
    public DerivedData derived; public DerivedNoAttr derivedNoAttr; public DerivedOfPlain derivedOfPlain; public SealedData sealedData;
    [SerializeField] private string privStr; [SerializeField] private int[] privArr;
    [NonSerialized, SerializeField] private int both;
    public Vector3? nullableVec; public Lazy<int> lazy; public WeakReference weak; public System.Text.StringBuilder sb; public Version ver; public Uri uri;
    public int last;
}
