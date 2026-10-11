using System;
using System.Collections.Generic;
using UnityEngine;
[Serializable] public class CallbackInner : ISerializationCallbackReceiver
{
    public int k; public List<int> keys = new List<int>();
    public void OnBeforeSerialize() { k = 77; keys.Add(7); }
    public void OnAfterDeserialize() { }
}
// A build runs the callbacks, so what it writes is not what the text holds.
public class SmbCallback : StateMachineBehaviour, ISerializationCallbackReceiver
{
    public int n; public int loaded; public CallbackInner inner; public byte tail;
    public void OnBeforeSerialize() { n = 99; }
    public void OnAfterDeserialize() { loaded += 1000; }
}
