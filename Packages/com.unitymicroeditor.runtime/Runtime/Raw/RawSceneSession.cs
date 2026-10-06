using System;
using System.Collections.Generic;
using System.IO;
using System.Security.Cryptography;
using System.Text;
using UnityEngine;
using UnityEngine.SceneManagement;
using Object = UnityEngine.Object;

namespace Dungeon.CLIPlay.Raw
{
    public static partial class RawSceneLoader
    {
        public static string CurrentRevision { get; private set; } = string.Empty;

        public static LiveSession OpenSession(string manifestPath, string contentRoutesPath = null)
        {
            var manifest = ReadManifest(manifestPath);
            var session = new LiveSession(manifest, manifestPath);
            if (!string.IsNullOrEmpty(contentRoutesPath)) RawProjectContent.Initialize(contentRoutesPath, session, manifest.projectRoot);
            try { session.LoadInitial(manifest); return session; }
            catch { session.Dispose(); throw; }
        }

        public sealed partial class LiveSession : IDisposable
        {
            private readonly RawReloadIdentity identity;
            private readonly NativeAssetCache cache = new NativeAssetCache();
            private ImportContext current;
            private readonly Dictionary<Scene, ImportContext> additive = new Dictionary<Scene, ImportContext>();
            private readonly Dictionary<Scene, AsyncOperation> unloading = new Dictionary<Scene, AsyncOperation>();
            private bool disposed;
            public Scene Scene { get; private set; }
            public string Revision { get; private set; }
            public int ReloadCount { get; private set; }
            public bool LastReloadRestartRequired { get; private set; }
            public int NativeLoadCount => cache.LoadCount;
            public int NativeObjectCount => cache.Assets.Count;
            public string NativeIdentityDigest => cache.IdentityDigest();
            public AsyncOperation PreviousSceneUnload { get; private set; }
            public void ReleaseRetiredNativeFiles() { ReleaseRetiredNativeContent(cache); }

            internal LiveSession(RawSceneManifest manifest, string manifestPath)
            { identity = CaptureIdentity(manifest); Revision = ManifestRevision(manifestPath); }

            internal void LoadInitial(RawSceneManifest manifest)
            {
                var types = LoadScriptTypes(manifest);
                InvokeRuntimeInitializers(manifest, 4);
                InvokeRuntimeInitializers(manifest, 2);
                InvokeRuntimeInitializers(manifest, 3);
                current = new ImportContext(manifest, types, cache);
                CurrentRevision = Revision;
                Scene = current.Load();
            }

            public bool TryGetPrefab(string guid, out GameObject prefab)
            {
                if (disposed) throw new ObjectDisposedException(nameof(LiveSession));
                prefab = null;
                return current != null && current.TryGetPrefab(guid, out prefab);
            }

            public Scene LoadAdditive(string manifestPath, string expectedManifestSha256)
            {
                if (disposed) throw new ObjectDisposedException(nameof(LiveSession));
                byte[] bytes = File.ReadAllBytes(manifestPath);
                using (var hash = SHA256.Create())
                    if (string.IsNullOrEmpty(expectedManifestSha256) || expectedManifestSha256.Length != 64
                        || RawReloadIdentity.Hex(hash.ComputeHash(bytes)) != expectedManifestSha256.ToLowerInvariant())
                        throw new InvalidDataException("Raw content manifest changed after publication.");
                RawSceneManifest manifest;
                using (var stream = new MemoryStream(bytes, false))
                using (var reader = new StreamReader(stream, Encoding.UTF8, true)) manifest = ParseManifest(reader.ReadToEnd());
                identity.RequireCompatible(CaptureIdentity(manifest));
                var context = new ImportContext(manifest, LoadScriptTypes(manifest), cache);
                Scene loaded = context.Load(false, null, false);
                unloading.Remove(loaded);
                additive.Add(loaded, context);
                return loaded;
            }

            public AsyncOperation UnloadAdditive(Scene scene)
            {
                if (unloading.TryGetValue(scene, out AsyncOperation previous)) return previous;
                if (disposed) throw new ObjectDisposedException(nameof(LiveSession));
                if (!additive.TryGetValue(scene, out ImportContext context))
                    throw new InvalidOperationException("Scene is not owned by this raw content session.");
                additive.Remove(scene);
                return RetireScene(scene, context);
            }

            private AsyncOperation RetireScene(Scene scene, ImportContext context)
            {
                // Mark ownership before callbacks can request the same unload.
                unloading[scene] = null;
                Exception failure = null;
                RawCleanup.Run(context.ReleaseOwnedSceneObjects, ref failure);
                RawCleanup.Run(context.ReleasePreloadedRootScopes, ref failure);
                RawCleanup.Run(() => unloading[scene] = scene.IsValid() && scene.isLoaded
                    ? SceneManager.UnloadSceneAsync(scene) : null, ref failure);
                if (failure != null) throw new InvalidOperationException("Raw content scene cleanup failed.", failure);
                return unloading[scene];
            }

            internal bool TryGetPendingUnload(Scene scene, out AsyncOperation operation)
            { return unloading.TryGetValue(scene, out operation); }

            private void RetireAdditiveScenes()
            {
                var previous = new List<KeyValuePair<Scene, ImportContext>>(additive);
                additive.Clear();
                foreach (var entry in previous) unloading[entry.Key] = null;
                Exception failure = null;
                foreach (var entry in previous)
                    RawCleanup.Run(() => RetireScene(entry.Key, entry.Value), ref failure);
                RawProjectContent.ForgetLoadedScenes(this);
                if (failure != null) throw new InvalidOperationException("Raw additive scene cleanup failed.", failure);
            }

