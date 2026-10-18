using System;
using System.Collections.Generic;
using System.IO;
using System.Security.Cryptography;
using System.Reflection;
using UnityEngine;
using UnityEngine.Rendering;
using Object = UnityEngine.Object;

namespace UnityMicroEditor.Raw
{
    public static partial class RawSceneLoader
    {
        private static void InvokeRuntimeInitializers(RawSceneManifest manifest, int phase)
        {
            if (manifest.runtimeInitializers == null) return;
            foreach (RawRuntimeInitializer initializer in manifest.runtimeInitializers)
            {
                if (initializer == null || initializer.phase < 0 || initializer.phase > 4
                    || string.IsNullOrEmpty(initializer.assembly) || string.IsNullOrEmpty(initializer.typeName)
                    || string.IsNullOrEmpty(initializer.methodName) || !initializer.methodName.StartsWith("__UnityPlay_RuntimeInitialize_", StringComparison.Ordinal))
                    throw new InvalidDataException("Runtime initialization bridge descriptor is invalid.");
                if (initializer.phase != phase) continue;
                Assembly assembly = null;
                if (manifest.assemblies != null)
                    foreach (string path in manifest.assemblies)
                        if (Path.GetFileNameWithoutExtension(path) == initializer.assembly)
                        {
                            foreach (Assembly candidate in AppDomain.CurrentDomain.GetAssemblies())
                                if (candidate.GetName().Name == initializer.assembly) { assembly = candidate; break; }
                            break;
                        }
                if (assembly == null) throw new InvalidDataException("Runtime initializer assembly is not a supplied source assembly: " + initializer.assembly);
                Type type = assembly.GetType(initializer.typeName, false);
                MethodInfo method = type == null ? null : type.GetMethod(initializer.methodName, BindingFlags.Public | BindingFlags.Static | BindingFlags.DeclaredOnly);
                if (method == null || method.ReturnType != typeof(void) || method.GetParameters().Length != 0 || method.ContainsGenericParameters)
                    throw new InvalidDataException("Runtime initializer is not a public static parameterless compiler bridge: " + initializer.typeName + "." + initializer.methodName);
                var callback = (Action)Delegate.CreateDelegate(typeof(Action), method);
                try { callback(); }
                catch (Exception exception) { Debug.LogException(exception); }
            }
        }

        private static void ValidateImportedPacks(RawSceneManifest manifest, Dictionary<string, RawSceneAsset> assets)
        {
            bool imported = false;
            foreach (RawSceneAsset asset in assets.Values)
                if (asset.kind == "imported")
                {
                    imported = true;
                    if (IsNull(asset.fileId) || string.IsNullOrEmpty(asset.typeName))
                        throw new InvalidDataException("Imported asset requires its exact local file ID and native type: " + asset.path);
                }
            if (imported && (manifest.importedAssetPacks == null || manifest.importedAssetPacks.Length == 0)
                && (manifest.nativeContentFiles == null || manifest.nativeContentFiles.Length == 0))
                throw new InvalidDataException("Imported assets require a native importer pack.");
            if (manifest.importedAssetPacks == null) return;
            var paths = new HashSet<string>(StringComparer.Ordinal);
            foreach (ImportedAssetPack pack in manifest.importedAssetPacks)
            {
                if (pack == null || string.IsNullOrEmpty(pack.registryAsset) || pack.sha256 == null || pack.sha256.Length != 64)
                    throw new InvalidDataException("Native importer pack metadata is incomplete.");
                string path = AssetPath(manifest, pack.bundlePath);
                if (!paths.Add(path)) throw new InvalidDataException("Duplicate native importer pack: " + path);
                using (var hash = SHA256.Create())
                using (var stream = File.OpenRead(path))
                {
                    byte[] bytes = hash.ComputeHash(stream);
                    var actual = new char[bytes.Length * 2];
                    const string digits = "0123456789abcdef";
                    for (int index = 0; index < bytes.Length; index++)
                    { actual[index * 2] = digits[bytes[index] >> 4]; actual[index * 2 + 1] = digits[bytes[index] & 15]; }
                    if (!string.Equals(new string(actual), pack.sha256, StringComparison.OrdinalIgnoreCase))
                        throw new InvalidDataException("Native importer pack changed after publication: " + path);
                }
            }
        }

