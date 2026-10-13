using System;
using System.Collections.Generic;
using System.Reflection;
using System.Runtime.CompilerServices;

namespace UnityMicroEditor.HotReload
{
    /// <summary>One method whose running body changed with the applied generation.</summary>
    internal sealed class Change
    {
        internal MethodBase Original;   // null for a method the patch added
        internal MethodBase Previous;   // what ran before: the original or an older replacement
        internal MethodBase Current;    // what runs now; the original again after a restore
        internal bool Removed;          // redirected to an empty stub, reported to nobody
    }

    // The applier reaches the Player only through this type: the engine log, the query for live
    // objects and the bridge's MethodPatch. Redirects, the field store and the request handling
    // need none of it, so they also run where neither assembly exists (plain Mono).
    internal abstract class Host
    {
        internal abstract void Warn(string message);
        internal abstract void Report(Exception error, object context);
        /// <summary>Whether live instances of the type can be found.</summary>
        internal abstract bool Enumerable(Type type);
        internal abstract object[] Instances(Type type);
        /// <summary>Whether an object that Instances returned still exists.</summary>
        internal abstract bool Alive(object instance);
        /// <summary>Whether a callback parameter of this type receives the list of patched methods.</summary>
        internal abstract bool TakesPatches(Type parameter);
        internal abstract object Patches(List<Change> changes);

        internal static Host Create()
        {
            try { return Player(); }
            catch (Exception) { return new PlainHost(); }
        }

        // Compiled on its own, so a missing engine or bridge assembly fails here and not in the caller.
        [MethodImpl(MethodImplOptions.NoInlining)]
        private static Host Player() { return new PlayerHost(); }
    }

    internal sealed class PlayerHost : Host
    {
        private readonly Type objects, patches;

        internal PlayerHost()
        {
            objects = typeof(UnityEngine.Object);
            patches = typeof(List<UnityMicroEditor.MethodPatch>);
        }

        internal override void Warn(string message) { UnityEngine.Debug.LogWarning("[unity-cli-play] code-reload " + message); }

        internal override void Report(Exception error, object context)
        {
            UnityEngine.Debug.LogException(error, context as UnityEngine.Object);
        }

        internal override bool Enumerable(Type type) { return objects.IsAssignableFrom(type); }

        internal override object[] Instances(Type type)
        {
            // Inactive objects are live too; a destroyed one compares equal to null.
            var live = new List<object>();
            foreach (UnityEngine.Object value in UnityEngine.Object.FindObjectsByType(type, UnityEngine.FindObjectsInactive.Include))
                if (value != null) live.Add(value);
            return live.ToArray();
        }

        internal override bool Alive(object instance) { return !(instance is UnityEngine.Object value) || value != null; }

        internal override bool TakesPatches(Type parameter) { return parameter == patches; }

        internal override object Patches(List<Change> changes)
        {
            var list = new List<UnityMicroEditor.MethodPatch>(changes.Count);
            foreach (Change change in changes)
                if (!change.Removed)
                    list.Add(new UnityMicroEditor.MethodPatch { originalMethod = change.Original, previousMethod = change.Previous, newMethod = change.Current });
            return list;
        }
    }

    internal sealed class PlainHost : Host
    {
        internal override void Warn(string message) { Console.Error.WriteLine("code-reload " + message); }
        internal override void Report(Exception error, object context) { Console.Error.WriteLine("code-reload callback failed: " + error); }
        internal override bool Enumerable(Type type) { return false; }
        internal override object[] Instances(Type type) { return new object[0]; }
        internal override bool Alive(object instance) { return true; }
        internal override bool TakesPatches(Type parameter) { return false; }
        internal override object Patches(List<Change> changes) { return null; }
    }
}
