using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Security.Cryptography;
using Unity.Collections;
using Unity.Content;
using Unity.Loading;
using UnityEngine;
using Object = UnityEngine.Object;

namespace UnityMicroEditor.Raw
{
    public static partial class RawSceneLoader
    {
        private sealed class NativeContentHandle
        {
            internal ContentFile File;
            internal string Hash;
            internal readonly Dictionary<string, Object> Objects = new Dictionary<string, Object>(StringComparer.Ordinal);
            internal readonly Dictionary<string, Object> ReplacedAssets = new Dictionary<string, Object>(StringComparer.Ordinal);
            internal NativeContentHandle[] Dependencies;
            // Set while the file loads: what its readback checks, and the
            // dependencies handed to the load, which live until it is done.
            internal NativeContentFileDescriptor Pending;
            internal NativeArray<ContentFile> PendingDependencies;
        }

        private static void ReleaseRetiredNativeContent(NativeAssetCache cache)
        {
            // A still-retained content consumer can reference an older revision
            // of a dependency. Keep that revision until its consumer retires.
            while (cache.RetiredContentFiles.Count != 0)
            {
                int unloadIndex = -1;
                for (int index = 0; index < cache.RetiredContentFiles.Count; index++)
                {
                    NativeContentHandle candidate = cache.RetiredContentFiles[index];
                    bool referenced = false;
                    foreach (NativeContentHandle active in cache.ContentFiles.Values)
                        if (NativeContentReferences(active, candidate)) { referenced = true; break; }
                    if (!referenced)
                        foreach (NativeContentHandle other in cache.RetiredContentFiles)
                            if (NativeContentReferences(other, candidate)) { referenced = true; break; }
                    if (!referenced) { unloadIndex = index; break; }
                }
                if (unloadIndex < 0) return;
                NativeContentHandle handle = cache.RetiredContentFiles[unloadIndex];
                if (handle.File.IsValid && !handle.File.UnloadAsync().WaitForCompletion(10000))
                    throw new InvalidOperationException("Retired native ContentFile unload timed out.");
                cache.RetiredContentFiles.RemoveAt(unloadIndex);
            }
        }

        private static bool NativeContentReferences(NativeContentHandle owner, NativeContentHandle dependency)
        {
            if (ReferenceEquals(owner, dependency) || owner.Dependencies == null) return false;
            foreach (NativeContentHandle value in owner.Dependencies) if (ReferenceEquals(value, dependency)) return true;
            return false;
        }

        private static void ReleaseNativeContentCache(NativeAssetCache cache)
        {
            var remaining = new HashSet<NativeContentHandle>(cache.ContentFiles.Values);
            foreach (NativeContentHandle handle in cache.RetiredContentFiles) remaining.Add(handle);
            while (remaining.Count != 0)
            {
                NativeContentHandle unload = null;
                foreach (NativeContentHandle candidate in remaining)
                {
                    bool referenced = false;
                    foreach (NativeContentHandle other in remaining)
                    {
                        if (ReferenceEquals(other, candidate) || other.Dependencies == null) continue;
                        foreach (NativeContentHandle dependency in other.Dependencies)
                            if (ReferenceEquals(dependency, candidate)) { referenced = true; break; }
                        if (referenced) break;
                    }
                    if (!referenced) { unload = candidate; break; }
                }
                if (unload == null) throw new InvalidOperationException("Native ContentFile lifetime dependencies contain a cycle.");
                if (unload.File.IsValid && !unload.File.UnloadAsync().WaitForCompletion(10000))
                    throw new InvalidOperationException("Native ContentFile unload timed out.");
                remaining.Remove(unload);
            }
            cache.ContentFiles.Clear(); cache.RetiredContentFiles.Clear();
        }

        private sealed partial class ImportContext
        {
            private void LoadNativeContentFiles() { LoadNativeContentFiles(manifest.nativeContentFiles); }

