using System;
using System.Collections.Generic;
using UnityEngine;
// What a callback finds in fields the document lacks: bit set = the field was null.
[Serializable] public class NullPlain { public int x; public string s; public List<int> l; public NullPlain2 deep; }
[Serializable] public class NullPlain2 { public int y; }
[Serializable] public class NullInner : ISerializationCallbackReceiver
{
    public List<int> list; public int[] arr; public string str; public NullPlain plain; public AnimationCurve curve; public Gradient grad; public NullPlain[] plains; public List<string> strs;
    public int afterNulls = -1; public int firstBefore = -1; public int lastBefore = -1; public int deepAfter = -1; public int deepBefore = -1;
    int Nulls() { return (list == null ? 1 : 0) | (arr == null ? 2 : 0) | (str == null ? 4 : 0) | (plain == null ? 8 : 0) | (curve == null ? 16 : 0) | (grad == null ? 32 : 0) | (plains == null ? 64 : 0) | (strs == null ? 128 : 0); }
    int Deep() { return plain == null ? -2 : (plain.s == null ? 1 : 0) | (plain.l == null ? 2 : 0) | (plain.deep == null ? 4 : 0); }
    public void OnBeforeSerialize() { if (firstBefore < 0) { firstBefore = Nulls(); deepBefore = Deep(); } lastBefore = Nulls(); }
    public void OnAfterDeserialize() { afterNulls = Nulls(); deepAfter = Deep(); }
}
public class SmbNulls : StateMachineBehaviour, ISerializationCallbackReceiver
{
    public List<int> list; public int[] arr; public string str; public NullPlain plain; public AnimationCurve curve; public Gradient grad; public NullPlain[] plains; public List<string> strs;
    public NullInner inner; public NullInner[] inners; public AnimationClip clip; public RectOffset ro;
    public int afterNulls = -1; public int firstBefore = -1; public int lastBefore = -1; public int deepAfter = -1; public int deepBefore = -1; public int innerAfter = -1; public int tail;
    int Nulls() { return (list == null ? 1 : 0) | (arr == null ? 2 : 0) | (str == null ? 4 : 0) | (plain == null ? 8 : 0) | (curve == null ? 16 : 0) | (grad == null ? 32 : 0) | (plains == null ? 64 : 0) | (strs == null ? 128 : 0) | (inner == null ? 256 : 0) | (inners == null ? 512 : 0) | (ro == null ? 1024 : 0); }
    int Deep() { return plain == null ? -2 : (plain.s == null ? 1 : 0) | (plain.l == null ? 2 : 0) | (plain.deep == null ? 4 : 0); }
    public void OnBeforeSerialize() { if (firstBefore < 0) { firstBefore = Nulls(); deepBefore = Deep(); } lastBefore = Nulls(); }
    public void OnAfterDeserialize() { afterNulls = Nulls(); deepAfter = Deep(); innerAfter = inner == null ? -2 : inner.afterNulls; }
}
