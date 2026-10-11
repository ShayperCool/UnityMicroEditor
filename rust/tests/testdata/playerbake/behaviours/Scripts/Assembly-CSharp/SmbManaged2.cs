using System;
using System.Collections.Generic;
using UnityEngine;
namespace Gap.Man
{
    public interface INode { }
    [Serializable] public class Leaf : INode { public int v = 1; public string s; }
    [Serializable] public class Branch : INode { [SerializeReference] public INode left; [SerializeReference] public INode right; public Leaf inline; public AnimationClip clip; [SerializeReference] public List<INode> kids; public int tag; }
    [Serializable] public class Outer { [Serializable] public class Inner : INode { public float f; } }
    [Serializable] public class Gen<T> : INode { public T value; }
    [Serializable] public struct SNode : INode { public int a; }
    [Serializable] public class EmptyNode : INode { }
    [Serializable] public class Holder { [SerializeReference] public INode node; public int x; }
}
[Serializable] public class GlobalNode : Gap.Man.INode { public bool on; public byte b; }
public class SmbManaged2 : StateMachineBehaviour
{
    public int before;
    [SerializeReference] public Gap.Man.INode a; [SerializeReference] public Gap.Man.INode shared1; [SerializeReference] public Gap.Man.INode shared2; [SerializeReference] public Gap.Man.INode nullRef;
    [SerializeReference] public Gap.Man.INode[] arr; [SerializeReference] public List<Gap.Man.INode> list; public Gap.Man.Holder holder; public Gap.Man.Holder[] holders;
    [SerializeReference] public object obj; [SerializeReference] public Gap.Man.Leaf concrete; [SerializeReference] public Gap.Man.INode cyc; public int after;
    public void Set(int n, AnimationClip clip, bool generics = true)
    {
        before = n; after = n + 1;
        var x = new Gap.Man.Leaf { v = n + 2, s = "shared" };
        var leaf = new Gap.Man.Leaf { v = n + 3, s = "leaf" };
        a = new Gap.Man.Branch
        {
            left = leaf, inline = new Gap.Man.Leaf { v = n + 4, s = "inline" }, clip = clip, tag = n + 5,
            right = new Gap.Man.Branch { left = leaf, kids = generics ? new List<Gap.Man.INode> { new Gap.Man.Leaf { v = n + 6 }, null, new Gap.Man.Gen<int> { value = n + 7 }, new Gap.Man.Gen<string> { value = "g" } } : new List<Gap.Man.INode> { new Gap.Man.Leaf { v = n + 6 }, null, new GlobalNode { b = 3 } }, tag = n + 8 },
            kids = new List<Gap.Man.INode>(),
        };
        shared1 = x; shared2 = x; nullRef = null;
        arr = new Gap.Man.INode[] { new Gap.Man.Outer.Inner { f = 0.5f }, new Gap.Man.SNode { a = n + 9 }, null, new Gap.Man.EmptyNode(), new GlobalNode { on = true, b = 7 }, x };
        list = new List<Gap.Man.INode>();
        holder = new Gap.Man.Holder { node = x, x = n + 10 };
        holders = new[] { new Gap.Man.Holder { node = new Gap.Man.Leaf { v = n + 11 }, x = 1 }, new Gap.Man.Holder { node = null, x = 2 } };
        obj = new Gap.Man.Leaf { v = n + 12, s = "obj" }; concrete = new Gap.Man.Leaf { v = n + 13, s = "concrete" };
        var c = new Gap.Man.Branch { tag = n + 14 }; c.left = c; c.right = leaf; cyc = c;
    }
}