            public Scene Reload(string manifestPath, string expectedManifestSha256)
            {
                if (disposed) throw new ObjectDisposedException(nameof(LiveSession));
                LastReloadRestartRequired = false;
                byte[] bytes = File.ReadAllBytes(manifestPath);
                using (var hash = SHA256.Create())
                    if (string.IsNullOrEmpty(expectedManifestSha256) || expectedManifestSha256.Length != 64
                        || RawReloadIdentity.Hex(hash.ComputeHash(bytes)) != expectedManifestSha256.ToLowerInvariant())
                        throw new InvalidDataException("Raw reload manifest changed after publication.");
                RawSceneManifest manifest;
                using (var stream = new MemoryStream(bytes, false))
                using (var reader = new StreamReader(stream, Encoding.UTF8, true)) manifest = ParseManifest(reader.ReadToEnd());
                identity.RequireCompatible(CaptureIdentity(manifest));
                var next = new ImportContext(manifest, LoadScriptTypes(manifest), cache);
                string oldRevision = CurrentRevision;
                string revision = ManifestRevision(manifestPath);
                CurrentRevision = revision;
                try
                {
                    Scene oldScene = Scene;
                    Scene nextScene = next.Load(false, () =>
                    {
                        LastReloadRestartRequired = true;
                        // Retire source-owned objects before activating the new
                        // graph, including roots moved to DontDestroyOnLoad.
                        Exception failure = null;
                        RawCleanup.Run(RetireAdditiveScenes, ref failure);
                        RawCleanup.Run(current.ReleaseOwnedSceneObjects, ref failure);
                        RawCleanup.Run(current.ReleasePreloadedRootScopes, ref failure);
                        if (failure != null) throw new InvalidOperationException("Previous raw scene cleanup failed.", failure);
                    });
                    current = next;
                    Scene = nextScene;
                    Revision = revision;
                    ReloadCount++;
                    PreviousSceneUnload = oldScene.IsValid() && oldScene.isLoaded ? SceneManager.UnloadSceneAsync(oldScene) : null;
                    LastReloadRestartRequired = false;
                    return nextScene;
                }
                catch { CurrentRevision = oldRevision; throw; }
            }

            public void Dispose()
            {
                if (disposed) return;
                disposed = true;
                Exception failure = null;
                RawCleanup.Run(RetireAdditiveScenes, ref failure);
                if (current != null)
                {
                    RawCleanup.Run(current.ReleaseOwnedSceneObjects, ref failure);
                    RawCleanup.Run(current.ReleasePreloadedRootScopes, ref failure);
                }
                RawProjectContent.Release(this);
                RawCleanup.Run(cache.Dispose, ref failure);
                if (failure != null) throw new InvalidOperationException("Raw content session cleanup failed.", failure);
            }
        }

        private static string ManifestRevision(string manifestPath)
        { return new DirectoryInfo(Path.GetDirectoryName(Path.GetFullPath(manifestPath))).Name; }

        private static RawReloadIdentity CaptureIdentity(RawSceneManifest manifest)
        {
            var assemblies = new Dictionary<string, string>(StringComparer.Ordinal);
            if (manifest.assemblies != null)
                foreach (string relative in manifest.assemblies)
                {
                    string path = AssetPath(manifest, relative);
                    string name = Path.GetFileNameWithoutExtension(path);
                    if (assemblies.ContainsKey(name)) throw new InvalidDataException("Duplicate source assembly identity: " + name);
                    assemblies.Add(name, RawReloadIdentity.HashFile(path));
                }
            var packs = new Dictionary<string, string>(StringComparer.Ordinal);
            if (manifest.importedAssetPacks != null)
                foreach (ImportedAssetPack pack in manifest.importedAssetPacks)
                {
                    string path = AssetPath(manifest, pack.bundlePath);
                    packs.Add(path, pack.sha256.ToLowerInvariant() + ":" + pack.crc + ":" + pack.registryAsset);
                }
            return new RawReloadIdentity(Path.GetFullPath(manifest.projectRoot), assemblies, packs);
        }

        private sealed class NativeAssetCache : IDisposable
        {
            internal readonly Dictionary<string, NativeContentHandle> ContentFiles = new Dictionary<string, NativeContentHandle>(StringComparer.Ordinal);
            internal readonly List<NativeContentHandle> RetiredContentFiles = new List<NativeContentHandle>();
            internal readonly Dictionary<string, Object> Assets = new Dictionary<string, Object>(StringComparer.Ordinal);
            internal readonly List<AssetBundle> Bundles = new List<AssetBundle>();
            internal bool Loaded;
            internal int LoadCount;
            internal string IdentityDigest()
            {
                var keys = new List<string>(Assets.Keys); keys.Sort(StringComparer.Ordinal);
                var text = new StringBuilder();
                foreach (string key in keys)
                {
                    Object value = Assets[key];
                    if (value == null) throw new InvalidDataException("Retained native asset was destroyed: " + key);
                    text.Append(key).Append(':').Append(value.GetEntityId().ToString()).Append('\n');
                }
                using (var hash = SHA256.Create()) return RawReloadIdentity.Hex(hash.ComputeHash(Encoding.UTF8.GetBytes(text.ToString())));
            }
            public void Dispose()
            {
                ReleaseNativeContentCache(this);
                foreach (AssetBundle bundle in Bundles) if (bundle != null) bundle.Unload(true);
                Bundles.Clear(); Assets.Clear(); Loaded = false;
            }
        }
    }
}
