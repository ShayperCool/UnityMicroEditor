using System;
using System.Collections.Generic;
using UnityEngine;
public class SmbDictCb : StateMachineBehaviour, ISerializationCallbackReceiver
{
    public SerDict dict = new SerDict(); public SerDict[] dicts; public Vector3 dir; public Vector3 unit; public float length; public AnimationClip clip; public bool hasClip; public Color tint; public Color half;
    public string label; public string upper; public AnimationCurve curve; public int tail;
    public void OnBeforeSerialize() { upper = label == null ? "" : label.ToUpperInvariant(); }
    public void OnAfterDeserialize() { unit = dir.normalized; length = Mathf.Max(dir.magnitude, 0.5f); hasClip = clip != null; half = Color.Lerp(tint, Color.black, 0.5f); }
}
