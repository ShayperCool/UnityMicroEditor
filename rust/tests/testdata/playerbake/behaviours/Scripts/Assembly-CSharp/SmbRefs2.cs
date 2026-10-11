using System;
using System.Collections.Generic;
using UnityEngine;
[Serializable] public class RefInner { public UnityEngine.Object any; public Material[] mats; public string label; }
[Serializable] public struct RefHolder { public AudioClip audio; public GameObject go; public MySO so; public RefInner inner; public List<RefInner> inners; }
public class SmbRefs2 : StateMachineBehaviour
{
    public RefHolder h; public RefHolder[] hs; public List<RefHolder> hl; public AudioClip audio; public Material mat; public MySO so; public OtherSO other; public GameObject go; public Font font; public byte tail;
}