            private void LoadNativeContentFiles(NativeContentFileDescriptor[] files)
            {
                if (files == null || files.Length == 0) return;
                if (nativeCache == null) throw new InvalidDataException("Direct native ContentFiles require a retained raw scene session.");
                var descriptors = new Dictionary<string, NativeContentFileDescriptor>(StringComparer.Ordinal);
                foreach (NativeContentFileDescriptor descriptor in files)
                {
                    ValidateNativeContentDescriptor(descriptor);
                    if (descriptors.ContainsKey(descriptor.guid)) throw new InvalidDataException("Duplicate native ContentFile GUID: " + descriptor.guid);
                    descriptors.Add(descriptor.guid, descriptor);
                }
                var loaded = new Dictionary<string, NativeContentHandle>(StringComparer.Ordinal);
                var visiting = new HashSet<string>(StringComparer.Ordinal);
                var created = new List<NativeContentHandle>();
                VerifyNativeContentHashes(files);
                try
                {
                    // Every file is started first, a dependency before what needs
                    // it, so that the loading thread takes them one after the
                    // other; each is then waited for and read back in that order.
                    foreach (NativeContentFileDescriptor descriptor in files)
                        LoadNativeContentDescriptor(descriptor, descriptors, loaded, visiting, created);
                    CompleteNativeContentFiles(created, loaded);
                    // All native parsing and exact-ID readback succeeds before
                    // replacing the cache. Old files remain alive for old scenes.
                    foreach (var item in loaded)
                    {
                        NativeContentHandle next = item.Value;
                        if (nativeCache.ContentFiles.TryGetValue(item.Key, out NativeContentHandle previous))
                        {
                            if (ReferenceEquals(previous, next)) continue;
                            foreach (var entry in previous.Objects)
                            {
                                if (previous.ReplacedAssets.TryGetValue(entry.Key, out Object original))
                                { assets[entry.Key] = original; nativeCache.Assets[entry.Key] = original; }
                                else { assets.Remove(entry.Key); nativeCache.Assets.Remove(entry.Key); }
                            }
                            nativeCache.RetiredContentFiles.Add(previous);
                        }
                        foreach (var entry in next.Objects)
                        {
                            if (assets.TryGetValue(entry.Key, out Object original)) next.ReplacedAssets.Add(entry.Key, original);
                            assets[entry.Key] = entry.Value;
                            nativeCache.Assets[entry.Key] = entry.Value;
                        }
                        nativeCache.ContentFiles[item.Key] = next;
                        nativeCache.LoadCount++;
                    }
                }
                catch (Exception loadFailure)
                {
                    Exception cleanupFailure = null;
                    for (int index = created.Count - 1; index >= 0; index--)
                    {
                        NativeContentHandle failed = created[index];
                        if (failed.File.IsValid)
                            RawCleanup.Run(() => failed.File.UnloadAsync().WaitForCompletion(10000), ref cleanupFailure);
                        if (failed.PendingDependencies.IsCreated) failed.PendingDependencies.Dispose();
                    }
                    if (cleanupFailure != null) throw new AggregateException("Native ContentFile load and cleanup failed.", loadFailure, cleanupFailure);
                    throw;
                }
                finally { verifiedContent = null; }
            }

            // The files whose hashes were checked for this load, as "path\nsha256".
            private static HashSet<string> verifiedContent;

            // Checks every file the load reads against its hash at once, on all
            // cores: some 1 000 files and 120 MB for a scene, which one thread
            // hashed for seconds. A file that does not match is checked again,
            // and refused, where it is loaded.
            private void VerifyNativeContentHashes(NativeContentFileDescriptor[] descriptors)
            {
                var checks = new HashSet<KeyValuePair<string, string>>();
                foreach (NativeContentFileDescriptor descriptor in descriptors)
                {
                    if (nativeCache.ContentFiles.TryGetValue(descriptor.guid, out NativeContentHandle cached) && cached.Hash == descriptor.sha256)
                        continue;
                    string path = AssetPath(manifest, descriptor.path);
                    checks.Add(new KeyValuePair<string, string>(path, descriptor.sha256));
                    foreach (NativeContentResourceDescriptor resource in descriptor.resources ?? Array.Empty<NativeContentResourceDescriptor>())
                    {
                        if (resource == null || string.IsNullOrEmpty(resource.virtualPath)) continue;
                        checks.Add(new KeyValuePair<string, string>(AssetPath(manifest, resource.path), resource.sha256));
                        checks.Add(new KeyValuePair<string, string>(Path.GetFullPath(Path.Combine(Environment.CurrentDirectory, resource.virtualPath)), resource.sha256));
                        checks.Add(new KeyValuePair<string, string>(Path.GetFullPath(Path.Combine(Path.GetDirectoryName(path), resource.virtualPath)), resource.sha256));
                    }
                }
                var verified = new System.Collections.Concurrent.ConcurrentBag<string>();
                System.Threading.Tasks.Parallel.ForEach(checks, check =>
                {
                    try
                    {
                        using (var stream = File.OpenRead(check.Key))
                        using (var hash = SHA256.Create())
                            if (RawReloadIdentity.Hex(hash.ComputeHash(stream)) == check.Value) verified.Add(check.Key + "\n" + check.Value);
                    }
                    catch (IOException) { }
                    catch (UnauthorizedAccessException) { }
                });
                verifiedContent = new HashSet<string>(verified, StringComparer.Ordinal);
            }

