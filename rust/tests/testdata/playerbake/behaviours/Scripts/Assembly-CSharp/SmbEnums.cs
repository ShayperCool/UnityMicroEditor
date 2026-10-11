using System;
using UnityEngine;
namespace Smb.Test.Enums
{
    public enum EInt { A, B, C = 10, Neg = -5 }
    public enum EByte : byte { A, B, Max = 255 }
    public enum ESByte : sbyte { A, B, Min = -128 }
    public enum EShort : short { A, B, Min = -32768 }
    public enum EUShort : ushort { A, B, Max = 65535 }
    public enum EUInt : uint { A, B, Max = 4294967295 }
    public enum ELong : long { A, B, Big = 5000000000, Neg = -5000000000 }
    public enum EULong : ulong { A, B, Big = 18446744073709551615 }
    [Flags] public enum EFlags { None = 0, X = 1, Y = 2, Z = 4 }
    public class SmbEnums : StateMachineBehaviour
    {
        public EInt ei; public EByte eb; public ESByte esb; public EShort es; public EUShort eus; public EUInt eu; public ELong el; public EULong eul; public EFlags ef;
        public EByte eb2; public bool after; public NestedEnum ne; public EByte[] ebs; public EInt[] eis; public System.DayOfWeek dow; public byte tail;
        public enum NestedEnum : short { P, Q }
    }
}
