using System;
using System.Collections.Generic;
using UnityEngine;
// Counts how often a build runs each serialization callback, and in which order around nested objects.
[Serializable] public class CntLeaf : ISerializationCallbackReceiver
{
    public int before, after, v;
    public void OnBeforeSerialize() { before++; }
    public void OnAfterDeserialize() { after++; }
}
[Serializable] public class CntMid : ISerializationCallbackReceiver
{
    public int before, after; public CntLeaf leaf; public List<int> leafBeforeSeen = new List<int>(); public List<int> leafAfterSeen = new List<int>();
    public void OnBeforeSerialize() { before++; leafBeforeSeen.Add(leaf != null ? leaf.before : -1); }
    public void OnAfterDeserialize() { after++; leafAfterSeen.Add(leaf != null ? leaf.after : -1); }
}
[Serializable] public struct CntStruct : ISerializationCallbackReceiver
{
    public int before, after, v;
    public void OnBeforeSerialize() { before++; }
    public void OnAfterDeserialize() { after++; }
}
[Serializable] public class CntBase : ISerializationCallbackReceiver
{
    public int before, after;
    void ISerializationCallbackReceiver.OnBeforeSerialize() { before++; }
    void ISerializationCallbackReceiver.OnAfterDeserialize() { after++; }
}
[Serializable] public class CntDerived : CntBase { public int own; }
public interface ICnt { }
[Serializable] public class CntRef : ICnt, ISerializationCallbackReceiver
{
    public int before, after; public CntLeaf leaf; [SerializeReference] public ICnt next;
    public void OnBeforeSerialize() { before++; }
    public void OnAfterDeserialize() { after++; }
}
[Serializable] public class CntPlainRef : ICnt { public int v; public CntLeaf leaf; }
[Serializable] public class CntHolder { public int x; public CntLeaf leaf; public CntLeaf[] leaves; public CntStruct st; }
public class SmbCount : StateMachineBehaviour, ISerializationCallbackReceiver
{
    public int before, after; public List<int> midBeforeSeen = new List<int>(); public List<int> midAfterSeen = new List<int>();
    public CntMid mid; public CntMid[] mids; public List<CntLeaf> leaves; public CntStruct st; public CntStruct[] sts; public CntDerived derived; public CntHolder holder; public CntHolder[] holders;
    [SerializeReference] public ICnt managed; [SerializeReference] public List<ICnt> managedList; public int tail;
    public void OnBeforeSerialize() { before++; midBeforeSeen.Add(mid != null ? mid.before : -1); }
    public void OnAfterDeserialize() { after++; midAfterSeen.Add(mid != null ? mid.after : -1); }
    public void Set()
    {
        mid = new CntMid { leaf = new CntLeaf { v = 1 } }; mids = new[] { new CntMid { leaf = new CntLeaf { v = 2 } }, new CntMid() };
        leaves = new List<CntLeaf> { new CntLeaf { v = 3 }, new CntLeaf { v = 4 } }; st.v = 5; sts = new[] { new CntStruct { v = 6 } }; derived = new CntDerived { own = 7 };
        holder = new CntHolder { x = 8, leaf = new CntLeaf { v = 9 }, leaves = new[] { new CntLeaf { v = 10 } } }; holders = new[] { new CntHolder { x = 11 } };
        var shared = new CntRef { leaf = new CntLeaf { v = 12 } }; shared.next = new CntRef { next = shared };
        managed = shared; managedList = new List<ICnt> { new CntPlainRef { v = 13, leaf = new CntLeaf { v = 14 } }, null, shared }; tail = 15;
    }
}
