using System;
using System.Reflection;
using UnityEngine.Scripting;

namespace UnityMicroEditor
{
    /// <summary>
    /// Runs the method after every code patch applied to the running Player, on the main thread.
    /// A static method runs once, an instance method of a UnityEngine.Object type on every live object.
    /// It takes no parameter or one List&lt;MethodPatch&gt;.
    /// </summary>
    [Preserve]
    [AttributeUsage(AttributeTargets.Method)]
    public sealed class InvokeOnHotReloadAttribute : Attribute { }

    /// <summary>Runs the parameterless method itself, with its new body, after a code patch changed it.</summary>
    [Preserve]
    [AttributeUsage(AttributeTargets.Method)]
    public sealed class InvokeOnHotReloadLocalAttribute : Attribute { }

    /// <summary>One method whose running body a code patch changed.</summary>
    [Preserve]
    public sealed class MethodPatch
    {
        /// <summary>The method of the assembly the Player loaded.</summary>
        public MethodBase originalMethod;
        /// <summary>What ran before this patch: the original or the replacement of an earlier patch.</summary>
        public MethodBase previousMethod;
        /// <summary>What runs now. The original again when the edit was reverted.</summary>
        public MethodBase newMethod;
    }
}