        private sealed partial class ImportContext
        {
            private void LoadPreloadedAssets()
            {
                if (manifest.preloadedAssets == null) { preloadedAssets = Array.Empty<Object>(); return; }
                preloadedAssets = new Object[manifest.preloadedAssets.Length];
                for (int index = 0; index < preloadedAssets.Length; index++)
                {
                    RawAssetReference reference = manifest.preloadedAssets[index];
                    preloadedAssets[index] = Resolve(new ReferenceData { guid = reference.guid, fileID = reference.fileId });
                }
            }

            private void InitializePreloadedRootScopes()
            {
                foreach (Object preload in preloadedAssets)
                {
                    if (preload == null || preload.GetType().FullName != "VContainer.Unity.VContainerSettings") continue;
                    Type settingsType = preload.GetType();
                    FieldInfo rootField = settingsType.GetField("RootLifetimeScope", BindingFlags.Public | BindingFlags.Instance);
                    MethodInfo createRoot = settingsType.GetMethod("GetOrCreateRootLifetimeScopeInstance", BindingFlags.Public | BindingFlags.Instance | BindingFlags.DeclaredOnly);
                    if (rootField == null || rootField.FieldType.FullName != "VContainer.Unity.LifetimeScope"
                        || createRoot == null || createRoot.GetParameters().Length != 0 || createRoot.ReturnType != rootField.FieldType)
                        throw new InvalidDataException("VContainer preload does not expose the supported public root-scope API.");
                    var root = rootField.GetValue(preload) as Component;
                    if (root == null) continue;
                    FieldInfo autoRun = rootField.FieldType.GetField("autoRun", BindingFlags.Public | BindingFlags.Instance);
                    if (autoRun == null || autoRun.FieldType != typeof(bool))
                        throw new InvalidDataException("VContainer root scope does not expose its public autoRun setting.");
                    if (!(bool)autoRun.GetValue(root)) continue;
                    // Unity runs this package's OnEnable while the native settings
                    // asset loads. Its authored prefab reference is restored later
                    // by our loader, so complete its public lazy initialization now.
                    try
                    {
                        var instance = createRoot.Invoke(preload, null) as Component;
                        if (instance == null) throw new InvalidDataException("VContainer did not create its preloaded root scope.");
                        preloadedRootScopes.Add(instance);
                    }
                    catch (TargetInvocationException exception)
                    { throw new InvalidOperationException("Preloaded VContainer root initialization failed.", exception.InnerException ?? exception); }
                }
            }

            internal void ReleasePreloadedRootScopes()
            {
                Exception failure = null;
                foreach (Component scope in preloadedRootScopes)
                {
                    if (scope == null) continue;
                    // VContainer exposes disposal publicly. Destroying the owned
                    // root now makes its native null test invalidate its cached
                    // instance before the next authored root is constructed.
                    try
                    {
                        MethodInfo dispose = scope.GetType().GetMethod("DisposeCore", BindingFlags.Public | BindingFlags.Instance);
                        if (dispose == null || dispose.GetParameters().Length != 0)
                            throw new InvalidDataException("VContainer root has no supported public disposal API.");
                        dispose.Invoke(scope, null);
                    }
                    catch (Exception exception) { if (failure == null) failure = exception; }
                    finally { if (scope != null) Object.DestroyImmediate(scope.gameObject); }
                }
                preloadedRootScopes.Clear();
                if (failure != null) throw new InvalidOperationException("Preloaded root disposal failed.", failure);
            }

