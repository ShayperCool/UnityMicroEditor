// Stand-in for UnityEngine.CoreModule: the classes the patch builder recognises at the top of a
// script's base chain. The fixtures run on Unity's standalone Mono, where the real module's
// native calls are not available.
using System;

namespace UnityEngine
{
    public class Object { }
    public class Component : Object { }
    public class Behaviour : Component { }
    public class MonoBehaviour : Behaviour
    {
        public void Invoke(string methodName, float time) { }
    }
    public class ScriptableObject : Object { }

    [AttributeUsage(AttributeTargets.Field)]
    public sealed class SerializeField : Attribute { }
}
