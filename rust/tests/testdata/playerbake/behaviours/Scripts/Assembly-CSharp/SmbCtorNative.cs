using System;
using System.Collections.Generic;
using System.Linq;
using UnityEngine;
// A constructor that asks the engine: an Editor build runs it, a process without the engine cannot.
public class SmbCtorNative : StateMachineBehaviour
{
    public int a = 5; public AnimationCurve curve = AnimationCurve.Linear(0, 0, 1, 1); public int b = 6; public Gradient grad = new Gradient(); public int[] arr = { 1, 2 }; public byte tail = 9;
}
