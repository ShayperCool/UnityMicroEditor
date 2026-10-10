using System;
using System.Collections.Generic;
using System.IO;
using UnityEngine;
using UnityEngine.SceneManagement;

namespace UnityMicroEditor.Raw
{
    /// <summary>Public source-content adapter for project composition roots.</summary>
    public static class RawProjectContent
    {
        [Serializable] private sealed class Routes
        {
            public int schemaVersion;
            public string project;
            public Route[] scenes;
            public Address[] addressables;
        }
        [Serializable] private sealed class Route
        {
            public string guid;
            public string manifest;
            public string sha256;
        }

        /// <summary>An Addressables key the session serves from an authored source.</summary>
        [Serializable] internal sealed class Address
        {
            public string guid;
            public string address;
            public string[] labels;
            public string path;
            public bool scene;
        }

        private static RawSceneLoader.LiveSession session;
        // Scenes another owner loaded and unloads; their content is ours.
        private static readonly HashSet<Scene> adopted = new HashSet<Scene>();
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
            foreach (Address address in value.addressables ?? Array.Empty<Address>())
                if (address == null || !Hex(address.guid, 32) || string.IsNullOrEmpty(address.address) || string.IsNullOrEmpty(address.path)
                    || address.scene && !validated.ContainsKey(address.guid)) throw new InvalidDataException("Invalid addressable content route.");
            routes.Clear(); loaded.Clear(); adopted.Clear();
            foreach (var entry in validated) routes.Add(entry.Key, entry.Value);
            session = owner;
            SceneManager.sceneUnloaded += ReleaseAdopted;
#if UNITY_MICRO_EDITOR_ADDRESSABLES
            try { RawAddressables.Install(value.addressables ?? Array.Empty<Address>()); }
            catch { Release(owner); throw; }
#endif
        }

        /// <summary>Builds a registered scene into an empty scene its caller loaded.</summary>
        internal static void LoadSceneInto(string guid, Scene target)
        {
            if (session == null || !routes.TryGetValue(guid ?? "", out Route route))
                throw new InvalidOperationException("Scene is not a registered raw content scene: " + guid);
            session.LoadAdditive(route.manifest, route.sha256, target);
            adopted.Add(target);
        }

        private static void ReleaseAdopted(Scene scene)
        {
            if (session != null && adopted.Remove(scene)) session.UnloadAdditive(scene);
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
#if UNITY_MICRO_EDITOR_ADDRESSABLES
            RawAddressables.Uninstall();
#endif
            SceneManager.sceneUnloaded -= ReleaseAdopted;
            session = null; routes.Clear(); loaded.Clear(); adopted.Clear();
        }

        internal static void ForgetLoadedScenes(RawSceneLoader.LiveSession owner)
        {
            if (session == owner) { loaded.Clear(); adopted.Clear(); }
        }

        private static bool Hex(string value, int length)
        {
            if (value == null || value.Length != length) return false;
            foreach (char c in value) if ((c < '0' || c > '9') && (c < 'a' || c > 'f')) return false;
            return true;
        }
    }
}
