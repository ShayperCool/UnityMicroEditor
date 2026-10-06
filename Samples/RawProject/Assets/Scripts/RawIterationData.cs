using UnityEngine;

public sealed class RawIterationData : ScriptableObject
{
    [SerializeField] private int revision;
    [SerializeField] private Color tint;
    public int Revision => revision;
    public Color Tint => tint;
}
