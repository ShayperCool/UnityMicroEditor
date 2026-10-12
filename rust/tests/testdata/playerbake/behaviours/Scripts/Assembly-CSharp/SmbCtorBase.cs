using System;
using System.Collections.Generic;
using System.Linq;
using UnityEngine;
public class SmbCtorBase : StateMachineBehaviour
{
    public int baseCall = Twice(4); public string baseText; protected static int Twice(int x) { return x * 2; }
    public SmbCtorBase() { baseText = "base" + baseCall; }
}