            private NativeContentHandle LoadNativeContentDescriptor(NativeContentFileDescriptor descriptor,
                Dictionary<string, NativeContentFileDescriptor> descriptors, Dictionary<string, NativeContentHandle> loaded,
                HashSet<string> visiting, List<NativeContentHandle> created)
            {
                if (loaded.TryGetValue(descriptor.guid, out NativeContentHandle complete)) return complete;
                if (!visiting.Add(descriptor.guid)) throw new InvalidDataException("Native ContentFile dependency cycle: " + descriptor.guid);
                NativeContentDependencyDescriptor[] declared = descriptor.dependencies ?? Array.Empty<NativeContentDependencyDescriptor>();
                var dependencies = new NativeContentHandle[declared.Length];
                var nativeDependencies = new NativeArray<ContentFile>(declared.Length, Allocator.Persistent);
                try
                {
                    for (int index = 0; index < declared.Length; index++)
                    {
                        NativeContentDependencyDescriptor dependency = declared[index];
                        if (dependency == null || !ValidNativeGUID(dependency.guid)) throw new InvalidDataException("Native ContentFile dependency GUID is invalid.");
                        if (dependency.global)
                        {
                            if (dependency.guid != BuiltinResourceGuid && dependency.guid != "0000000000000000e000000000000000")
                                throw new InvalidDataException("Global native dependencies must identify Unity built-in resources.");
                            nativeDependencies[index] = ContentFile.GlobalTableDependency;
                            continue;
                        }
                        if (!descriptors.TryGetValue(dependency.guid, out NativeContentFileDescriptor target))
                            throw new InvalidDataException("Native ContentFile dependency has no published descriptor: " + dependency.guid);
                        dependencies[index] = LoadNativeContentDescriptor(target, descriptors, loaded, visiting, created);
                        nativeDependencies[index] = dependencies[index].File;
                    }
                    if (nativeCache.ContentFiles.TryGetValue(descriptor.guid, out NativeContentHandle cached)
                        && cached.Hash == descriptor.sha256 && NativeContentObjectsMatch(cached, descriptor)
                        && NativeContentDependenciesMatch(cached.Dependencies, dependencies))
                    {
                        loaded.Add(descriptor.guid, cached);
                        return cached;
                    }
                    string path = AssetPath(manifest, descriptor.path);
                    RequireNativeContentHash(path, descriptor.sha256);
                    ValidateNativeContentResources(descriptor, path);
                    var handle = new NativeContentHandle { Hash = descriptor.sha256, Dependencies = dependencies };
                    created.Add(handle);
                    handle.File = ContentLoadInterface.LoadContentFileAsync(ContentNamespace.Default, path, nativeDependencies);
                    handle.Pending = descriptor;
                    handle.PendingDependencies = nativeDependencies;
                    nativeDependencies = default;
                    loaded.Add(descriptor.guid, handle);
                    return handle;
                }
                finally
                {
                    if (nativeDependencies.IsCreated) nativeDependencies.Dispose();
                    visiting.Remove(descriptor.guid);
                }
            }

            // Waits for the files started by the load, in the order they were
            // started, and reads back what each holds.
            // The objects of the content files the game's code destroyed while they loaded.
            private readonly HashSet<string> destroyedByGame = new HashSet<string>(StringComparer.Ordinal);

