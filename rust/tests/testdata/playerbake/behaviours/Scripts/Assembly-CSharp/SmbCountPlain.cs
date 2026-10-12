using System;
using System.Collections.Generic;
using UnityEngine;
// Nested callbacks under a behaviour that has none itself.
public class SmbCountPlain : StateMachineBehaviour
{
    public CntMid mid; public CntLeaf[] leaves; public CntStruct st; public CntDerived derived; [SerializeReference] public ICnt managed; public int tail;
    public void Set()
    {
        mid = new CntMid { leaf = new CntLeaf { v = 1 } }; leaves = new[] { new CntLeaf { v = 2 } }; st.v = 3; derived = new CntDerived { own = 4 };
        managed = new CntRef { leaf = new CntLeaf { v = 5 } }; tail = 6;
    }
}
// The usual dictionary kept as two lists.
[Serializable] public class SerDict : ISerializationCallbackReceiver
{
    public List<string> keys = new List<string>(); public List<int> values = new List<int>();
    Dictionary<string, int> map = new Dictionary<string, int>();
    public void Add(string key, int value) { map[key] = value; }
    public void OnBeforeSerialize() { keys.Clear(); values.Clear(); foreach (var pair in map) { keys.Add(pair.Key); values.Add(pair.Value); } }
    public void OnAfterDeserialize() { map = new Dictionary<string, int>(); for (int i = 0; i < Math.Min(keys.Count, values.Count); i++) map[keys[i]] = values[i]; }
}
