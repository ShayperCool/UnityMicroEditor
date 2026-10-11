using System.Collections.Generic;
using UnityEngine;
public class SoNode : SoBase { public SoNode next; public SoPack owner; public GameObject characterPrefab; public List<float> weightOverrides; public BoneRef sourceWeaponBone; }