            private void ConfigureImportedBindings()
            {
                if (manifest.importedAssetBindings == null) return;
                var patches = new Dictionary<Object, Dictionary<string, object>>();
                foreach (ImportedAssetBinding binding in manifest.importedAssetBindings)
                {
                    if (binding == null || string.IsNullOrEmpty(binding.ownerGuid) || IsNull(binding.ownerFileId)
                        || string.IsNullOrEmpty(binding.propertyPath)) throw new InvalidDataException("Native asset deferred binding is incomplete.");
                    Object owner = Resolve(new ReferenceData { guid = binding.ownerGuid, fileID = binding.ownerFileId });
                    if (!(owner is ScriptableObject))
                        throw new InvalidDataException("Native asset deferred binding must identify a ScriptableObject: " + binding.ownerGuid + "/" + binding.ownerFileId);
                    Object target = Resolve(new ReferenceData { guid = binding.targetGuid, fileID = binding.targetFileId });
                    if (!patches.TryGetValue(owner, out Dictionary<string, object> map))
                    {
                        string original = JsonUtility.ToJson(owner);
                        importedBindingSnapshots.Add(owner, original);
                        map = RawJson.Parse(original) as Dictionary<string, object>;
                        if (map == null) throw new InvalidDataException("Unity could not serialize native asset deferred binding owner.");
                        patches.Add(owner, map);
                    }
                    var wrapper = (Dictionary<string, object>)RawJson.Parse(JsonUtility.ToJson(new RuntimeReference { target = target }));
                    if (!wrapper.TryGetValue("target", out object serialized))
                        throw new InvalidDataException("Unity did not serialize the deferred binding target reference.");
                    ImportedAssetRuntimePaths.Set(map, binding.propertyPath, serialized);
                }
                foreach (var patch in patches) JsonUtility.FromJsonOverwrite(RawJson.Write(patch.Value), patch.Key);
            }

            private void LoadImportedAssets()
            {
                if (manifest.importedAssetPacks == null && manifest.nativeContentFiles == null) return;
                if (nativeCache != null && nativeCache.Loaded)
                {
                    importedBundles.AddRange(nativeCache.Bundles);
                    foreach (var entry in nativeCache.Assets) assets.Add(entry.Key, entry.Value);
                    if (manifest.renderPipelineGlobalSettings != null)
                        foreach (RawRenderPipelineGlobalSettings settings in manifest.renderPipelineGlobalSettings)
                            if (!RawGlobalSettings(settings))
                                RegisterPipelineGlobalSettings(settings, (RenderPipelineGlobalSettings)assets[AssetKey(settings.guid, settings.fileId)]);
                    LoadNativeContentFiles();
                    ValidateRequestedImportedAssets();
                    return;
                }
                foreach (ImportedAssetPack pack in manifest.importedAssetPacks ?? Array.Empty<ImportedAssetPack>())
                {
                    string path = AssetPath(manifest, pack.bundlePath);
                    AssetBundle bundle = AssetBundle.LoadFromFile(path, pack.crc);
                    if (bundle == null) throw new InvalidDataException("Unity refused native importer pack: " + path);
                    importedBundles.Add(bundle);
                    if (nativeCache != null) nativeCache.Bundles.Add(bundle);
                    if (bundle.isStreamedSceneAssetBundle || bundle.GetAllScenePaths().Length != 0)
                        throw new InvalidDataException("Native importer packs must not contain serialized scenes: " + path);
                }
                LoadImportedPipelineGlobalSettings();
                for (int packIndex = 0; manifest.importedAssetPacks != null && packIndex < manifest.importedAssetPacks.Length; packIndex++)
                {
                    ImportedAssetPack pack = manifest.importedAssetPacks[packIndex];
                    AssetBundle bundle = importedBundles[packIndex];
                    string path = AssetPath(manifest, pack.bundlePath);
                    ImportedAssetRegistry registry = bundle.LoadAsset<ImportedAssetRegistry>(pack.registryAsset);
                    if (registry == null || registry.schemaVersion != 1 || registry.entries == null)
                        throw new InvalidDataException("Native importer registry is missing or has an unsupported schema: " + path);
                    for (int entryIndex = 0; entryIndex < registry.entries.Length; entryIndex++)
                    {
                        ImportedAssetRegistry.Entry entry = registry.entries[entryIndex];
                        if (entry == null || string.IsNullOrEmpty(entry.guid) || IsNull(entry.fileId)
                            || string.IsNullOrEmpty(entry.typeName))
                            throw new InvalidDataException("Native importer registry entry " + entryIndex + " has invalid identity metadata: " + path);
                        if (entry.value == null)
                            throw new InvalidDataException("Native importer registry entry " + entryIndex + " has a missing object: "
                                + entry.guid + "/" + entry.fileId + " (" + entry.typeName + ") in " + path);
                        if (entry.value.GetType().FullName != entry.typeName)
                            throw new InvalidDataException("Native importer object type disagrees with its registry: " + entry.guid + "/" + entry.fileId);
                        string key = AssetKey(entry.guid, entry.fileId);
                        if (assets.TryGetValue(key, out Object present))
                        {
                            if (present != entry.value) throw new InvalidDataException("Native packs disagree on asset identity: " + key);
                        }
                        else assets.Add(key, entry.value);
                    }
                }
                LoadNativeContentFiles();
                if (nativeCache != null)
                {
                    foreach (var entry in assets) nativeCache.Assets[entry.Key] = entry.Value;
                    nativeCache.Loaded = true;
                    nativeCache.LoadCount++;
                }
                ValidateRequestedImportedAssets();
            }

