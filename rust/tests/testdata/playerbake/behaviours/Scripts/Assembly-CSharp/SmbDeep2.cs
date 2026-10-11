using System;
using UnityEngine;
using Smb.Test;
using Smb.Test.Enums;
[Serializable] public class E0 { public int v; public E1 next; }
[Serializable] public class E1 { public int v; public E2 next; }
[Serializable] public class E2 { public int v; public E3 next; }
[Serializable] public class E3 { public int v; public E4 next; }
[Serializable] public class E4 { public int v; public E5 next; }
[Serializable] public class E5 { public int v; public E6 next; }
[Serializable] public class E6 { public int v; public E7 next; }
[Serializable] public class E7 { public int v; public E8 next; }
[Serializable] public class E8 { public int v; public E9 next; }
[Serializable] public class E9 { public int v; public E10 next; }
[Serializable] public class E10 { public int v; public int[] ints; public string[] strs; public AnimationClip clip; public EByte en; public AnimationCurve curve; public Color32 c32; public LayerMask mask; public Vector3 vec; public Inner inner; public AnimationClip[] clips; public bool b; public string s; public Gradient grad; public float f; public Inner[] inners; public Rect rect; public char c; }
public class SmbDeep2 : StateMachineBehaviour { public E0 e; public byte tail; }
