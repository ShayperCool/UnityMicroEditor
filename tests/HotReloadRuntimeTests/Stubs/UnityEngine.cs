// Stand-in for UnityEngine.CoreModule on plain Mono: exactly what the applier's PlayerHost, the bridge's
// CodeReload.cs and its HotReloadAttributes.cs use, with the engine's observable behaviour. The log is recorded
// so the harness can read what the applier reported.
using System;
using System.Collections.Generic;

namespace UnityEngine
{
    public enum FindObjectsInactive { Exclude, Include }
    public enum FindObjectsSortMode { None, InstanceID }

    public class Object
    {
        private static readonly List<Object> live = new List<Object>();
        private bool destroyed;

        public Object() { lock (live) live.Add(this); }

        private static readonly List<Object> doomed = new List<Object>();

        // As in the engine: the object exists until the end of the frame, and compares
        // equal to null only then. DestroyImmediate removes it at once.
        public static void Destroy(Object value) { lock (live) doomed.Add(value); }

        public static void DestroyImmediate(Object value)
        {
            value.destroyed = true;
            lock (live) live.Remove(value);
        }

        /// What the engine does between two frames with the objects passed to Destroy.
        public static void EndOfFrame()
        {
            Object[] due;
            lock (live) { due = doomed.ToArray(); doomed.Clear(); }
            foreach (Object value in due) DestroyImmediate(value);
        }

        public static Object[] FindObjectsByType(Type type, FindObjectsInactive findObjectsInactive, FindObjectsSortMode sortMode)
        {
            var found = new List<Object>();
            lock (live) foreach (Object value in live) if (type.IsInstanceOfType(value)) found.Add(value);
            return found.ToArray();
        }

        // A destroyed object compares equal to null, as in the engine.
        public static bool operator ==(Object left, Object right)
        {
            bool leftGone = ReferenceEquals(left, null) || left.destroyed, rightGone = ReferenceEquals(right, null) || right.destroyed;
            return leftGone || rightGone ? leftGone == rightGone : ReferenceEquals(left, right);
        }

        public static bool operator !=(Object left, Object right) { return !(left == right); }
        public override bool Equals(object other) { return this == other as Object; }
        public override int GetHashCode() { return base.GetHashCode(); }
    }

    public class MonoBehaviour : Object { }

    public static class Debug
    {
        public static readonly List<Exception> Exceptions = new List<Exception>();
        public static readonly List<Object> Contexts = new List<Object>();
        public static readonly List<string> Warnings = new List<string>();
        public static readonly List<string> Logs = new List<string>();

        public static void Log(object message)
        {
            lock (Logs) Logs.Add(Convert.ToString(message));
        }

        public static void LogException(Exception exception, Object context)
        {
            lock (Exceptions) { Exceptions.Add(exception); Contexts.Add(context); }
        }

        public static void LogWarning(object message)
        {
            lock (Warnings) Warnings.Add(Convert.ToString(message));
        }
    }
}

namespace UnityEngine.Scripting
{
    public class PreserveAttribute : Attribute { }
}
