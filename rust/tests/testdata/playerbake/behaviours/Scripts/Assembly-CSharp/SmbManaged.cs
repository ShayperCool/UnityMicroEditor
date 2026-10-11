using System;
using System.Collections.Generic;
using UnityEngine;
public interface IShape { }
[Serializable] public class Circle : IShape { public float r; }
[Serializable] public class Box : IShape { public Vector2 size; public string label; }
public class SmbManaged : StateMachineBehaviour
{
    public int before; [SerializeReference] public IShape shape; [SerializeReference] public List<IShape> shapes; [SerializeReference] public IShape none; public int after;
}
