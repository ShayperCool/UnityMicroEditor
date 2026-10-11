using System;
using System.Collections.Generic;
using UnityEngine;
using Smb.Test;
using Smb.Test.Enums;
public class SmbArrays : StateMachineBehaviour
{
    public int[] ints; public List<int> intList; public string[] strs; public List<string> strList; public float[] floats; public bool[] bools;
    public byte[] bytes; public List<byte> byteList; public sbyte[] sbytes; public char[] chars; public short[] shorts; public ushort[] ushorts; public long[] longs; public ulong[] ulongs; public double[] doubles; public uint[] uints;
    public Vector3[] vecs; public Color32[] cols; public List<Color> colList; public EInt[] enums; public List<EByte> benums; public AnimationCurve[] curves; public Gradient[] grads;
    public Inner[] structs; public List<Outer> classes; public LayerMask[] masks; public Quaternion[] quats;
    public int[][] jag; public List<List<int>> nestedList; public int[,] multi; public List<int[]> listOfArrays; public Generic<int[]> genArr;
    public AnimationClip[] clips; public List<GameObject> gos; public byte tail;
}
