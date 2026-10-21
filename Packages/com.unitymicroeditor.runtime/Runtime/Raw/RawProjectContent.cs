using System;
using System.Collections.Generic;
using System.IO;
using UnityEngine;
using UnityEngine.SceneManagement;
using Object = UnityEngine.Object;

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
            public BuildScene[] buildScenes;
        }
        /// <summary>A scene of the project's Build Settings; the host has an empty scene of its name at its build index.</summary>
        [Serializable] private sealed class BuildScene
        {
            public int index;
            public string guid;
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
        // The authored scene of each build index the host stands in for.
        private static readonly Dictionary<int, string> buildScenes = new Dictionary<int, string>();
        // Where the host build keeps its stand-in scenes.
        private const string StandInFolder = "/BuildScenes/";

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
            var standIns = new Dictionary<int, string>();
            foreach (BuildScene scene in value.buildScenes ?? Array.Empty<BuildScene>())
                if (scene == null || scene.index < 1 || !validated.ContainsKey(scene.guid ?? "") || standIns.ContainsKey(scene.index))
                    throw new InvalidDataException("Invalid build scene route.");
                else standIns.Add(scene.index, scene.guid);
            routes.Clear(); loaded.Clear(); adopted.Clear(); buildScenes.Clear();
            foreach (var entry in validated) routes.Add(entry.Key, entry.Value);
            foreach (var entry in standIns) buildScenes.Add(entry.Key, entry.Value);
            session = owner;
            SceneManager.sceneUnloaded += ReleaseAdopted;
            SceneManager.sceneLoaded += BuildStandIn;
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

        // The engine loaded the host's stand-in for a Build Settings scene, as
        // the game asked SceneManager: the authored scene is built into it.
        private static void BuildStandIn(Scene scene, LoadSceneMode mode)
        {
            if (session == null || !buildScenes.TryGetValue(scene.buildIndex, out string guid)
                || scene.path.IndexOf(StandInFolder, StringComparison.Ordinal) < 0 || adopted.Contains(scene)) return;
            try { LoadSceneInto(guid, scene); }
            catch (Exception failure)
            {
                Debug.LogError("[UnityMicroEditor] Build Settings scene " + scene.name + " could not be built from its source.");
                Debug.LogException(failure);
            }
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
            if (!TryLoadAsset(guid, typeof(GameObject), out Object asset)) return false;
            prefab = (GameObject)asset;
            return true;
        }

        /// <summary>An addressed asset of the session, or its first sub-object of
        /// <paramref name="type"/> (a texture's sprite); the main object for null.</summary>
        public static bool TryLoadAsset(string guid, Type type, out Object asset)
        {
            asset = null;
            return session != null && session.TryGetAsset(guid, type, out asset);
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
            SceneManager.sceneLoaded -= BuildStandIn;
            session = null; routes.Clear(); loaded.Clear(); adopted.Clear(); buildScenes.Clear();
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
