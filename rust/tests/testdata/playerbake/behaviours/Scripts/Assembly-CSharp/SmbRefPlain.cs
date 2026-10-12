using System;
using System.Collections.Generic;
using UnityEngine;
// The plain case: a class that gained reference fields, none of which a constructor sets, and one that it sets.
public class SmbRefPlain : StateMachineBehaviour
{
    public int a; [SerializeReference] public IRNode r; [SerializeReference] public List<IRNode> rs; [SerializeReference] public IRNode[] ra; public RHolderPlain plain; public RHolderPlain[] plains; public int z;
}
