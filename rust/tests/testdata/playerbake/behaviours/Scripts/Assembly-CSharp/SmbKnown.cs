using System;
using System.Collections.Generic;
using UnityEngine;
using Smb.Test.Enums;
[Serializable] public class Known2 { public int k = 7; public string t = "seven"; public bool on = true; }
[Serializable]
public class Known
{
    public int hp = 100; public string s = "x"; public float f = 1.5f; public bool on = true; public int plain; public Known2 sub = new Known2(); public Known2 subNull; public List<int> list = new List<int>();
    public Vector3 v; public Known2[] subs; public EInt e = EInt.C; public int[] none = new int[0];
}
[Serializable] public struct KnownS { public int a; public Known2 c; public string s; }
[Serializable] public class KnownNoCtor { public int q = 3; public string w = "w"; public Known2 sub = new Known2(); public KnownNoCtor(int x) { q = x; } }
// Every initializer is one the reader follows.
public class SmbKnown : StateMachineBehaviour
{
    public int i0; public int i5 = 5; public uint u = 4000000000; public long l = -7; public ulong ul = 18446744073709551615; public short sh = -3; public byte by = 200; public sbyte sb = -5; public bool on = true; public bool off;
    public float f = 1.5f; public double d = 2.5; public char c = 'x'; public string s = "hello"; public string sNull; public string sEmpty = ""; public EInt e = EInt.C; public EByte eb = EByte.B;
    public float fNeg = -0.0f; public float fInf = float.PositiveInfinity; public float fNaN = float.NaN; public double dBig = 1e300; public int iMin = int.MinValue; public float fThird = 1f / 3f;
    public string sUni = "gämma 中"; public char cUni = '中'; public long lBig = 5000000000; public ushort us = 60000; public double dThird = 1.0 / 3.0; public int m1 = -1; public int big = 100000;
    public float fSmall = 1e-10f; public double dNeg = -123456789.123456789; public float fMax = float.MaxValue; public double dEps = double.Epsilon; public string sQuote = "a \"b\" c\n d"; public float fInt = 3;
    public Known a; public Known b = new Known(); public Known[] items; public List<Known> itemList = new List<Known>(); public KnownS st; public KnownS[] sts; public KnownNoCtor ndc; public KnownNoCtor[] ndcs;
    public Vector3 v; public Color col; public AnimationCurve curve; public Gradient grad; public LayerMask mask; public RectOffset ro; public Rect rect; public Bounds bn; public Quaternion q; public Color32 c32; public Hash128 h;
    public Matrix4x4 m; public int[] arr; public string[] strs; public AnimationClip clip; public Vector2Int v2i; public List<string> names = new List<string>(); public int ctorSet; public byte tail = 9;
    public SmbKnown() { ctorSet = 12; }
}
