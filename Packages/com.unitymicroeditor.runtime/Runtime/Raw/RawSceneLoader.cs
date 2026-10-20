using System;
using System.Collections.Generic;
using System.IO;
using UnityEngine;
using UnityEngine.SceneManagement;
using Object = UnityEngine.Object;

namespace UnityMicroEditor.Raw
{
    /// <summary>
    /// Loads a session's scene, which the import worker wrote in the Player
    /// format, with the engine's own loader, after the content files it
    /// refers to.
    /// </summary>
    public static partial class RawSceneLoader
    {
        private const string BuiltinMeshGuid = "0000000000000000e000000000000000";
        private const string BuiltinResourceGuid = "0000000000000000f000000000000000";

        public static RawSceneManifest ReadManifest(string manifestPath)
        {
            if (string.IsNullOrEmpty(manifestPath) || !Path.IsPathRooted(manifestPath))
                throw new ArgumentException("Scene manifest path must be absolute.", nameof(manifestPath));
            return ParseManifest(File.ReadAllText(manifestPath));
        }

        private static RawSceneManifest ParseManifest(string text)
        {
            var manifest = JsonUtility.FromJson<RawSceneManifest>(text);
            if (manifest == null) throw new InvalidDataException("Empty scene manifest.");
            Validate(manifest);
            return manifest;
        }

        public static void Validate(string manifestPath) { ReadManifest(manifestPath); }

        public static void Validate(RawSceneManifest manifest)
        {
            if (manifest == null || manifest.schemaVersion != 1 || !manifest.supported)
                throw new InvalidDataException("Scene manifest is unsupported or has the wrong schema version.");
            if (manifest.unityVersion != Application.unityVersion)
                throw new InvalidDataException("Project and prepared host must use the same exact Unity version.");
            if (string.IsNullOrEmpty(manifest.projectRoot) || !Path.IsPathRooted(manifest.projectRoot)
                || !Directory.Exists(manifest.projectRoot)) throw new InvalidDataException("Project root is missing.");
            if (string.IsNullOrEmpty(manifest.sceneName)) throw new InvalidDataException("Scene name is missing.");
            if (!HasNativeScene(manifest)) throw new InvalidDataException("The manifest names no scene in the Player format.");
            if (manifest.assemblies != null)
                foreach (string path in manifest.assemblies) AssetPath(manifest, path);
        }

        // The game's assemblies, which the scene's scripts name.
        private static void LoadScriptAssemblies(RawSceneManifest manifest)
        {
            if (manifest.assemblies == null) return;
            foreach (string path in manifest.assemblies)
            {
                string fullPath = AssetPath(manifest, path);
                if (Path.GetExtension(fullPath) != ".dll") throw new InvalidDataException("Script assembly is not a DLL: " + path);
                UnityEngine.Assemblies.CurrentAssemblies.LoadFromPath(fullPath);
            }
        }

        private static string AssetPath(RawSceneManifest manifest, string path)
        {
            if (string.IsNullOrEmpty(path)) throw new InvalidDataException("Asset path is missing.");
            string result = Path.GetFullPath(Path.IsPathRooted(path) ? path : Path.Combine(manifest.projectRoot, path));
            if (!File.Exists(result)) throw new FileNotFoundException("A file the manifest names is missing.", result);
            return result;
        }
        private static string AssetKey(string guid, string fileId) { return guid + ":" + (fileId ?? ""); }
        private static bool IsNull(string fileId) { return string.IsNullOrEmpty(fileId) || fileId == "0"; }

        private sealed partial class ImportContext
        {
            private readonly RawSceneManifest manifest;
            private readonly NativeAssetCache nativeCache;
            private readonly Dictionary<string, Object> assets = new Dictionary<string, Object>(StringComparer.Ordinal);
            private Object[] preloadedAssets = Array.Empty<Object>();
            private Scene scene;

            public ImportContext(RawSceneManifest manifest, NativeAssetCache nativeCache)
            { this.manifest = manifest; this.nativeCache = nativeCache; }

            // A valid target is an empty scene some other owner loaded; the
            // scene's roots are given to it once they woke.
            public Scene Load(bool runInitializers = true, Action commit = null, bool main = true, Scene target = default)
            { return LoadNative(runInitializers, commit, main, target); }

            internal bool TryGetPrefab(string guid, out GameObject prefab) { return TryGetNativePrefab(guid, out prefab); }

            internal void ReleaseOwnedSceneObjects() { UnloadNativeScene(); }

            // An object the manifest names by its source, as a content file holds it.
            private Object Resolve(string guid, string fileId)
            {
                if (IsNull(fileId)) return null;
                if (!assets.TryGetValue(AssetKey(guid, fileId), out Object value))
                    throw new InvalidDataException("The scene's content holds no object " + guid + "/" + fileId + ".");
                return value;
            }
        }
    }
}
