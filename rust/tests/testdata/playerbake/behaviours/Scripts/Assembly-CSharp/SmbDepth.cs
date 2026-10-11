using System;
using System.Collections.Generic;
using UnityEngine;
[Serializable] public class Chain { public int v; public Chain next; }
[Serializable] public class Tree { public int v; public List<Tree> kids; }
public class SmbDepth : StateMachineBehaviour { public Chain chain; public Tree tree; public byte tail; }
