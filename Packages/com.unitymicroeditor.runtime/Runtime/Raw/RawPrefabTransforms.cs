using System;
using System.Collections.Generic;
using UnityEngine;

namespace Dungeon.CLIPlay.Raw
{
    /// <summary>Authored prefab hierarchy queries for inactive raw templates.</summary>
    public static class RawPrefabTransforms
    {
        private static readonly Dictionary<Transform, Transform> roots = new Dictionary<Transform, Transform>();

        public static Transform Root(Transform target)
        {
            // Access the native property first to preserve Unity's behavior for
            // null and destroyed receivers. Native clones never enter this map.
            Transform nativeRoot = target.root;
            return roots.TryGetValue(target, out Transform root) && root != null ? root : nativeRoot;
        }

        internal static void Register(Transform target, Transform root)
        {
            if (target == null || root == null) throw new ArgumentNullException("Raw prefab hierarchy registration is null.");
            if (roots.ContainsKey(target)) throw new InvalidOperationException("Raw prefab Transform was registered twice.");
            roots.Add(target, root);
        }

        internal static void Unregister(Transform[] targets)
        {
            if (targets == null) return;
            foreach (Transform target in targets)
                if (!ReferenceEquals(target, null)) roots.Remove(target);
        }

        public static Component GetComponentInChildren(Component target, Type type)
        { return GetComponentInChildren(target.gameObject, type, false); }

        public static Component GetComponentInChildren(Component target, Type type, bool includeInactive)
        { return GetComponentInChildren(target.gameObject, type, includeInactive); }

        public static Component GetComponentInChildren(GameObject target, Type type)
        { return GetComponentInChildren(target, type, false); }

        public static Component GetComponentInChildren(GameObject target, Type type, bool includeInactive)
        {
            Transform start = target.transform;
            if (!roots.TryGetValue(start, out Transform root) || root == null)
                return target.GetComponentInChildren(type, includeInactive);
            // Unity always checks the query object, including an inactive root.
            Component component = target.GetComponent(type);
            return component != null ? component : SearchChildren(start, root, type, includeInactive);
        }

        public static T GetComponentInChildren<T>(Component target)
        { return GetComponentInChildren<T>(target.gameObject, false); }

        public static T GetComponentInChildren<T>(Component target, bool includeInactive)
        { return GetComponentInChildren<T>(target.gameObject, includeInactive); }

        public static T GetComponentInChildren<T>(GameObject target)
        { return GetComponentInChildren<T>(target, false); }

        public static T GetComponentInChildren<T>(GameObject target, bool includeInactive)
        {
            if (!roots.ContainsKey(target.transform)) return target.GetComponentInChildren<T>(includeInactive);
            Component component = GetComponentInChildren(target, typeof(T), includeInactive);
            return (T)(object)component;
        }

        private static Component SearchChildren(Transform parent, Transform root, Type type, bool includeInactive)
        {
            for (int index = 0; index < parent.childCount; index++)
            {
                Transform child = parent.GetChild(index);
                if (!includeInactive && !AuthoredActive(child, root)) continue;
                Component component = child.gameObject.GetComponent(type);
                if (component != null) return component;
                component = SearchChildren(child, root, type, includeInactive);
                if (component != null) return component;
            }
            return null;
        }

        private static bool AuthoredActive(Transform target, Transform root)
        {
            for (Transform current = target; current != null; current = current.parent)
            {
                if (!current.gameObject.activeSelf) return false;
                if (current == root) return true;
            }
            return false;
        }
    }
}
