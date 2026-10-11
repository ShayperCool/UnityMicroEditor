using System;
using UnityEngine;
public abstract class SmbBaseA : StateMachineBehaviour { public int a1; [SerializeField] private int a2; private int a3; public string aStr; protected bool aProt; [SerializeField] protected bool aProtSer; }
public abstract class SmbBaseB<T> : SmbBaseA { public T generic; public int b1; [SerializeField] private T[] bArr; }
public class SmbDerived : SmbBaseB<float> { public int d1; public byte d2; }
