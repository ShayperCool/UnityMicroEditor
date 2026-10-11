using System;
using System.Collections.Generic;
using UnityEngine;
[Serializable] public struct T0 { public int v; public T1 next; }
[Serializable] public struct T1 { public int v; public T2 next; }
[Serializable] public struct T2 { public int v; public T3 next; }
[Serializable] public struct T3 { public int v; public T4 next; }
[Serializable] public struct T4 { public int v; public T5 next; }
[Serializable] public struct T5 { public int v; public T6 next; }
[Serializable] public struct T6 { public int v; public T7 next; }
[Serializable] public struct T7 { public int v; public T8 next; }
[Serializable] public struct T8 { public int v; public T9 next; }
[Serializable] public struct T9 { public int v; public bool b; public T10 next; }
[Serializable] public struct T10 { public int v; public string s; public bool b; public T11 next; public T11[] arr; }
[Serializable] public struct T11 { public int v; public bool b; }
[Serializable] public class K1 { public int v; public K2 next; }
[Serializable] public class K2 { public int v; public K3 next; }
[Serializable] public class K3 { public int v; public K4 next; }
[Serializable] public class K4 { public int v; public K5 next; }
[Serializable] public class K5 { public int v; public K6 next; }
[Serializable] public class K6 { public int v; public K7 next; }
[Serializable] public class K7 { public int v; public K8 next; }
[Serializable] public class K8 { public int v; public K9 next; }
[Serializable] public class K9 { public int v; public bool b; public K10 next; }
[Serializable] public class K10 { public int v; public bool b; public K11 next; }
[Serializable] public class K11 { public int v; public bool b; }
public class SmbCut2 : StateMachineBehaviour
{
    public T0 t; [SerializeField] private Dictionary<int, K1> dict; public byte tail;
    public void Set()
    {
        var k = new K1 { v = 1 };
        k.next = new K2 { v = 2 }; k.next.next = new K3 { v = 3 }; k.next.next.next = new K4 { v = 4 }; k.next.next.next.next = new K5 { v = 5 };
        var k5 = k.next.next.next.next; k5.next = new K6 { v = 6 }; k5.next.next = new K7 { v = 7 }; k5.next.next.next = new K8 { v = 8 };
        var k8 = k5.next.next.next; k8.next = new K9 { v = 9, b = true }; k8.next.next = new K10 { v = 10, b = true }; k8.next.next.next = new K11 { v = 11, b = true };
        dict = new Dictionary<int, K1> { { 42, k } };
        t.v = 100; t.next.next.next.next.next.next.next.next.next.b = true; t.next.next.next.next.next.next.next.next.next.next.v = 110; t.next.next.next.next.next.next.next.next.next.next.b = true;
        t.next.next.next.next.next.next.next.next.next.next.s = "ten"; tail = 9;
    }
}
