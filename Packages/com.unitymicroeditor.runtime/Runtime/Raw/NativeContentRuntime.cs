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
            internal readonly Dictionary<string, RawNativeMeshRetention> MeshRetentions = new Dictionary<string, RawNativeMeshRetention>(StringComparer.Ordinal);
            internal readonly List<Object> OwnedUIObjects = new List<Object>();
            internal string UIIdentity;
            internal NativeContentHandle[] Dependencies;
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
                NativeUIRuntime.Release(handle.OwnedUIObjects);
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
                NativeUIRuntime.Release(unload.OwnedUIObjects);
                if (unload.File.IsValid && !unload.File.UnloadAsync().WaitForCompletion(10000))
                    throw new InvalidOperationException("Native ContentFile unload timed out.");
                remaining.Remove(unload);
            }
            cache.ContentFiles.Clear(); cache.RetiredContentFiles.Clear();
        }

        private sealed partial class ImportContext
        {
            private void LoadNativeContentFiles()
            {
                if (manifest.nativeContentFiles == null || manifest.nativeContentFiles.Length == 0) return;
                if (nativeCache == null) throw new InvalidDataException("Direct native ContentFiles require a retained raw scene session.");
                var descriptors = new Dictionary<string, NativeContentFileDescriptor>(StringComparer.Ordinal);
                foreach (NativeContentFileDescriptor descriptor in manifest.nativeContentFiles)
                {
                    ValidateNativeContentDescriptor(descriptor);
                    if (descriptors.ContainsKey(descriptor.guid)) throw new InvalidDataException("Duplicate native ContentFile GUID: " + descriptor.guid);
                    descriptors.Add(descriptor.guid, descriptor);
                }
                var loaded = new Dictionary<string, NativeContentHandle>(StringComparer.Ordinal);
                var visiting = new HashSet<string>(StringComparer.Ordinal);
                var created = new List<NativeContentHandle>();
                try
                {
                    foreach (NativeContentFileDescriptor descriptor in manifest.nativeContentFiles)
                        LoadNativeContentDescriptor(descriptor, descriptors, loaded, visiting, created);
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
                    RegisterNativeMeshRetention(loaded.Values);
                }
                catch (Exception loadFailure)
                {
                    Exception cleanupFailure = null;
                    for (int index = created.Count - 1; index >= 0; index--)
                    {
                        NativeContentHandle failed = created[index];
                        RawCleanup.Run(() => NativeUIRuntime.Release(failed.OwnedUIObjects), ref cleanupFailure);
                        if (failed.File.IsValid)
                            RawCleanup.Run(() => failed.File.UnloadAsync().WaitForCompletion(10000), ref cleanupFailure);
                    }
                    if (cleanupFailure != null) throw new AggregateException("Native ContentFile load and cleanup failed.", loadFailure, cleanupFailure);
                    throw;
                }
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
                        && cached.UIIdentity == NativeUIRuntime.Identity(descriptor)
                        && NativeMeshRetentionMatches(cached, descriptor)
                        && NativeContentDependenciesMatch(cached.Dependencies, dependencies))
                    {
                        loaded.Add(descriptor.guid, cached);
                        return cached;
                    }
                    string path = AssetPath(manifest, descriptor.path);
                    RequireNativeContentHash(path, descriptor.sha256);
                    ValidateNativeContentResources(descriptor, path);
                    var handle = new NativeContentHandle { Hash = descriptor.sha256, Dependencies = dependencies, UIIdentity = NativeUIRuntime.Identity(descriptor) };
                    created.Add(handle);
                    int nativeErrors = 0;
                    string nativeFailure = string.Empty;
                    var errorLock = new object();
                    Application.LogCallback capture = (message, stack, type) =>
                    {
                        if (type != LogType.Error && type != LogType.Exception && type != LogType.Assert) return;
                        lock (errorLock) { nativeErrors++; if (nativeFailure.Length == 0) nativeFailure = message; }
                    };
                    Application.logMessageReceivedThreaded += capture;
                    try
                    {
                        handle.File = ContentLoadInterface.LoadContentFileAsync(ContentNamespace.Default, path, nativeDependencies);
                        if (!handle.File.IsValid || !handle.File.WaitForCompletion(10000) || handle.File.LoadingStatus != LoadingStatus.Completed)
                            throw new InvalidDataException("Native ContentFile failed to load: " + path);
                        foreach (NativeContentObjectDescriptor entry in descriptor.objects)
                        {
                            long localID = long.Parse(entry.fileId, NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture);
                            Object value = handle.File.GetObject(unchecked((ulong)localID));
                            if (value == null || value.GetType().FullName != entry.typeName)
                                throw new InvalidDataException("Native ContentFile identity/type readback failed: " + descriptor.guid + "/" + entry.fileId);
                            if (value is Shader shader && !shader.isSupported)
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
                        NativeUIRuntime.Prepare(descriptor, handle.Objects, (guid, fileID) =>
                        {
                            NativeContentHandle target = guid == descriptor.guid ? handle : loaded[guid];
                            if (!target.Objects.TryGetValue(AssetKey(guid, fileID), out Object value))
                                throw new InvalidDataException("Native UI target local ID was not published: " + guid + "/" + fileID);
                            return value;
                        }, handle.OwnedUIObjects);
                        InitializeNativeMeshRetention(handle, descriptor);
                        lock (errorLock)
                            if (nativeErrors != 0) throw new InvalidDataException("Native ContentFile emitted errors: " + nativeFailure);
                    }
                    finally { Application.logMessageReceivedThreaded -= capture; }
                    loaded.Add(descriptor.guid, handle);
                    return handle;
                }
                finally { nativeDependencies.Dispose(); visiting.Remove(descriptor.guid); }
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
                ValidateNativeMeshRetention(descriptor);
                NativeUIRuntime.Validate(descriptor);
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
                using (var stream = File.OpenRead(path))
                using (var hash = SHA256.Create())
                    if (RawReloadIdentity.Hex(hash.ComputeHash(stream)) != expected)
                        throw new InvalidDataException("Native content changed after publication: " + path);
            }
        }
    }
}
