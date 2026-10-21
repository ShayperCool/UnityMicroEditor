using System;
using System.Reflection;
using UnityEngine;
using Object = UnityEngine.Object;

namespace UnityMicroEditor
{
    /// <summary>The engine APIs the bridge needs whose form differs between the supported Unity releases.</summary>
    /// <remarks>6000.6 has the assembly API of <c>UnityEngine.Assemblies</c> and <c>EntityId</c>; 6000.0 has neither.
    /// The tool compiles the bridge with the <c>UNITY_*_OR_NEWER</c> symbols of the host's release, as the Editor does.</remarks>
    internal static class EngineCompat
    {
        /// <summary>The managed assemblies loaded in the Player.</summary>
        internal static System.Collections.Generic.IReadOnlyList<Assembly> LoadedAssemblies()
        {
#if UNITY_6000_6_OR_NEWER
            return UnityEngine.Assemblies.CurrentAssemblies.GetLoadedAssemblies();
#else
            return AppDomain.CurrentDomain.GetAssemblies();
#endif
        }

        /// <summary>Loads a managed assembly from a file into the Player.</summary>
        internal static void LoadFromPath(string path)
        {
#if UNITY_6000_6_OR_NEWER
            UnityEngine.Assemblies.CurrentAssemblies.LoadFromPath(path);
#else
            Assembly.LoadFrom(path);
#endif
        }

        /// <summary>The identity of a live object, as text.</summary>
        internal static string Identity(Object value)
        {
#if UNITY_6000_6_OR_NEWER
            return value.GetEntityId().ToString();
#else
            return value.GetInstanceID().ToString(System.Globalization.CultureInfo.InvariantCulture);
#endif
        }

        /// <summary>Every object of type <typeparamref name="T"/>, inactive ones included, in no order.</summary>
        internal static T[] FindAll<T>() where T : Object
        {
#if UNITY_6000_6_OR_NEWER
            return Object.FindObjectsByType<T>(FindObjectsInactive.Include);
#else
            return Object.FindObjectsByType<T>(FindObjectsInactive.Include, FindObjectsSortMode.None);
#endif
        }
    }
}
