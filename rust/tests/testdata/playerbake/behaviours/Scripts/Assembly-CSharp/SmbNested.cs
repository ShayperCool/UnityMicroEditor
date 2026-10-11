using System;
using System.Collections.Generic;
using UnityEngine;
namespace Smb.Test
{
    [Serializable] public struct Inner { public int a; public string name; public Vector3 pos; public bool on; }
    [Serializable] public class Outer { public Inner inner; public float w; public Inner[] inners; public List<string> tags; public bool tail; [SerializeField] private byte hiddenByte; [NonSerialized] public int skipped; }
    [Serializable] public class Generic<T> { public T value; public List<T> list; }
    [Serializable] public struct EmptyStruct { }
    [Serializable] public class OnlyPrivate { private int x; }
    public struct NotSerializableStruct { public int x; }
    public class NotSerializableClass { public int x; }
    [Serializable] public struct TwoBools { public bool p; public bool q; }
    [Serializable] public struct Level2 { public TwoBools bools; public byte b; }
    [Serializable] public class Level1 { public Level2 l2; public Level2[] l2s; public short s; }
    public class SmbNested : StateMachineBehaviour
    {
        public Inner inner; public Outer outer; public Outer[] outers; public List<Inner> innerList;
        public Generic<int> gi; public Generic<Inner> gin; public EmptyStruct es; public OnlyPrivate op; public NotSerializableStruct nss; public NotSerializableClass nsc;
        public TwoBools tb; public byte afterTb; public Level1 l1; public List<Level1> l1s; public EmptyStruct[] ess; public byte after;
        [Serializable] public struct NestedType { public int z; }
        public NestedType nt;
    }
}
