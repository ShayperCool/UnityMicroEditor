using System;
using System.Collections.Generic;
using UnityEngine;
using Smb.Test.Enums;
[Serializable] public class Init2 { public int k = 7; public string t = "seven"; }
[Serializable]
public class Init
{
    public int hp = 100; public string s = "x"; public float f = 1.5f; public bool on = true; public int plain; public Init2 sub = new Init2(); public Init2 subNull; public int[] arr = { 1, 2 };
    public List<int> list = new List<int>(); public Vector3 v = Vector3.one;
}
[Serializable] public struct InitS { public int a; public Init2 c; public string s; }
[Serializable] public class NoDefaultCtor { public int q = 3; public string w = "w"; public NoDefaultCtor(int x) { q = x; } }
[Serializable] public class CtorBody { public int a = 1; public int b; public CtorBody() { b = a + 41; } }
public class SmbInit : StateMachineBehaviour
{
    public int i0; public int i5 = 5; public uint u = 4000000000; public long l = -7; public ulong ul = 18446744073709551615; public short sh = -3; public byte by = 200; public sbyte sb = -5; public bool on = true; public bool off;
    public float f = 1.5f; public double d = 2.5; public char c = 'x'; public string s = "hello"; public string sNull; public string sEmpty = ""; public EInt e = EInt.C; public EByte eb = EByte.B;
    public float fNeg = -0.0f; public float fInf = float.PositiveInfinity; public float fNaN = float.NaN; public double dBig = 1e300; public int iMin = int.MinValue; public float fThird = 1f / 3f; public int iExpr = 3 * 7;
    public string sUni = "gämma 中"; public char cUni = '中'; public long lBig = 5000000000; public ushort us = 60000; public double dThird = 1.0 / 3.0;
    public Init a; public Init b = new Init(); public Init c5 = new Init { hp = 5 }; public Init[] items; public List<Init> itemList = new List<Init>(); public InitS st; public NoDefaultCtor ndc; public CtorBody cb; public CtorBody[] cbs;
    public Vector3 v; public Vector3 v1 = new Vector3(1, 2, 3); public Color col = Color.red; public AnimationCurve curve; public AnimationCurve curveInit = AnimationCurve.Linear(0, 0, 1, 1); public Gradient grad;
    public Gradient gradInit = new Gradient(); public LayerMask mask = 3; public int[] arrNull; public int[] arrInit = { 1, 2, 3 }; public AnimationClip clip; public Rect rect; public Hash128 h; public RectOffset ro;
    public Color32 c32; public Bounds bn; public Matrix4x4 m; public Quaternion q; public Quaternion qi = Quaternion.identity; public Vector2Int v2i; public string[] strs; public List<string> strList = new List<string> { "a" };
    public int ctorSet; public int afterCtor = 1; public byte tail = 9;
    public SmbInit() { ctorSet = 12; }
}
