using UnityEngine;
using UnityEngine.Events;
public class SmbEvent : StateMachineBehaviour { public UnityEvent evt; public UnityEvent<int> evtInt; public byte tail; }
