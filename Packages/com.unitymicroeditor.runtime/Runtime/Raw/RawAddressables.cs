#if UNITY_MICRO_EDITOR_ADDRESSABLES
using System;
using System.Collections.Generic;
using System.IO;
using UnityEngine;
using UnityEngine.AddressableAssets;
using UnityEngine.AddressableAssets.ResourceLocators;
using UnityEngine.ResourceManagement.AsyncOperations;
using UnityEngine.ResourceManagement.ResourceLocations;
using UnityEngine.ResourceManagement.ResourceProviders;
using UnityEngine.SceneManagement;

namespace UnityMicroEditor.Raw
{
    /// <summary>
    /// Serves the project's Addressables keys from authored sources, as the
    /// Editor's "Use Asset Database" play mode does: an address, GUID or label
    /// resolves to a scene or an asset of the raw session (a prefab, material,
    /// texture or one of its sprites, clip, ScriptableObject and the like),
    /// with no bundles and nothing to download. Keys it does not know stay
    /// with the host's catalog.
    /// </summary>
    internal static class RawAddressables
    {
        private static SourceLocator locator;
        private static SourceProvider provider;
        // The stock scene provider loads this empty scene of the host build;
        // the raw loader then builds the authored scene into it.
        private static string hostScene;
        // Scene loads whose dependency was provided, in the order the engine activates them.
        private static readonly Queue<RawProjectContent.Address> pendingScenes = new Queue<RawProjectContent.Address>();

        internal static void Install(RawProjectContent.Address[] entries)
        {
            if (entries.Length == 0 || locator != null) return;
            hostScene = SceneUtility.GetScenePathByBuildIndex(0);
            if (string.IsNullOrEmpty(hostScene)) throw new InvalidOperationException("The host Player has no scene to adopt addressable scenes into.");
            provider = new SourceProvider();
            locator = new SourceLocator(entries, provider.ProviderId);
            Addressables.ResourceManager.ResourceProviders.Add(provider);
            Addressables.AddResourceLocator(locator);
            SceneManager.sceneLoaded += BuildAdoptedScene;
            Debug.Log("[UnityMicroEditor] Addressables: " + entries.Length + " keys are served from sources.");
            if (!File.Exists(Addressables.RuntimePath + "/settings.json"))
            {
                // Without a catalog the package's own initialization fails, as
                // in any Player built without content. Source keys still resolve.
                Debug.LogWarning("[UnityMicroEditor] Addressables: the host has no built content, so Addressables.InitializeAsync reports a failure. Build Addressables content with the Player to get a catalog.");
                return;
            }
            // Initialize before the game asks, so that its first request already
            // sees source keys ahead of the host's catalog.
            AsyncOperationHandle<IResourceLocator> initialization = Addressables.InitializeAsync(false);
            if (initialization.IsDone) Initialized(initialization);
            else initialization.Completed += Initialized;
        }

        internal static void Uninstall()
        {
            if (locator == null) return;
            SceneManager.sceneLoaded -= BuildAdoptedScene;
            Addressables.RemoveResourceLocator(locator);
            Addressables.ResourceManager.ResourceProviders.Remove(provider);
            locator = null; provider = null; pendingScenes.Clear();
        }

        private static void Initialized(AsyncOperationHandle<IResourceLocator> initialization)
        {
            if (locator == null) return;
            if (initialization.Status != AsyncOperationStatus.Succeeded) return;
            var hosts = new List<IResourceLocator>();
            foreach (IResourceLocator candidate in Addressables.ResourceLocators)
                if (!ReferenceEquals(candidate, locator) && !(candidate is HostLocator)) hosts.Add(candidate);
            foreach (IResourceLocator host in hosts)
            {
                ResourceLocatorInfo info = Addressables.GetLocatorInfo(host);
                Addressables.RemoveResourceLocator(host);
                Addressables.AddResourceLocator(new HostLocator(host, locator), info?.LocalHash, info?.CatalogLocation);
            }
        }

        private static void BuildAdoptedScene(Scene scene, LoadSceneMode mode)
        {
            if (pendingScenes.Count == 0 || scene.path != hostScene) return;
            RawProjectContent.Address entry = pendingScenes.Dequeue();
            try { RawProjectContent.LoadSceneInto(entry.guid, scene); }
            catch (Exception failure)
            {
                // The stock provider still completes its handle; the scene stays empty.
                Debug.LogError("[UnityMicroEditor] Addressable scene could not be built from its source: " + entry.path);
                Debug.LogException(failure);
            }
        }

        private sealed class SceneRequest { public RawProjectContent.Address entry; }
        private sealed class NoBundle : IAssetBundleResource { public AssetBundle GetAssetBundle() { return null; } }

        private sealed class SourceLocator : IResourceLocator
        {
            private readonly Dictionary<string, List<RawProjectContent.Address>> entriesByKey = new Dictionary<string, List<RawProjectContent.Address>>(StringComparer.Ordinal);
            private readonly HashSet<string> addresses = new HashSet<string>(StringComparer.Ordinal);
            private readonly string providerId;
            private int sceneRequests;

