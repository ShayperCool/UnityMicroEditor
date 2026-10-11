using System;
using UnityEngine;
namespace Second.Deep.Ns
{
    public enum SecondEnum : byte { Zero, One, Two }
    [Serializable] public unsafe struct Fixed { public fixed int vals[4]; public fixed byte bytes[3]; public fixed float fl[2]; public fixed bool bl[3]; public fixed char ch[2]; public fixed short sh[3]; public fixed long lg[2]; public fixed double db[1]; public int after; }
    [Serializable] public struct SecondData { public int id; public string label; }
    public class SmbSecond : StateMachineBehaviour { public Fixed fx; public string label; public SecondEnum e; public SecondData data; public bool tail; }
}
