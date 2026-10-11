using UnityEngine;
public class SmbKnownDerived : SmbKnownBase { public int own = 8; public float ownF = 0.25f; public bool ownOn = true; public string ownS = "own"; public Known init; public Known[] inits; public SmbKnownDerived() { baseA = 30; ownF = 0.5f; } }
