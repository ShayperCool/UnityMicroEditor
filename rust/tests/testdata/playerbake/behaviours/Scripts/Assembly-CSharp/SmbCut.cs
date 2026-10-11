using System;
using System.Collections.Generic;
using UnityEngine;
public enum CutE8 : byte { A, B, C }
public enum CutE32 { A, B, C }
[Serializable] public struct CutS { public int a; }
[Serializable] public class C0 { public int v; public C1 next; }
[Serializable] public class C1 { public int v; public C2 next; }
[Serializable] public class C2 { public int v; public C3 next; }
[Serializable] public class C3 { public int v; public C4 next; }
[Serializable] public class C4 { public int v; public C5 next; }
[Serializable] public class C5 { public int v; public C6 next; }
[Serializable] public class C6 { public int v; public C7 next; }
[Serializable] public class C7 { public int v; public C8 next; }
[Serializable] public class C8 { public int v; public C9 next; }
[Serializable] public class C9 { public int v; public C10 next; public C10[] arr; }
[Serializable] public class C10
{
    public bool b; public byte u8; public sbyte s8; public short s16; public ushort u16; public int i32; public uint u32; public long i64; public ulong u64; public float f; public double d; public char c; public string s;
    public CutE8 e8; public CutE32 e32; public int[] ints; public List<float> floats; public string[] strs; public CutS st; public CutS[] sts; public AnimationClip clip; public Vector3 vec; public Color col;
    public LayerMask mask; public AnimationCurve curve; public Gradient grad; [SerializeField] private int priv; [HideInInspector] public int hid; public bool b2; public float f2; public C11 next; public Hash128 h; public Rect r;
    public Quaternion q; public byte[] bytes; public bool[] bools; public long[] longs; public char[] chars; public CutE32[] enums; public byte tail;
}
[Serializable] public class C11 { public int v; }
public class SmbCut : StateMachineBehaviour { public C0 c; public byte tail; }