            private void CompleteNativeContentFiles(List<NativeContentHandle> created, Dictionary<string, NativeContentHandle> loaded)
            {
                int nativeErrors = 0;
                string nativeFailure = string.Empty;
                var errorLock = new object();
                // What the engine reports of the files themselves. An exception of the
                // game's code that loading runs (a ScriptableObject's OnEnable) and what
                // that code logs are the game's: the Editor logs them, and the load stands.
                Application.LogCallback capture = (message, stack, type) =>
                {
                    if (type != LogType.Error && type != LogType.Assert) return;
                    if (stack != null && stack.Contains("UnityEngine.Debug:Log")) return;
                    lock (errorLock) { nativeErrors++; if (nativeFailure.Length == 0) nativeFailure = message; }
                };
                Application.logMessageReceivedThreaded += capture;
                try
                {
                    foreach (NativeContentHandle handle in created)
                    {
                        NativeContentFileDescriptor descriptor = handle.Pending;
                        if (descriptor == null) continue;
                        try
                        {
                            if (!handle.File.IsValid || !handle.File.WaitForCompletion(10000) || handle.File.LoadingStatus != LoadingStatus.Completed)
                                throw new InvalidDataException("Native ContentFile failed to load: " + AssetPath(manifest, descriptor.path));
                        }
                        finally
                        {
                            if (handle.PendingDependencies.IsCreated) handle.PendingDependencies.Dispose();
                            handle.Pending = null;
                        }
                        foreach (NativeContentObjectDescriptor entry in descriptor.objects)
                        {
                            long localID = long.Parse(entry.fileId, NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture);
                            Object value = handle.File.GetObject(unchecked((ulong)localID));
                            // An object of an Editor-only class (an animator controller, an audio
                            // mixer) has another, runtime class in a Player; only its presence is checked.
                            bool editorClass = entry.typeName.StartsWith("UnityEditor.", StringComparison.Ordinal);
                            // The game's code that loading runs may destroy an object of its own
                            // class (an Editor path of a ScriptableObject's OnEnable that keeps one
                            // instance of its settings); what refers to it then finds none.
                            if (value == null && !editorClass && !entry.typeName.StartsWith("UnityEngine.", StringComparison.Ordinal))
                            {
                                Debug.LogWarning("[unity-micro-editor] the game's code destroyed " + entry.typeName + " of " + AssetPath(manifest, descriptor.path) + " while it loaded");
                                destroyedByGame.Add(AssetKey(descriptor.guid, entry.fileId));
                                continue;
                            }
                            if (value == null || !editorClass && value.GetType().FullName != entry.typeName)
                                throw new InvalidDataException("Native ContentFile identity/type readback failed: " + descriptor.guid + "/" + entry.fileId);
                            // A shader a build compiled no program of (every variant stripped) is
                            // unsupported in a built Player as well; nothing draws with it.
                            if (value is Shader shader && !shader.isSupported
                                && Array.IndexOf(descriptor.programlessShaders ?? Array.Empty<string>(), entry.fileId) < 0)
                                throw new InvalidDataException("Native compiled Shader is unsupported on the active GPU: " + descriptor.guid + "/" + entry.fileId);
                            // A clip that is not preloaded stays unloaded until the game loads or plays it, one
                            // that loads in the background may still be loading, and a streaming clip opens its
                            // resource when it plays; each is its authored behaviour. Only a preloaded,
                            // synchronous clip that is decoded or kept in memory has to hold its data here.
                            if (value is AudioClip clip && (clip.loadState == AudioDataLoadState.Failed
                                || (clip.preloadAudioData && !clip.loadInBackground && clip.loadType != AudioClipLoadType.Streaming
                                    && clip.loadState != AudioDataLoadState.Loaded)))
                                throw new InvalidDataException("Direct native audio failed to load its data: " + descriptor.guid);
                            handle.Objects.Add(AssetKey(descriptor.guid, entry.fileId), value);
                        }
                    }
                    lock (errorLock)
                        if (nativeErrors != 0) throw new InvalidDataException("Native ContentFile emitted errors: " + nativeFailure);
                }
                finally
                {
                    Application.logMessageReceivedThreaded -= capture;
                    foreach (NativeContentHandle handle in created)
                        if (handle.PendingDependencies.IsCreated) handle.PendingDependencies.Dispose();
                }
            }

