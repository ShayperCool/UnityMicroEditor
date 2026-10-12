using System;
using System.Collections.Generic;
using UnityEngine;
using UnityEngine.Events;
// What games do with the callbacks: a dictionary that is its own class, a wrapper that upgrades old data, an event.
[Serializable] public class SerDictBase<TKey, TValue> : Dictionary<TKey, TValue>, ISerializationCallbackReceiver
{
    [SerializeField] List<TKey> keys = new List<TKey>(); [SerializeField] List<TValue> values = new List<TValue>();
    public void OnBeforeSerialize() { keys.Clear(); values.Clear(); foreach (var pair in this) { keys.Add(pair.Key); values.Add(pair.Value); } }
    public void OnAfterDeserialize() { Clear(); for (int i = 0; i < Math.Min(keys.Count, values.Count); i++) this[keys[i]] = values[i]; }
}
[Serializable] public class StringIntDict : SerDictBase<string, int> { }
[Serializable] public class GradWrap : ISerializationCallbackReceiver
{
    public Gradient gradient; public AnimationCurve curve; public Color first; public int version; public List<float> times; public AnimationClip clip; public bool hasClip; public bool hasGradient;
    public void OnAfterDeserialize() { if (version < 2) { version = 2; first = Color.white; } hasClip = clip != null; hasGradient = gradient != null; }
    public void OnBeforeSerialize() { if (times == null) times = new List<float>(); if (times.Count == 0) times.Add(0.5f); }
}
public class SmbWrap : StateMachineBehaviour
{
    public StringIntDict map; public StringIntDict[] maps; public GradWrap wrap; public GradWrap wrapNew = new GradWrap(); public UnityEvent evt; public AnimationClip clip; public int tail;
}
