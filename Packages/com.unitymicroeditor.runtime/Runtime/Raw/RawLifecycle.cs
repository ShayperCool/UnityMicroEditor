using System;
using System.Collections.Generic;
using UnityEngine;

namespace Dungeon.CLIPlay.Raw
{
    [AttributeUsage(AttributeTargets.Assembly, AllowMultiple = false)]
    public sealed class RawLifecycleInstrumentedAttribute : Attribute
    {
        public int Version { get; private set; }
        public RawLifecycleInstrumentedAttribute(int version) { Version = version; }
    }

    [AttributeUsage(AttributeTargets.Assembly, AllowMultiple = false)]
    public sealed class RawScriptableObjectInstrumentedAttribute : Attribute
    {
        public int Version { get; private set; }
        public RawScriptableObjectInstrumentedAttribute(int version) { Version = version; }
    }

    /// <summary>Initialization barrier for source callbacks rewritten by the raw compiler.</summary>
    public static class RawLifecycle
    {
        public enum Phase { Awake, OnEnable, OnDisable, OnDestroy }
        private static RawLifecycleQueue<MonoBehaviour> queue;
        private static readonly List<RawLifecycleQueue<MonoBehaviour>> deferred = new List<RawLifecycleQueue<MonoBehaviour>>();
        private static readonly Dictionary<Type, int> executionOrders = new Dictionary<Type, int>();
        private static RawLifecycleQueue<ScriptableObject> scriptableObjects;

        public static bool ShouldCollect(ScriptableObject target, Phase phase)
        {
            if (scriptableObjects == null) return false;
            // CreateInstance enters Unity callbacks before it returns a handle
            // to the loader. The explicit scope owns these newly created data
            // objects and records them at their first callback.
            scriptableObjects.Register(target);
            return scriptableObjects.ShouldCollect(target, (int)phase);
        }

        public static void Enqueue(ScriptableObject target, Phase phase, Action callback, int order)
        {
            if (callback == null) throw new ArgumentNullException(nameof(callback));
            if (scriptableObjects == null) throw new InvalidOperationException("No raw ScriptableObject scope owns this callback.");
            scriptableObjects.Enqueue(target, (int)phase, callback, order);
        }

        internal static ScriptableObjectScope BeginScriptableObjects()
        {
            if (scriptableObjects != null) throw new InvalidOperationException("Nested raw ScriptableObject initialization is unsupported.");
            scriptableObjects = new RawLifecycleQueue<ScriptableObject>(target => target != null,
                target => target != null, target => target != null,
                (exception, target) => Debug.LogException(exception, target));
            return new ScriptableObjectScope(scriptableObjects);
        }

        internal sealed class ScriptableObjectScope : IDisposable
        {
            private readonly RawLifecycleQueue<ScriptableObject> owned;
            private bool complete;
            internal ScriptableObjectScope(RawLifecycleQueue<ScriptableObject> owned) { this.owned = owned; }
            public void Complete()
            {
                if (complete || !ReferenceEquals(scriptableObjects, owned))
                    throw new InvalidOperationException("Raw ScriptableObject scope is no longer active.");
                try { owned.Complete(); complete = true; }
                finally { scriptableObjects = null; }
            }
            public void Dispose() { if (ReferenceEquals(scriptableObjects, owned)) scriptableObjects = null; }
        }

        /// <summary>Registers the authored MonoImporter override for the exact script type.</summary>
        public static void SetExecutionOrder(Type type, int order)
        {
            if (type == null) throw new ArgumentNullException(nameof(type));
            if (!typeof(MonoBehaviour).IsAssignableFrom(type))
                throw new ArgumentException("Execution order must identify a MonoBehaviour type.", nameof(type));
            executionOrders[type] = order;
        }

        public static bool IsCollecting { get { return queue != null && queue.IsCollecting; } }
        public static bool ShouldCollect(MonoBehaviour target, Phase phase)
        {
            var owner = FindQueue(target);
            return owner != null && owner.ShouldCollect(target, (int)phase);
        }

        /// <summary>
        /// Source compatibility for Unity's native activation progress during
        /// scene initialization. The compiler rewrites this public property
        /// read; components outside the graph use the engine value unchanged.
        /// </summary>
        public static bool IsActiveAndEnabled(Behaviour target)
        {
            // Read the native property first, preserving its null/destroyed
            // object behavior and all actual activity/enable flags.
            bool native = target.isActiveAndEnabled;
            if (!native) return false;
            var script = target as MonoBehaviour;
            if (script == null) return native;
            var owner = FindQueue(script);
            bool awake;
            return owner == null || !owner.TryGetAwakeState(script, out awake) || awake;
        }

