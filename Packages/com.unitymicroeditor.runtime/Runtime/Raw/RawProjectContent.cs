using System;
using System.Collections.Generic;
using System.IO;
using UnityEngine;
using UnityEngine.SceneManagement;

namespace Dungeon.CLIPlay.Raw
{
    /// <summary>Public source-content adapter for project composition roots.</summary>
    public static class RawProjectContent
    {
        [Serializable] private sealed class Routes
        {
            public int schemaVersion;
            public string project;
            public Route[] scenes;
        }
        [Serializable] private sealed class Route
        {
            public string guid;
            public string manifest;
            public string sha256;
        }

        private static RawSceneLoader.LiveSession session;
        private static readonly Dictionary<string, Route> routes = new Dictionary<string, Route>(StringComparer.Ordinal);
        private static readonly Dictionary<string, Scene> loaded = new Dictionary<string, Scene>(StringComparer.Ordinal);

        internal static void Initialize(string path, RawSceneLoader.LiveSession owner, string project)
        {
            if (session != null) throw new InvalidOperationException("A raw content session is already bound.");
            if (!Path.IsPathRooted(path)) throw new InvalidDataException("Content routes require an absolute path.");
            var value = JsonUtility.FromJson<Routes>(File.ReadAllText(path));
            if (value == null || value.schemaVersion != 1 || value.project != project || value.scenes == null)
                throw new InvalidDataException("Raw content routes do not match this project.");
            var validated = new Dictionary<string, Route>(StringComparer.Ordinal);
            foreach (Route route in value.scenes)
            {
                if (route == null || !Hex(route.guid, 32) || !Hex(route.sha256, 64) || !Path.IsPathRooted(route.manifest)
                    || validated.ContainsKey(route.guid)) throw new InvalidDataException("Invalid or duplicate raw content scene route.");
                validated.Add(route.guid, route);
            }
            routes.Clear(); loaded.Clear();
            foreach (var entry in validated) routes.Add(entry.Key, entry.Value);
            session = owner;
        }

        public static bool TryLoadScene(string guid, out Scene scene)
        {
            scene = default;
            if (session == null || !routes.TryGetValue(guid ?? "", out Route route)) return false;
            if (loaded.TryGetValue(guid, out scene) && scene.IsValid() && scene.isLoaded) return true;
            scene = session.LoadAdditive(route.manifest, route.sha256);
            loaded[guid] = scene;
            return true;
        }

        public static bool TryLoadPrefab(string guid, out GameObject prefab)
        {
            prefab = null;
            return session != null && session.TryGetPrefab(guid, out prefab);
        }

        public static AsyncOperation UnloadScene(Scene scene)
        {
            if (session == null) throw new InvalidOperationException("Raw content session is unavailable.");
            string key = null;
            foreach (var entry in loaded) if (entry.Value.handle == scene.handle) { key = entry.Key; break; }
            if (key == null)
            {
                if (session.TryGetPendingUnload(scene, out AsyncOperation pending)) return pending;
                throw new InvalidOperationException("Scene is not a registered raw content scene.");
            }
            // Remove the route before OnDisable/OnDestroy can request its unload.
            loaded.Remove(key);
            AsyncOperation operation = session.UnloadAdditive(scene);
            return operation;
        }

        internal static void Release(RawSceneLoader.LiveSession owner)
        {
            if (session != owner) return;
            session = null; routes.Clear(); loaded.Clear();
        }

        internal static void ForgetLoadedScenes(RawSceneLoader.LiveSession owner)
        {
            if (session == owner) loaded.Clear();
        }

        private static bool Hex(string value, int length)
        {
            if (value == null || value.Length != length) return false;
            foreach (char c in value) if ((c < '0' || c > '9') && (c < 'a' || c > 'f')) return false;
            return true;
        }
    }
}