            private static bool NativeContentDependenciesMatch(NativeContentHandle[] first, NativeContentHandle[] second)
            {
                if (first == null || first.Length != second.Length) return false;
                for (int index = 0; index < first.Length; index++) if (!ReferenceEquals(first[index], second[index])) return false;
                return true;
            }

            private static bool NativeContentObjectsMatch(NativeContentHandle handle, NativeContentFileDescriptor descriptor)
            {
                if (!handle.File.IsValid || handle.Objects.Count != descriptor.objects.Length) return false;
                foreach (NativeContentObjectDescriptor entry in descriptor.objects)
                    if (!handle.Objects.TryGetValue(AssetKey(descriptor.guid, entry.fileId), out Object value)
                        || value == null || value.GetType().FullName != entry.typeName) return false;
                return true;
            }

            private void ValidateNativeContentResources(NativeContentFileDescriptor descriptor, string serializedPath)
            {
                if (descriptor.resources == null) return;
                foreach (NativeContentResourceDescriptor resource in descriptor.resources)
                {
                    if (resource == null || resource.virtualPath != "VirtualArtifacts/Primary/" + descriptor.guid + ".resource")
                        throw new InvalidDataException("Direct native resource alias is outside its source GUID namespace.");
                    RequireNativeContentHash(AssetPath(manifest, resource.path), resource.sha256);
                    string mounted = Path.GetFullPath(Path.Combine(Environment.CurrentDirectory, resource.virtualPath));
                    RequireNativeContentHash(mounted, resource.sha256);
                    // Physical native files resolve their streamed resources
                    // relative to the serialized file directory. The isolated
                    // probe had this directory equal to CWD; project imports
                    // must validate the distinct staged resource before Unity
                    // attempts a native AudioClip load.
                    string besideSerialized = Path.GetFullPath(Path.Combine(Path.GetDirectoryName(serializedPath), resource.virtualPath));
                    if (!string.Equals(besideSerialized, mounted, StringComparison.Ordinal))
                        RequireNativeContentHash(besideSerialized, resource.sha256);
                }
            }

            private static void ValidateNativeContentDescriptor(NativeContentFileDescriptor descriptor)
            {
                if (descriptor == null || !ValidNativeGUID(descriptor.guid) || string.IsNullOrEmpty(descriptor.path)
                    || descriptor.objects == null || descriptor.objects.Length == 0 || descriptor.objects.Length > 100000
                    || !ValidNativeHash(descriptor.sha256)) throw new InvalidDataException("Native ContentFile descriptor is incomplete.");
                var keys = new HashSet<string>(StringComparer.Ordinal);
                foreach (NativeContentObjectDescriptor entry in descriptor.objects)
                    if (entry == null || string.IsNullOrEmpty(entry.typeName)
                        || !long.TryParse(entry.fileId, NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture, out long id) || id == 0
                        || id.ToString(CultureInfo.InvariantCulture) != entry.fileId || !keys.Add(entry.fileId))
                        throw new InvalidDataException("Native ContentFile object identity is invalid.");
            }

            private static bool ValidNativeGUID(string guid)
            { return guid != null && guid.Length == 32 && guid != "00000000000000000000000000000000" && ValidNativeHex(guid); }

            private static bool ValidNativeHash(string hash)
            { return hash != null && hash.Length == 64 && ValidNativeHex(hash); }

            private static bool ValidNativeHex(string value)
            {
                foreach (char digit in value) if (digit < '0' || digit > '9' && (digit < 'a' || digit > 'f')) return false;
                return true;
            }

            private static void RequireNativeContentHash(string path, string expected)
            {
                if (!ValidNativeHash(expected)) throw new InvalidDataException("Native content hash is invalid.");
                if (verifiedContent != null && verifiedContent.Contains(path + "\n" + expected)) return;
                using (var stream = File.OpenRead(path))
                using (var hash = SHA256.Create())
                    if (RawReloadIdentity.Hex(hash.ComputeHash(stream)) != expected)
                        throw new InvalidDataException("Native content changed after publication: " + path);
            }
        }
    }
}