            private void ValidateRequestedImportedAssets()
            {
                if (manifest.assets == null) return;
                foreach (RawSceneAsset asset in manifest.assets)
                {
                    if (asset.kind != "imported") continue;
                    if (!assets.TryGetValue(AssetKey(asset.guid, asset.fileId), out Object value)
                        || value == null || value.GetType().FullName != asset.typeName)
                        throw new InvalidDataException("Native importer pack is missing the requested object: " + asset.guid + "/" + asset.fileId);
                }
            }

            // Global settings published from their authored fields, as a Player
            // build serializes them, are a raw ScriptableObject: the pipeline
            // registers them once the raw objects exist (ConfigurePipeline).
            private bool RawGlobalSettings(RawRenderPipelineGlobalSettings settings)
            {
                foreach (RawSceneAsset asset in manifest.assets ?? Array.Empty<RawSceneAsset>())
                    if (asset.guid == settings.guid && asset.fileId == settings.fileId) return asset.kind == "scriptableObject";
                return false;
            }

            private void LoadImportedPipelineGlobalSettings()
            {
                if (manifest.renderPipelineGlobalSettings == null) return;
                foreach (RawRenderPipelineGlobalSettings settings in manifest.renderPipelineGlobalSettings)
                {
                    if (RawGlobalSettings(settings)) continue;
                    RawSceneAsset descriptor = null;
                    foreach (RawSceneAsset asset in manifest.assets)
                        if (asset.guid == settings.guid && asset.fileId == settings.fileId) { descriptor = asset; break; }
                    if (descriptor == null || descriptor.kind != "imported" || string.IsNullOrEmpty(descriptor.path))
                        throw new InvalidDataException("Native render pipeline global settings require an imported source asset descriptor.");
                    string name = descriptor.path.Replace('\\', '/').ToLowerInvariant();
                    RenderPipelineGlobalSettings globalSettings = null;
                    foreach (AssetBundle bundle in importedBundles)
                    {
                        if (!bundle.Contains(name)) continue;
                        var candidate = bundle.LoadAsset<RenderPipelineGlobalSettings>(name);
                        if (candidate == null) throw new InvalidDataException("Native global-settings source path does not contain a RenderPipelineGlobalSettings asset: " + name);
                        if (globalSettings != null && globalSettings != candidate)
                            throw new InvalidDataException("Native packs disagree on render pipeline global settings identity: " + name);
                        globalSettings = candidate;
                    }
                    if (globalSettings == null)
                        throw new InvalidDataException("Native importer cache must expose render pipeline global settings before its registry loads: " + name);
                    if (globalSettings.GetType().FullName != descriptor.typeName)
                        throw new InvalidDataException("Native global-settings type differs from its imported descriptor: " + name);
                    assets.Add(AssetKey(settings.guid, settings.fileId), globalSettings);
                    // Renderer features run native OnEnable during registry loading
                    // and request their shaders from these registered settings.
                    RegisterPipelineGlobalSettings(settings, globalSettings);
                }
            }

            // The GameObject of a record: a copy of its native template when
            // it has one, a new GameObject otherwise.
            private GameObject NewGameObject(RawSceneObject record, string name, bool rect)
            {
                if (string.IsNullOrEmpty(record.templateGuid))
                    return rect ? new GameObject(name, typeof(RectTransform)) : new GameObject(name);
                if (!assets.TryGetValue(AssetKey(record.templateGuid, record.templateFileId), out Object found) || !(found is GameObject template))
                    throw Error(record, "The native template of this GameObject was not loaded.");
                var instance = Object.Instantiate(template);
                instance.name = name;
                if (rect && !(instance.transform is RectTransform)) instance.AddComponent<RectTransform>();
                return instance;
            }

