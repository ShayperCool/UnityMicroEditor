using System.Collections.Generic;
using UnityEngine;
using Smb.Test;
using Smb.Test.Enums;
public class SmbHidden : StateMachineBehaviour
{
    [HideInInspector] public Inner hStruct; [HideInInspector] public int[] hArr; [HideInInspector] public string hStr; [HideInInspector] public bool hBool; [HideInInspector] public EByte hEnum;
    [HideInInspector] public AnimationCurve hCurve; [HideInInspector] public AnimationClip hClip; [HideInInspector] public List<Inner> hList; [HideInInspector] public Vector3 hVec; [HideInInspector] public char hChar;
    [HideInInspector] public AnimationClip[] hClips; [HideInInspector] public string[] hStrs; [HideInInspector] public Color32 hC32; [HideInInspector] [SerializeField] private byte hPriv;
    [Range(0, 1)] public float ranged; [Tooltip("tip")] public int tipped; [Header("head")] public int headed; [Multiline] public string multi; [TextArea] public string area; [ColorUsage(true, true)] public Color hdr;
    [Min(1)] public int min; [Delayed] public int delayed; [NonReorderable] public int[] nonReorder; [GradientUsage(true)] public Gradient gradUsage; [Space] public int spaced; [ContextMenuItem("a", "b")] public int ctx;
    void b() { }
}
