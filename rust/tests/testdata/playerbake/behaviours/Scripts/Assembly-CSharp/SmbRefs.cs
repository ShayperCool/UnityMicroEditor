using System.Collections.Generic;
using UnityEngine;
public interface IThing { }
public class SmbRefs : StateMachineBehaviour
{
    public AnimationClip clip; public GameObject go; public MySO so; public Material mat; public Object any; public ScriptableObject sobase; public Transform tr; public Texture2D tex;
    public HelperMono mono; public Motion motion; public RuntimeAnimatorController rac; public Avatar avatar; public AvatarMask avatarMask; public Component comp; public IThing iface;
    public StateMachineBehaviour smb; public SmbRefs self; public MonoBehaviour mb; public Shader shader; public TextAsset text; public Texture texBase; public Mesh mesh; public Sprite sprite;
    public MySO[] sos; public List<Material> mats; public byte tail;
}
