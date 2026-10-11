using System;
using UnityEngine;
[Serializable] public class D0 { public int v; public Vector2 p; public string s; public D1 next; public D1[] arr; }
[Serializable] public class D1 { public int v; public Vector2 p; public string s; public D2 next; public D2[] arr; }
[Serializable] public class D2 { public int v; public Vector2 p; public string s; public D3 next; public D3[] arr; }
[Serializable] public class D3 { public int v; public Vector2 p; public string s; public D4 next; public D4[] arr; }
[Serializable] public class D4 { public int v; public Vector2 p; public string s; public D5 next; public D5[] arr; }
[Serializable] public class D5 { public int v; public Vector2 p; public string s; public D6 next; public D6[] arr; }
[Serializable] public class D6 { public int v; public Vector2 p; public string s; public D7 next; public D7[] arr; }
[Serializable] public class D7 { public int v; public Vector2 p; public string s; public D8 next; public D8[] arr; }
[Serializable] public class D8 { public int v; public Vector2 p; public string s; public D9 next; public D9[] arr; }
[Serializable] public class D9 { public int v; public Vector2 p; public string s; public D10 next; public D10[] arr; }
[Serializable] public class D10 { public int v; public Vector2 p; public string s; public D11 next; public D11[] arr; }
[Serializable] public class D11 { public int v; public Vector2 p; public string s; public D12 next; public D12[] arr; }
[Serializable] public class D12 { public int v; public Vector2 p; public string s; }
public class SmbDeep : StateMachineBehaviour { public D0 d; public byte tail; }