        /// <summary>Preserves delayed Awake when a source callback activates an object after scene construction.</summary>
        public static void SetActive(GameObject target, bool value)
        {
            bool wasActive = target.activeSelf;
            target.SetActive(value);
            if (!value || wasActive == value) return;
            if (target == null || queue == null && deferred.Count == 0) return;
            Transform activatedRoot = target.transform;
            Func<MonoBehaviour, bool> belongs = script => script != null &&
                activatedRoot != null && (script.gameObject == target || script.transform.IsChildOf(activatedRoot));
            if (queue != null && !queue.IsCollecting) queue.FlushActivated(belongs);
            if (deferred.Count == 0) return;
            // A disabled script receives no native OnEnable, so its consumed
            // Awake wrapper needs an explicit activation notification. The
            // queues already own the known components; no scene search occurs.
            var pending = deferred.ToArray();
            foreach (var owner in pending)
                if (!ReferenceEquals(owner, queue) && owner.HasPendingActiveAwake) Resume(owner, belongs);
            PruneDeferred();
        }

        public static void Enqueue(MonoBehaviour target, Phase phase, Action callback, int order)
        {
            if (callback == null) throw new ArgumentNullException(nameof(callback));
            var owner = FindQueue(target);
            if (owner == null) throw new InvalidOperationException("No raw lifecycle initialization barrier owns this callback.");
            // Public attribute metadata belongs to the compiler protocol; no
            // private fields or lifecycle methods are inspected.
            int effectiveOrder;
            var targetType = target.GetType();
            if (!executionOrders.TryGetValue(targetType, out effectiveOrder))
            {
                var attribute = Attribute.GetCustomAttribute(targetType, typeof(DefaultExecutionOrder), true) as DefaultExecutionOrder;
                effectiveOrder = attribute == null ? order : attribute.order;
            }
            owner.Enqueue(target, (int)phase, callback, effectiveOrder);
            if (owner.IsComplete && !ReferenceEquals(queue, owner)) Resume(owner, script => ReferenceEquals(script, target));
        }

        private static RawLifecycleQueue<MonoBehaviour> FindQueue(MonoBehaviour target)
        {
            if (queue != null && queue.ContainsRegisteredTarget(target)) return queue;
            foreach (var owner in deferred) if (owner.HasPendingAwakeFor(target)) return owner;
            return null;
        }

        private static void Resume(RawLifecycleQueue<MonoBehaviour> owner, Func<MonoBehaviour, bool> belongs)
        {
            var previous = queue;
            queue = owner;
            try { owner.FlushActivated(belongs); }
            finally { queue = previous; PruneDeferred(); }
        }

        private static void PruneDeferred()
        {
            for (int index = deferred.Count - 1; index >= 0; index--)
                if (!deferred[index].HasPendingAwake) deferred.RemoveAt(index);
        }

        internal static Scope Begin()
        {
            if (queue != null) throw new InvalidOperationException("Nested raw scene initialization is unsupported.");
            queue = new RawLifecycleQueue<MonoBehaviour>(target => target != null,
                target => target != null && target.gameObject.activeInHierarchy,
                target => target != null && target.enabled,
                (exception, target) => Debug.LogException(exception, target));
            return new Scope(queue);
        }

        internal static void Register(MonoBehaviour target) { if (queue != null) queue.Register(target); }

        internal sealed class Scope : IDisposable
        {
            private readonly RawLifecycleQueue<MonoBehaviour> owned;
            private bool complete;
            internal Scope(RawLifecycleQueue<MonoBehaviour> owned) { this.owned = owned; }
            public void Complete()
            {
                if (complete || !ReferenceEquals(queue, owned)) throw new InvalidOperationException("Raw lifecycle scope is no longer active.");
                try
                {
                    owned.Complete(); complete = true;
                    if (owned.HasPendingAwake) deferred.Add(owned);
                    PruneDeferred();
                }
                finally { queue = null; }
            }
            public void Dispose() { if (ReferenceEquals(queue, owned)) queue = null; }
        }
    }
}