            public string LocatorId { get { return "UnityMicroEditorSources"; } }
            public IEnumerable<object> Keys { get { return entriesByKey.Keys; } }
            public IEnumerable<IResourceLocation> AllLocations { get { return Array.Empty<IResourceLocation>(); } }

            internal SourceLocator(RawProjectContent.Address[] entries, string providerId)
            {
                this.providerId = providerId;
                foreach (RawProjectContent.Address entry in entries)
                {
                    addresses.Add(entry.address);
                    Add(entry.address, entry); Add(entry.guid, entry);
                    foreach (string label in entry.labels ?? Array.Empty<string>()) Add(label, entry);
                }
            }

            internal bool Serves(string address) { return address != null && addresses.Contains(address); }

            private void Add(string key, RawProjectContent.Address entry)
            {
                if (string.IsNullOrEmpty(key)) return;
                if (!entriesByKey.TryGetValue(key, out List<RawProjectContent.Address> entries)) entriesByKey.Add(key, entries = new List<RawProjectContent.Address>());
                if (!entries.Contains(entry)) entries.Add(entry);
            }

            public bool Locate(object key, Type type, out IList<IResourceLocation> locations)
            {
                locations = null;
                if (!(key is string text) || !entriesByKey.TryGetValue(text, out List<RawProjectContent.Address> entries)) return false;
                var found = new List<IResourceLocation>();
                foreach (RawProjectContent.Address entry in entries)
                {
                    if (!entry.scene)
                    {
                        // The asset's object of the asked type, as the
                        // session's content holds it: a texture asked as a
                        // Sprite is found by its sprite.
                        Type asked = type == null || type == typeof(object) ? null : type;
                        if (RawProjectContent.TryLoadAsset(entry.guid, asked, out UnityEngine.Object asset))
                            found.Add(new ResourceLocationBase(entry.address, entry.path, providerId, asked ?? asset.GetType()) { Data = entry });
                    }
                    else if (type == null || type == typeof(object) || type == typeof(SceneInstance))
                    {
                        // A dependency of its own makes every load reach the
                        // provider, which is where the load is tied to its scene.
                        var request = new ResourceLocationBase(entry.address, entry.path + "#" + (++sceneRequests), providerId, typeof(SceneRequest))
                        { Data = new SceneRequest { entry = entry } };
                        found.Add(new ResourceLocationBase(entry.address, hostScene, typeof(SceneProvider).FullName, typeof(SceneInstance), request));
                    }
                }
                if (found.Count == 0) return false;
                locations = found;
                return true;
            }
        }

        // The host catalog still answers the keys the sources do not serve.
        private sealed class HostLocator : IResourceLocator
        {
            private readonly IResourceLocator host;
            private readonly SourceLocator sources;

            internal HostLocator(IResourceLocator host, SourceLocator sources) { this.host = host; this.sources = sources; }

            public string LocatorId { get { return host.LocatorId; } }
            public IEnumerable<object> Keys { get { return host.Keys; } }
            // The interface has this member unless the package was built for JSON catalogs.
            public IEnumerable<IResourceLocation> AllLocations
            {
                get
                {
                    var property = typeof(IResourceLocator).GetProperty("AllLocations");
                    return property == null ? Array.Empty<IResourceLocation>() : (IEnumerable<IResourceLocation>)property.GetValue(host);
                }
            }

            public bool Locate(object key, Type type, out IList<IResourceLocation> locations)
            {
                if (!host.Locate(key, type, out locations)) return false;
                List<IResourceLocation> kept = null;
                for (int index = 0; index < locations.Count; index++)
                {
                    if (!sources.Serves(locations[index].PrimaryKey)) { if (kept != null) kept.Add(locations[index]); continue; }
                    if (kept != null) continue;
                    kept = new List<IResourceLocation>(locations.Count);
                    for (int before = 0; before < index; before++) kept.Add(locations[before]);
                }
                if (kept == null) return true;
                locations = kept.Count == 0 ? null : kept;
                return locations != null;
            }
        }

        private sealed class SourceProvider : ResourceProviderBase
        {
            public override void Provide(ProvideHandle handle)
            {
                object data = handle.Location.Data;
                if (data is SceneRequest request)
                {
                    pendingScenes.Enqueue(request.entry);
                    handle.Complete<IAssetBundleResource>(new NoBundle(), true, null);
                    return;
                }
                UnityEngine.Object asset = null;
                Exception failure = null;
                try
                {
                    Type asked = handle.Type == null || handle.Type == typeof(object) ? null : handle.Type;
                    if (!(data is RawProjectContent.Address entry))
                        failure = new InvalidOperationException("Sources provide no asset for " + handle.Location.InternalId);
                    else if (!RawProjectContent.TryLoadAsset(entry.guid, asked, out asset))
                        failure = new InvalidOperationException("The raw session has no " + (asked == null ? "asset" : asked.Name) + " for " + entry.path);
                }
                catch (Exception error) { failure = error; }
                handle.Complete<object>(asset, failure == null, failure);
            }
        }
    }
}
#endif
