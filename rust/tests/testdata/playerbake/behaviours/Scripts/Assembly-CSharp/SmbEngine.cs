using System;
using System.Collections.Generic;
using UnityEngine;
using Unity.Mathematics;
// Engine types as fields: the ones the engine stores in a form of its own and a choice of those it stores by their C# fields.
public class SmbEngine : StateMachineBehaviour
{
    public GUID guid; public GUID[] guids; public PropertyName prop; public PropertyName[] props; public GUIStyle style; public GUIStyle[] styles; public GUIStyleState state;
    public Pose pose; public BoneWeight bone; public float3 f3; public quaternion q; public float4x4 m; public half2 h2; public int2[] cells; public bool3 b3;
    public ParticleSystem.MinMaxCurve curve; public ParticleSystem.MinMaxGradient gradient; public GUIContent content; public RangeInt range;
    public UnityEngine.SceneManagement.LoadSceneParameters load; public ContactFilter2D filter; public UnityEngine.TextCore.GlyphRect glyph;
    public UnityEngine.Random.State rnd; public GUISettings gui; public List<Pose> poses; public byte tail;
}
