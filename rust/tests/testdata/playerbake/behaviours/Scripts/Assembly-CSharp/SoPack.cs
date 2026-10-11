using System;
using System.Collections.Generic;
using UnityEngine;
[Serializable] public struct BoneRef { public string name; public int index; public int depth; }
[Serializable] public class PackEntry { public string id; public MySO so; public AnimationClip clip; public Texture2D tex; public BoneRef bone; public List<float> weights; public SoNode node; }
public class SoPack : ScriptableObject
{
    public string title; public int count; public List<PackEntry> entries; public SoNode[] nodes; public SoNode first; public SoPack self; public GameObject prefab; public Material mat; public AnimationClip clip; public AudioClip audio;
    public Texture2D tex; public BoneRef bone; public Vector3 offset; public AnimationCurve curve; public SoBase baseRef; public ScriptableObject anySo; public UnityEngine.Object any; public Gap.Data.SoNamespaced spaced; public byte tail;
}
