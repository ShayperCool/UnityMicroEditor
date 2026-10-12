using System;
using System.Collections.Generic;
using UnityEngine;
public class SmbRefOne : StateMachineBehaviour
{
    public int a; [SerializeReference] public IRNode r = new RLeaf(); public int z;
}
