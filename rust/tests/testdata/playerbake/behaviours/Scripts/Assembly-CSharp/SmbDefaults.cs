using System;
using System.Collections.Generic;
using UnityEngine;
using Smb.Test;
using Smb.Test.Enums;
using UnityEngine.Serialization;
public class SmbDefaults : StateMachineBehaviour
{
    public int dfInt = 5; public float dfFloat = 1.5f; public string dfStr = "hello"; public int[] dfArr = { 1, 2, 3 };
    public AnimationCurve dfCurve = AnimationCurve.Linear(0, 0, 1, 1); public Inner dfInner = new Inner { a = 7, name = "seven" }; public Outer dfOuter; public Outer dfOuterInit = new Outer { w = 2.5f };
    public Color dfCol = Color.red; public bool dfOn = true; public Gradient dfGrad; public EInt dfEnum = EInt.C; public List<string> dfNames = new List<string> { "a", "bc" };
    public string dfNullStr; public AnimationCurve dfNullCurve; public Vector3 dfVec = new Vector3(1, 2, 3); public Quaternion dfQuat; public LayerMask dfMask = 3; public Color32 dfC32 = new Color32(1, 2, 3, 4);
    [FormerlySerializedAs("dfOldName")] public int dfRenamed = 9; public double dfDouble = 2.5; public char dfChar = 'x'; public long dfLong = -7; public AnimationClip dfClip; public int[] dfNullArr; public byte dfTail = 200;
}
