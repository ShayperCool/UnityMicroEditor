using System;
using System.Collections.Generic;
using System.Linq;
using UnityEngine;
// Constructors whose values are not constants: a document that lacks the fields holds what running them leaves.
[Serializable] public class PrivCtor { public int q = 3; public string w = "w"; PrivCtor() { q = 4; } public static PrivCtor Make() { return new PrivCtor(); } }
[Serializable] public class ArgCtor { public int q = 3; public string w; public ArgCtor(int x, string y = "d") { q = x; w = y; } }
[Serializable] public class StaticInit { public static readonly int Base = Compute(); static int Compute() { return 17; } public int v = Base * 2; public List<int> squares = Enumerable.Range(1, 3).Select(n => n * n).ToList(); }
[Serializable] public class CtorPart { public int hp = 100; public string s = "x"; public CtorPart2 sub = new CtorPart2 { k = 70 }; public int[] arr = { 1, 2 }; public Vector3 v = Vector3.one * 2f; }
[Serializable] public class CtorPart2 { public int k = 7; public string t = "seven"; }
[Serializable] public struct CtorStruct { public int a; public CtorPart2 c; public string s; public CtorStruct(int n) { a = n; c = new CtorPart2 { k = n }; s = "s" + n; } }
public class SmbCtor : SmbCtorBase
{
    const int K = 4;
    public int call = Twice(21); public int branch; public int[] arr = { 1, 2, 3 }; public List<string> names = new List<string> { "a", "b" }; public string[] empty = new string[2];
    public PrivCtor priv; public PrivCtor privMade = PrivCtor.Make(); public PrivCtor[] privs; public ArgCtor arg = new ArgCtor(7, "seven"); public ArgCtor argNull; public ArgCtor[] args = { new ArgCtor(1), null, new ArgCtor(2, "two") };
    public StaticInit si = new StaticInit(); public string concat = "a" + K.ToString() + 'c'; public float root = Mathf.Sqrt(16f); public Vector3 v = Vector3.up * 2f; public Color c = Color.Lerp(Color.red, Color.blue, 0.5f);
    public Vector3 norm = new Vector3(3, 4, 0).normalized; public Quaternion q = new Quaternion(0, 0.6f, 0, 0.8f); public Rect rect = new Rect(1, 2, 3, 4); public Bounds bounds = new Bounds(Vector3.one, Vector3.one * 4);
    public LayerMask mask = 1 << 3 | 1 << 5; public Color32 c32 = new Color32(1, 2, 3, 4); public Vector2Int v2i = new Vector2Int(3, -4); public Matrix4x4 m = Matrix4x4.identity;
    public CtorPart part = new CtorPart { hp = 5 }; public CtorPart partA; public CtorPart partB; public CtorPart[] parts = { new CtorPart { hp = 6 }, new CtorPart { s = "second" } }; public List<CtorPart> partList;
    public CtorStruct st = new CtorStruct(9); public CtorStruct[] sts = { new CtorStruct(1) }; public double d = Math.Pow(2, 0.5); public long big = long.MaxValue / 3; public char ch = (char)('a' + K); public string nul = null;
    public AnimationClip clip; public AnimationCurve curve; public Gradient grad; public int ctorSet; public byte tail = 9;
    public SmbCtor()
    {
        branch = arr.Length > 2 ? 10 : 20; var shared = new CtorPart { hp = 77 }; partA = shared; partB = shared; partList = new List<CtorPart>();
        for (int i = 0; i < 2; i++) partList.Add(new CtorPart { hp = i, sub = null });
        ctorSet = names.Count + 12;
    }
}
