using UnityEngine;
using Smb.Test.Enums;
// Every initializer is a constant: the class a missing field can be filled in for.
public class SmbInitPlain : StateMachineBehaviour
{
    public int i0; public int i5 = 5; public uint u = 4000000000; public long l = -7; public ulong ul = 18446744073709551615; public short sh = -3; public byte by = 200; public sbyte sb = -5; public bool on = true; public bool off;
    public float f = 1.5f; public double d = 2.5; public char c = 'x'; public string s = "hello"; public string sNull; public string sEmpty = ""; public EInt e = EInt.C; public EByte eb = EByte.B;
    public float fNeg = -0.0f; public float fInf = float.PositiveInfinity; public float fNaN = float.NaN; public double dBig = 1e300; public int iMin = int.MinValue; public float fThird = 1f / 3f; public int iExpr = 3 * 7;
    public string sUni = "gämma 中"; public char cUni = '中'; public long lBig = 5000000000; public ushort us = 60000; public double dThird = 1.0 / 3.0; public int m1 = -1; public int big = 100000; public int i8 = 8; public int i100 = 100;
    public Init a; public Init[] items; public InitS st; public Vector3 v; public AnimationCurve curve; public Gradient grad; public int[] arrNull; public AnimationClip clip; public Rect rect; public Hash128 h; public RectOffset ro;
    public Color32 c32; public Bounds bn; public Matrix4x4 m; public Quaternion q; public Vector2Int v2i; public string[] strs; public LayerMask mask; public Color col; public byte tail = 9;
}
