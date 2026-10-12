using System;
using System.Collections.Generic;
using UnityEngine;
// Managed references a document lacks: what a build writes for the field.
public interface IRNode { }
[Serializable] public class RLeaf : IRNode { public int v = 3; public string s = "leaf"; }
[Serializable] public class RBranch : IRNode { [SerializeReference] public IRNode child = new RLeaf { v = 8 }; [SerializeReference] public IRNode none; public int tag = 4; }
[Serializable] public class RHolder
{
    public int x = 1; [SerializeReference] public IRNode node = new RLeaf { v = 21 }; [SerializeReference] public IRNode empty;
    [SerializeReference] public List<IRNode> nodes = new List<IRNode> { new RLeaf { v = 22 }, null };
}
[Serializable] public class RHolderPlain { public int x; [SerializeReference] public IRNode node; [SerializeReference] public IRNode[] nodes; }
public class SmbRefNew : StateMachineBehaviour
{
    public int before = 1;
    [SerializeReference] public IRNode init = new RLeaf { v = 5 };
    [SerializeReference] public IRNode unset;
    [SerializeReference] public IRNode ctorSet;
    [SerializeReference] public IRNode sharedA; [SerializeReference] public IRNode sharedB;
    [SerializeReference] public IRNode branch = new RBranch();
    [SerializeReference] public IRNode[] arr = { new RLeaf { v = 6 }, null, new RLeaf { v = 7 } };
    [SerializeReference] public List<IRNode> list = new List<IRNode>();
    [SerializeReference] public List<IRNode> listNull;
    public RHolder holder; public RHolder holderInit = new RHolder { x = 2 }; public RHolder[] holders; public RHolderPlain plain;
    public int after = 2;
    public SmbRefNew() { ctorSet = new RLeaf { v = 9 }; var s = new RLeaf { v = 10 }; sharedA = s; sharedB = s; }
}