            private void CreateAuthoredComponents()
            {
                var records = new Dictionary<string, RawSceneObject>(StringComparer.Ordinal);
                foreach (RawSceneObject record in currentRecords) records.Add(record.fileId, record);
                var created = new HashSet<string>(StringComparer.Ordinal);
                foreach (RawSceneObject ownerRecord in currentRecords)
                {
                    if (ownerRecord.classId != 1) continue;
                    var data = Map(RawJson.Parse(ownerRecord.dataJson));
                    var ordered = new List<RawSceneObject>();
                    if (Value(data, "m_Component", null) is List<object> components)
                    {
                        foreach (object item in components)
                        {
                            ReferenceData reference = Reference(Value(Map(item), "component", item));
                            if (!string.IsNullOrEmpty(reference.guid) || !records.TryGetValue(reference.fileID, out RawSceneObject component)
                                || !IsComponent(component) || component.gameObjectId != ownerRecord.fileId)
                                throw Error(ownerRecord, "Authored component list contains an invalid owner or object reference.");
                            ordered.Add(component);
                        }
                    }
                    else foreach (RawSceneObject candidate in currentRecords)
                        if (IsComponent(candidate) && candidate.gameObjectId == ownerRecord.fileId) ordered.Add(candidate);
                    foreach (RawSceneObject record in ordered)
                    {
                        if (!created.Add(record.fileId)) throw Error(record, "Authored component list contains a duplicate component.");
                        if (IsTransform(record.classId)) continue;
                        GameObject owner = (GameObject)sceneObjects[record.gameObjectId];
                        if (record.templateComponent > 0)
                        {
                            Component[] made = owner.GetComponents<Component>();
                            Component held = record.templateComponent < made.Length ? made[record.templateComponent] : null;
                            if (held == null || held is Transform)
                                throw Error(record, "The native template of its GameObject does not hold this component.");
                            sceneObjects.Add(record.fileId, held);
                            continue;
                        }
                        if (record.classId == 114)
                        {
                            if (record.skipMissingScript) { sceneObjects.Add(record.fileId, null); continue; }
                            Type type = types[record.typeName];
                            foreach (RequireComponent requirement in type.GetCustomAttributes(typeof(RequireComponent), true))
                            {
                                VerifyRequiredComponent(record, requirement.m_Type0);
                                VerifyRequiredComponent(record, requirement.m_Type1);
                                VerifyRequiredComponent(record, requirement.m_Type2);
                            }
                            Component component = null;
                            foreach (Component candidate in owner.GetComponents<Component>())
                                if (candidate != null && candidate.GetType() == type && !sceneObjects.ContainsValue(candidate))
                                { component = candidate; break; }
                            if (component == null) component = owner.AddComponent(type);
                            if (component == null) throw Error(record, "Unity refused to create the authored component " + type.FullName);
                            sceneObjects.Add(record.fileId, component);
                            RawLifecycle.Register((MonoBehaviour)component);
                        }
                        else sceneObjects.Add(record.fileId, CreateComponent(owner, record));
                    }
                }
                foreach (RawSceneObject record in currentRecords)
                    if (IsComponent(record) && !created.Contains(record.fileId))
                        throw Error(record, "Authored GameObject component list omits this component.");
            }

            private void VerifyRequiredComponent(RawSceneObject record, Type required)
            {
                if (required == null) return;
                foreach (RawSceneObject candidate in currentRecords)
                {
                    if (candidate.gameObjectId != record.gameObjectId || !IsComponent(candidate)) continue;
                    if (candidate.skipMissingScript) continue;
                    Type actual = candidate.classId == 114 ? types[candidate.typeName]
                        : IsTransform(candidate.classId) ? (candidate.classId == 224 ? typeof(RectTransform) : typeof(Transform))
                        : NativeComponentType(candidate.classId);
                    if (actual != null && required.IsAssignableFrom(actual)) return;
                }
                throw Error(record, "Authored GameObject lacks its required component " + required.FullName);
            }
        }
    }
}
