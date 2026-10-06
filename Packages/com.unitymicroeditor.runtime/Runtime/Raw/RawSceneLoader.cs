using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Reflection;
using Unity.Collections;
using UnityEngine;
using UnityEngine.Rendering;
using UnityEngine.SceneManagement;
using Object = UnityEngine.Object;

namespace Dungeon.CLIPlay.Raw
{
    /// <summary>Imports the explicit raw-scene subset using public Player APIs.</summary>
    public static partial class RawSceneLoader
    {
        private const string BuiltinMeshGuid = "0000000000000000e000000000000000";
        private const string BuiltinResourceGuid = "0000000000000000f000000000000000";

        public static RawSceneManifest ReadManifest(string manifestPath)
        {
            if (string.IsNullOrEmpty(manifestPath) || !Path.IsPathRooted(manifestPath))
                throw new ArgumentException("Raw manifest path must be absolute.", nameof(manifestPath));
            return ParseManifest(File.ReadAllText(manifestPath));
        }

        private static RawSceneManifest ParseManifest(string text)
        {
            var manifest = JsonUtility.FromJson<RawSceneManifest>(text);
            if (manifest == null) throw new InvalidDataException("Empty raw scene manifest.");
            Validate(manifest);
            return manifest;
        }

        public static void Validate(string manifestPath) { ReadManifest(manifestPath); }

        public static void Validate(RawSceneManifest manifest)
        {
            if (manifest == null || manifest.schemaVersion != 1 || !manifest.supported)
                throw new InvalidDataException("Raw manifest is unsupported or has the wrong schema version.");
            if (manifest.unityVersion != Application.unityVersion)
                throw new InvalidDataException("Raw project and prepared host must use the same exact Unity version.");
            if (string.IsNullOrEmpty(manifest.projectRoot) || !Path.IsPathRooted(manifest.projectRoot)
                || !Directory.Exists(manifest.projectRoot)) throw new InvalidDataException("Raw project root is missing.");
            if (string.IsNullOrEmpty(manifest.sceneName) || manifest.objects == null)
                throw new InvalidDataException("Raw scene name or object records are missing.");
            if (manifest.diagnostics != null)
                foreach (var diagnostic in manifest.diagnostics)
                    if (diagnostic != null && diagnostic.severity == "error")
                        throw new InvalidDataException(diagnostic.code + ": " + diagnostic.message);
            var records = new Dictionary<string, RawSceneObject>(StringComparer.Ordinal);
            foreach (var record in manifest.objects)
            {
                if (record == null || string.IsNullOrEmpty(record.fileId) || records.ContainsKey(record.fileId))
                    throw new InvalidDataException("Scene object IDs must be nonempty and unique.");
                if (!SupportedClass(record.classId)) throw Error(record, "Unsupported class ID " + record.classId);
                if (!(RawJson.Parse(record.dataJson) is Dictionary<string, object>)) throw Error(record, "Object data must be a JSON map.");
                records.Add(record.fileId, record);
                if (record.classId == 114 && !record.skipMissingScript && string.IsNullOrEmpty(record.typeName)) throw Error(record, "Missing MonoBehaviour typeName.");
                if (record.skipMissingScript && (record.classId != 114 || !string.IsNullOrEmpty(record.typeName)))
                    throw Error(record, "Missing-script compatibility flag requires an unresolved MonoBehaviour type.");
            }
            var assets = new Dictionary<string, RawSceneAsset>(StringComparer.Ordinal);
            if (manifest.assets != null)
                foreach (var asset in manifest.assets)
                {
                    if (asset == null || string.IsNullOrEmpty(asset.guid)) throw new InvalidDataException("Asset GUID is missing.");
                    string key = AssetKey(asset.guid, asset.fileId);
                    if (assets.ContainsKey(key)) throw new InvalidDataException("Duplicate raw asset " + key);
                    if (asset.kind != "texture" && asset.kind != "material" && asset.kind != "authoredMaterial" && asset.kind != "script" && asset.kind != "scriptableObject" && asset.kind != "mesh" && asset.kind != "cubemap" && asset.kind != "imported" && asset.kind != "prefab")
                        throw new InvalidDataException("Unsupported asset " + asset.path + " (" + asset.kind + ").");
                    if (asset.kind == "texture")
                    {
                        string path = AssetPath(manifest, asset.path);
                        string extension = Path.GetExtension(path).ToLowerInvariant();
                        if (extension != ".png" && extension != ".jpg" && extension != ".jpeg")
                            throw new InvalidDataException("Only PNG/JPEG raw textures are supported: " + asset.path);
                        ValidateTextureImporter(asset);
                    }
                    if (asset.kind == "material")
                    {
                        if (asset.shaderName != "Standard" && asset.shaderName != "Unlit/Texture" && asset.shaderName != "Unlit/Color")
                            throw new InvalidDataException("Unsupported prepared shader " + asset.shaderName);
                        if (asset.materialMode != 0) throw new InvalidDataException("Only opaque materials are supported: " + asset.path);
                    }
                    if (asset.kind == "scriptableObject" && (asset.classId != 114 || string.IsNullOrEmpty(asset.typeName)))
                        throw new InvalidDataException("ScriptableObject type is missing: " + asset.path);
                    if (asset.kind == "mesh") RawAuthoredMesh.Validate(asset);
                    if (asset.kind == "authoredMaterial") RawAuthoredMaterial.Validate(asset);
                    if (asset.kind == "cubemap") RawAuthoredCubemap.Validate(asset);
                    assets.Add(key, asset);
                }
            AddPrefabValidationAliases(manifest, assets);
            foreach (var record in manifest.objects)
            {
                if (IsComponent(record.classId)
                    && (!records.TryGetValue(record.gameObjectId ?? "", out var owner) || owner.classId != 1))
                    throw Error(record, "Component owner is not a GameObject.");
                if (record.references != null)
                    foreach (var reference in record.references)
                        ValidateReference(record, reference, records, assets, manifest.emptyBuiltinLightingDataVerified);
                ValidateRecordSettings(record, manifest.emptyBuiltinLightingDataVerified);
                if (record.classId == 1)
                {
                    var data = ReadData<GameObjectData>(record.dataJson);
                    if (data.m_Layer < 0 || data.m_Layer > 31) throw Error(record, "GameObject layer must be between 0 and 31.");
                }
                if (IsComponent(record.classId))
                {
                    var map = (Dictionary<string, object>)RawJson.Parse(record.dataJson);
                    if (!map.TryGetValue("m_GameObject", out var ownerData) || !(ownerData is Dictionary<string, object> ownerReference)
                        || !ownerReference.TryGetValue("fileID", out object ownerId)
                        || Convert.ToString(ownerId, CultureInfo.InvariantCulture) != record.gameObjectId)
                        throw Error(record, "Component owner disagrees with its serialized m_GameObject reference.");
                }
            }
            foreach (var asset in assets.Values)
            {
                if (asset.kind == "material" && !string.IsNullOrEmpty(asset.textureGuid)
                    && !assets.ContainsKey(AssetKey(asset.textureGuid, asset.textureFileId)))
                    throw new InvalidDataException("Material texture is missing: " + asset.path);
                if ((asset.kind == "scriptableObject" || asset.kind == "authoredMaterial") && asset.references != null)
                    foreach (var reference in asset.references)
                        ValidateReference(AssetRecord(asset), reference, records, assets);
            }
            ValidateHierarchy(records);
            ValidatePrefabGraphs(manifest, assets);
            ValidateImportedPacks(manifest, assets);
            if (manifest.scriptableObjectBarrierVersion != 0 && manifest.scriptableObjectBarrierVersion != 1)
                throw new InvalidDataException("Unsupported ScriptableObject lifecycle instrumentation protocol.");
            if (manifest.importedAssetBindings != null)
                foreach (ImportedAssetBinding binding in manifest.importedAssetBindings)
                {
                    if (binding == null || string.IsNullOrEmpty(binding.propertyPath)
                        || !assets.TryGetValue(AssetKey(binding.ownerGuid, binding.ownerFileId), out RawSceneAsset owner) || owner.kind != "imported"
                        || (!IsNull(binding.targetFileId) && !assets.ContainsKey(AssetKey(binding.targetGuid, binding.targetFileId))))
                        throw new InvalidDataException("Native asset deferred binding has an unresolved owner or target.");
                }
            ValidateLifecycleProtocol(manifest);
            ActivationOrder(manifest);
            if (manifest.assemblies != null)
                foreach (string path in manifest.assemblies) AssetPath(manifest, path);
            if (!string.IsNullOrEmpty(manifest.renderPipelineGuid))
                if (!assets.ContainsKey(AssetKey(manifest.renderPipelineGuid, manifest.renderPipelineFileId)))
                    throw new InvalidDataException("Prepared render pipeline asset is missing.");
            if (manifest.preloadedAssets != null)
                foreach (RawAssetReference reference in manifest.preloadedAssets)
                    if (reference == null || string.IsNullOrEmpty(reference.fileId)
                        || (!IsNull(reference.fileId) && !assets.ContainsKey(AssetKey(reference.guid, reference.fileId))))
                        throw new InvalidDataException("Prepared preload asset is missing.");
            if (manifest.renderPipelineGlobalSettings != null)
            {
                var pipelines = new HashSet<string>(StringComparer.Ordinal);
                foreach (RawRenderPipelineGlobalSettings settings in manifest.renderPipelineGlobalSettings)
                    if (settings == null || string.IsNullOrEmpty(settings.pipelineTypeName)
                        || !pipelines.Add(settings.pipelineTypeName)
                        || !assets.ContainsKey(AssetKey(settings.guid, settings.fileId)))
                        throw new InvalidDataException("Prepared render pipeline global settings are missing or duplicated.");
            }
        }

        public static Scene Load(string manifestPath)
        {
            var manifest = ReadManifest(manifestPath);
            var types = LoadScriptTypes(manifest);
            InvokeRuntimeInitializers(manifest, 4);
            InvokeRuntimeInitializers(manifest, 2);
            InvokeRuntimeInitializers(manifest, 3);
            var context = new ImportContext(manifest, types);
            return context.Load();
        }

        private static T ReadData<T>(string json) where T : new()
        {
            var data = new T();
            JsonUtility.FromJsonOverwrite(json, data);
            return data;
        }

        private static CameraData ReadCameraData(string json)
        {
            var map = (Dictionary<string, object>)RawJson.Parse(json);
            RenameField(map, "near clip plane", "nearClipPlane"); RenameField(map, "far clip plane", "farClipPlane");
            RenameField(map, "field of view", "fieldOfView"); RenameField(map, "orthographic size", "orthographicSize");
            RenameField(map, "m_projectionMatrixMode", "ProjectionMatrixMode");
            return ReadData<CameraData>(RawJson.Write(map));
        }

        private static void RenameField(Dictionary<string, object> map, string original, string replacement)
        { if (map.TryGetValue(original, out object value)) { map.Remove(original); map[replacement] = value; } }

        private static Dictionary<string, Type> LoadScriptTypes(RawSceneManifest manifest)
        {
            var assemblies = new List<Assembly>();
            if (manifest.assemblies != null)
                foreach (string path in manifest.assemblies)
                {
                    string fullPath = AssetPath(manifest, path);
                    if (Path.GetExtension(fullPath) != ".dll") throw new InvalidDataException("Script assembly is not a DLL: " + path);
                    var assembly = UnityEngine.Assemblies.CurrentAssemblies.LoadFromPath(fullPath);
                    if (manifest.lifecycleBarrierVersion == 1)
                    {
                        var marker = Attribute.GetCustomAttribute(assembly, typeof(RawLifecycleInstrumentedAttribute)) as RawLifecycleInstrumentedAttribute;
                        if (marker == null || marker.Version != 1)
                            throw new InvalidDataException("Raw assembly lacks the verified lifecycle instrumentation marker: " + path);
                    }
                    assemblies.Add(assembly);
                }
            var types = new Dictionary<string, Type>(StringComparer.Ordinal);
            foreach (var assembly in AppDomain.CurrentDomain.GetAssemblies())
                if (!assemblies.Contains(assembly)) assemblies.Add(assembly);
            foreach (var record in AllScriptRecords(manifest))
            {
                if (record.classId != 114 || record.skipMissingScript || types.ContainsKey(record.typeName)) continue;
                Type type = null;
                foreach (var assembly in assemblies)
                {
                    Type candidate = assembly.GetType(record.typeName, false);
                    if (candidate == null) continue;
                    if (type != null && candidate != type) throw Error(record, "Ambiguous script type " + record.typeName);
                    type = candidate;
                }
                if (type == null) type = LoadHostScriptType(record.typeName, assemblies);
                if (type == null || type.IsAbstract || type.ContainsGenericParameters || !typeof(MonoBehaviour).IsAssignableFrom(type))
                    throw Error(record, "Script type is not a concrete MonoBehaviour in the supplied assemblies: " + record.typeName);
                types.Add(record.typeName, type);
            }
            if (manifest.assets != null)
                foreach (var asset in manifest.assets)
                {
                    if (asset.kind != "scriptableObject") continue;
                    Type type = null;
                    foreach (var assembly in assemblies)
                    {
                        Type candidate = assembly.GetType(asset.typeName, false);
                        if (candidate == null) continue;
                        if (type != null && candidate != type) throw new InvalidDataException("Ambiguous ScriptableObject type " + asset.typeName);
                        type = candidate;
                    }
                    if (type == null) type = LoadHostScriptType(asset.typeName, assemblies);
                    if (type == null || type.IsAbstract || type.ContainsGenericParameters || !typeof(ScriptableObject).IsAssignableFrom(type))
                        throw new InvalidDataException("Asset type is not a concrete ScriptableObject in supplied assemblies: " + asset.typeName);
                    if (manifest.scriptableObjectBarrierVersion == 1)
                    {
                        var marker = Attribute.GetCustomAttribute(type.Assembly, typeof(RawScriptableObjectInstrumentedAttribute)) as RawScriptableObjectInstrumentedAttribute;
                        if ((marker == null || marker.Version != 1) && !TrustedHostScriptableObject(type))
                            throw new InvalidDataException("Raw ScriptableObject assembly lacks the verified lifecycle instrumentation marker: " + asset.typeName);
                    }
                    types[asset.typeName] = type;
                }
            if (manifest.assets != null)
                foreach (RawSceneAsset asset in manifest.assets)
                    if (asset.kind == "script" && asset.executionOrder != 0 && !string.IsNullOrEmpty(asset.typeName))
                    {
                        Type type = null;
                        foreach (Assembly assembly in assemblies)
                        {
                            Type candidate = assembly.GetType(asset.typeName, false);
                            if (candidate == null) continue;
                            if (type != null && type != candidate) throw new InvalidDataException("Ambiguous script execution-order type: " + asset.typeName);
                            type = candidate;
                        }
                        if (type != null && typeof(MonoBehaviour).IsAssignableFrom(type)) RawLifecycle.SetExecutionOrder(type, asset.executionOrder);
                    }
            return types;
        }

        private static Type LoadHostScriptType(string typeName, List<Assembly> loaded)
        {
            if (typeName == "UnityEngine.UIElements.UIDocument") return typeof(UnityEngine.UIElements.UIDocument);
            if (typeName == "UnityEngine.UIElements.PanelSettings") return typeof(UnityEngine.UIElements.PanelSettings);
            string managed = Path.GetDirectoryName(typeof(RawSceneLoader).Assembly.Location);
            string metadata = string.IsNullOrEmpty(managed) ? null : Path.GetFullPath(Path.Combine(managed, "..", "..", "host.json"));
            if (metadata == null || !File.Exists(metadata)) return null;
            HostScriptTypeManifest host = JsonUtility.FromJson<HostScriptTypeManifest>(File.ReadAllText(metadata));
            if (host == null || host.providedPackageAssemblies == null) return null;
            var candidates = new List<string>(host.providedPackageAssemblies);
            if (typeName.StartsWith("TMPro.", StringComparison.Ordinal) && candidates.Remove("Unity.TextMeshPro")) candidates.Insert(0, "Unity.TextMeshPro");
            if (typeName.StartsWith("UnityEngine.UI.", StringComparison.Ordinal) && candidates.Remove("UnityEngine.UI")) candidates.Insert(0, "UnityEngine.UI");
            foreach (string name in candidates)
            {
                if (string.IsNullOrEmpty(name) || name != Path.GetFileName(name)) throw new InvalidDataException("Host package assembly identity is invalid.");
                string path = Path.Combine(managed, name + ".dll");
                if (!File.Exists(path)) continue;
                Assembly assembly = null;
                foreach (Assembly present in loaded) if (present.GetName().Name == name) { assembly = present; break; }
                if (assembly == null)
                {
                    assembly = UnityEngine.Assemblies.CurrentAssemblies.LoadFromPath(path);
                    loaded.Add(assembly);
                }
                Type type = assembly.GetType(typeName, false);
                if (type != null) return type;
            }
            return null;
        }

        // These pinned engine/package assets initialize empty containers in
        // OnEnable. Their public caches are verified after restoring authored
        // fields. User ScriptableObjects still require the lifecycle barrier.
        private static bool TrustedHostScriptableObject(Type type)
        {
            if (type == typeof(UnityEngine.UIElements.PanelSettings)) return true;
            string assembly = type.Assembly.GetName().Name;
            // NGO's pinned NetworkPrefabsList has no lifecycle callbacks.
            if (type.FullName == "Unity.Netcode.NetworkPrefabsList" && assembly == "Unity.Netcode.Runtime") return true;
            if (type == typeof(VolumeProfile) && assembly == "Unity.RenderPipelines.Core.Runtime") return true;
            return typeof(VolumeComponent).IsAssignableFrom(type)
                && (assembly == "Unity.RenderPipelines.Core.Runtime" || assembly == "Unity.RenderPipelines.Universal.Runtime");
        }

        [Serializable] private sealed class HostScriptTypeManifest { public string[] providedPackageAssemblies; }

        private static void ValidateLifecycleProtocol(RawSceneManifest manifest)
        {
            if (manifest.lifecycleBarrierVersion == 0)
            {
                if (manifest.lifecycleBarrierAssemblies != null && manifest.lifecycleBarrierAssemblies.Length != 0)
                    throw new InvalidDataException("Lifecycle assembly verification requires protocol version 1.");
                return;
            }
            if (manifest.lifecycleBarrierVersion != 1 || manifest.assemblies == null || manifest.lifecycleBarrierAssemblies == null)
                throw new InvalidDataException("Unsupported or incomplete raw lifecycle instrumentation protocol.");
            var supplied = new HashSet<string>(manifest.assemblies, StringComparer.Ordinal);
            var verified = new HashSet<string>(manifest.lifecycleBarrierAssemblies, StringComparer.Ordinal);
            if (supplied.Count != manifest.assemblies.Length || verified.Count != manifest.lifecycleBarrierAssemblies.Length
                || !supplied.SetEquals(verified))
                throw new InvalidDataException("Every supplied assembly must be uniquely verified for all raw lifecycle phases.");
        }

        private static RawSceneObject AssetRecord(RawSceneAsset asset)
        { return new RawSceneObject { fileId = asset.fileId, sourcePath = asset.path, classId = asset.classId }; }

        private static void ValidateReference(RawSceneObject record, RawSceneReference reference,
            Dictionary<string, RawSceneObject> records, Dictionary<string, RawSceneAsset> assets, bool emptyLightingDataVerified = false)
        {
            if (reference == null || IsNull(reference.fileId)) return;
            // Script assets select a managed type; they are not runtime Unity objects.
            if (record.classId == 114 && reference.property == "m_Script") return;
            if (string.IsNullOrEmpty(reference.guid))
            {
                if (!records.ContainsKey(reference.fileId)) throw Error(record, "Unresolved scene reference " + reference.property + ": " + reference.fileId);
                return;
            }
            if (reference.guid == BuiltinMeshGuid && Primitive(reference.fileId, out _)) return;
            if (reference.guid == BuiltinResourceGuid && reference.fileId == "10303") return;
            if (record.classId == 21 && reference.property == "m_Shader" && reference.guid == BuiltinResourceGuid && reference.fileId == "108") return;
            if (record.classId == 104 && reference.guid == BuiltinResourceGuid && reference.fileId == "10304") return;
            if (record.classId == 104 && reference.property == "m_SpotCookie" && reference.guid == BuiltinMeshGuid && reference.fileId == "10001") return;
            if (emptyLightingDataVerified && record.classId == 157 && reference.property == "m_LightingDataAsset"
                && reference.guid == BuiltinResourceGuid && reference.fileId == "20201") return;
            if (!assets.TryGetValue(AssetKey(reference.guid, reference.fileId), out var asset) || asset.kind == "script")
                throw Error(record, "Unresolved asset reference " + reference.property + ": " + reference.guid + "/" + reference.fileId);
        }

        private static void ValidateHierarchy(Dictionary<string, RawSceneObject> records)
        {
            var owners = new HashSet<string>(StringComparer.Ordinal);
            foreach (var record in records.Values)
            {
                if (!IsTransform(record.classId)) continue;
                if (!owners.Add(record.gameObjectId)) throw Error(record, "GameObject contains duplicate Transforms.");
                var seen = new HashSet<string>(StringComparer.Ordinal);
                var current = record;
                while (true)
                {
                    if (!seen.Add(current.fileId)) throw Error(record, "Transform hierarchy contains a cycle.");
                    var transform = ReadData<TransformData>(current.dataJson);
                    if (transform.m_Father == null || IsNull(transform.m_Father.fileID)) break;
                    if (!string.IsNullOrEmpty(transform.m_Father.guid) || !records.TryGetValue(transform.m_Father.fileID, out current)
                        || !IsTransform(current.classId)) throw Error(record, "Transform parent is not a scene Transform.");
                }
                var parent = ReadData<TransformData>(record.dataJson);
                if (parent.m_Father != null && !IsNull(parent.m_Father.fileID))
                {
                    var father = ReadData<TransformData>(records[parent.m_Father.fileID].dataJson);
                    bool listed = false;
                    if (father.m_Children != null)
                        foreach (var reference in father.m_Children)
                            if (reference != null && reference.fileID == record.fileId) listed = true;
                    if (!listed) throw Error(record, "Transform parent does not list this child.");
                }
                if (parent.m_Children != null)
                {
                    var children = new HashSet<string>(StringComparer.Ordinal);
                    foreach (var reference in parent.m_Children)
                    {
                        if (reference == null || !children.Add(reference.fileID)
                            || !records.TryGetValue(reference.fileID, out var child) || !IsTransform(child.classId))
                            throw Error(record, "Transform child is missing, duplicated or not a Transform.");
                        var data = ReadData<TransformData>(child.dataJson);
                        if (data.m_Father == null || data.m_Father.fileID != record.fileId)
                            throw Error(record, "Transform children/parent records disagree.");
                    }
                }
            }
            foreach (var record in records.Values)
                if (record.classId == 1 && !owners.Contains(record.fileId)) throw Error(record, "GameObject has no Transform.");
        }

        private static string[] ActivationOrder(RawSceneManifest manifest)
        {
            var records = new Dictionary<string, RawSceneObject>(StringComparer.Ordinal);
            var transforms = new Dictionary<string, RawSceneObject>(StringComparer.Ordinal);
            foreach (var record in manifest.objects)
            {
                records.Add(record.fileId, record);
                if (IsTransform(record.classId)) transforms.Add(record.gameObjectId, record);
            }
            var rootByObject = new Dictionary<string, string>(StringComparer.Ordinal);
            var activeByObject = new Dictionary<string, bool>(StringComparer.Ordinal);
            var roots = new List<string>();
            foreach (var record in manifest.objects)
            {
                if (record.classId != 1) continue;
                string owner = record.fileId;
                bool active = ReadData<GameObjectData>(record.dataJson).m_IsActive != 0;
                string root = owner;
                var current = transforms[owner];
                while (true)
                {
                    var data = ReadData<TransformData>(current.dataJson);
                    if (data.m_Father == null || IsNull(data.m_Father.fileID)) break;
                    current = records[data.m_Father.fileID]; root = current.gameObjectId;
                    active &= ReadData<GameObjectData>(records[root].dataJson).m_IsActive != 0;
                }
                rootByObject.Add(owner, root); activeByObject.Add(owner, active);
                if (root == owner && active) roots.Add(owner);
            }
            var scriptRoots = new HashSet<string>(StringComparer.Ordinal);
            var prerequisites = new Dictionary<string, HashSet<string>>(StringComparer.Ordinal);
            foreach (string root in roots) prerequisites.Add(root, new HashSet<string>(StringComparer.Ordinal));
            foreach (var record in manifest.objects)
            {
                string owner = record.classId == 1 ? record.fileId : record.gameObjectId;
                if (string.IsNullOrEmpty(owner) || !activeByObject.TryGetValue(owner, out bool active) || !active) continue;
                string root = rootByObject[owner];
                if (record.classId == 114) scriptRoots.Add(root);
                if (record.references == null) continue;
                foreach (var reference in record.references)
                {
                    if (reference == null || !string.IsNullOrEmpty(reference.guid) || IsNull(reference.fileId)
                        || !records.TryGetValue(reference.fileId, out var target)) continue;
                    string targetOwner = target.classId == 1 ? target.fileId : target.gameObjectId;
                    if (string.IsNullOrEmpty(targetOwner) || !activeByObject.TryGetValue(targetOwner, out bool targetActive) || !targetActive) continue;
                    string dependency = rootByObject[targetOwner];
                    if (dependency != root) prerequisites[root].Add(dependency);
                }
            }
            return RawActivationPlanner.Order(roots, scriptRoots, prerequisites, manifest.lifecycleBarrierVersion == 1);
        }

        private static void ValidateRecordSettings(RawSceneObject record, bool emptyLightingDataVerified = false)
        {
            if (record.classId == 20)
            {
                var data = ReadCameraData(record.dataJson);
                if (data.ProjectionMatrixMode != 1 && data.ProjectionMatrixMode != 0) throw Error(record, "Physical camera projection is unsupported.");
            }
            if (record.classId == 108)
            {
                var data = ReadData<LightData>(record.dataJson);
                if (data.m_Type < 0 || data.m_Type > 2) throw Error(record, "Only Spot, Directional and Point lights are supported.");
                if (data.m_Lightmapping != 4) throw Error(record, "Only realtime lights are supported.");
            }
            if (record.classId == 23 || record.classId == 120 || record.classId == 137 || record.classId == 199)
            {
                var data = ReadData<RendererData>(record.dataJson);
                if (data.m_LightmapIndex >= 0 && data.m_LightmapIndex < 65534) throw Error(record, "Baked lightmaps are unsupported.");
            }
            if (record.classId == 29 || record.classId == 157 || record.classId == 196)
            {
                if (record.references != null)
                    foreach (var reference in record.references)
                        if (reference != null && !IsNull(reference.fileId))
                        {
                            if (emptyLightingDataVerified && record.classId == 157 && reference.property == "m_LightingDataAsset"
                                && reference.guid == BuiltinResourceGuid && reference.fileId == "20201") continue;
                            throw Error(record, "Baked occlusion, lightmap and navigation data are unsupported.");
                        }
            }
        }

        private static void ValidateTextureImporter(RawSceneAsset asset)
        {
            if (string.IsNullOrEmpty(asset.importerJson)) return;
            var data = ReadData<TextureImporterData>(asset.importerJson);
            if (data.textureType != 0 || data.textureShape != 1)
                throw new InvalidDataException("Only default 2D texture imports are supported: " + asset.path);
        }

        private static bool SupportedClass(int classId)
        {
            return classId == 1 || classId == 4 || classId == 20 || classId == 23 || classId == 33 || classId == 54
                || classId == 65 || classId == 108 || classId == 114 || classId == 135 || classId == 136
                || classId == 29 || classId == 104 || classId == 157 || classId == 196 || classId == 1660057539
                || NativeComponentType(classId) != null;
        }

        private static bool IsComponent(int classId)
        {
            return classId == 4 || classId == 20 || classId == 23 || classId == 33 || classId == 54
                || classId == 65 || classId == 108 || classId == 114 || classId == 135 || classId == 136
                || NativeComponentType(classId) != null;
        }

        private static bool IsTransform(int classId) { return classId == 4 || classId == 224; }

        private static string AssetPath(RawSceneManifest manifest, string path)
        {
            if (string.IsNullOrEmpty(path)) throw new InvalidDataException("Raw asset path is empty.");
            string result = Path.GetFullPath(Path.IsPathRooted(path) ? path : Path.Combine(manifest.projectRoot, path));
            if (!File.Exists(result)) throw new FileNotFoundException("Raw input file is missing.", result);
            return result;
        }
        private static string AssetKey(string guid, string fileId) { return guid + ":" + (fileId ?? ""); }
        private static bool IsNull(string fileId) { return string.IsNullOrEmpty(fileId) || fileId == "0"; }
        private static InvalidDataException Error(RawSceneObject record, string message)
        { return new InvalidDataException(record.sourcePath + " [" + record.fileId + "]: " + message); }
        private static bool Primitive(string id, out PrimitiveType type)
        {
            switch (id)
            {
                case "10202": type = PrimitiveType.Cube; return true;
                case "10206": type = PrimitiveType.Cylinder; return true;
                case "10207": type = PrimitiveType.Sphere; return true;
                case "10208": type = PrimitiveType.Capsule; return true;
                case "10209": type = PrimitiveType.Plane; return true;
                case "10210": type = PrimitiveType.Quad; return true;
                default: type = PrimitiveType.Cube; return false;
            }
        }

        private sealed partial class ImportContext
        {
            private readonly RawSceneManifest manifest;
            private readonly Dictionary<string, Type> types;
            private readonly NativeAssetCache nativeCache;
            private readonly List<Component> preloadedRootScopes = new List<Component>();
            private readonly RawOwnedObjects<GameObject> ownedSceneObjects = new RawOwnedObjects<GameObject>();
            private readonly Dictionary<Object, string> importedBindingSnapshots = new Dictionary<Object, string>();
            private Dictionary<string, Object> sceneObjects = new Dictionary<string, Object>(StringComparer.Ordinal);
            private RawSceneObject[] currentRecords;
            private GameObject resourceLifetime;
            private readonly Dictionary<string, Object> assets = new Dictionary<string, Object>(StringComparer.Ordinal);
            private readonly Dictionary<string, Mesh> builtinMeshes = new Dictionary<string, Mesh>(StringComparer.Ordinal);
            private readonly List<Object> ownedAssets = new List<Object>();
            private readonly List<AssetBundle> importedBundles = new List<AssetBundle>();
            private readonly List<Transform> registeredPrefabTransforms = new List<Transform>();
            private Object[] preloadedAssets;
            private readonly string[] activationOrder;
            private Scene scene;

            public ImportContext(RawSceneManifest manifest, Dictionary<string, Type> types, NativeAssetCache nativeCache = null)
            { this.manifest = manifest; this.types = types; this.nativeCache = nativeCache; activationOrder = ActivationOrder(manifest); currentRecords = manifest.objects; }

            public Scene Load(bool runInitializers = true, Action commit = null, bool initializePreloads = true)
            {
                Scene previous = SceneManager.GetActiveScene();
                RawLifecycle.Scope lifecycle = manifest.lifecycleBarrierVersion == 1 ? RawLifecycle.Begin() : null;
                RawLifecycle.ScriptableObjectScope scriptableLifecycle = RawLifecycle.BeginScriptableObjects();
                try
                {
                    // CreateScene refuses an already loaded scene name. The
                    // authored name is restored through Scene's public setter
                    // before any source component or lifecycle callback exists.
                    string creationName = SceneManager.GetSceneByName(manifest.sceneName).IsValid()
                        ? "UnityPlay staging " + Guid.NewGuid().ToString("N") : manifest.sceneName;
                    scene = SceneManager.CreateScene(creationName);
                    scene.name = manifest.sceneName;
                    LoadAssets();
                    if (initializePreloads) LoadPreloadedAssets();
                    ConfigurePipeline();
                    resourceLifetime = new GameObject("Raw scene imported assets");
                    resourceLifetime.SetActive(false);
                    SceneManager.MoveGameObjectToScene(resourceLifetime, scene);
                    resourceLifetime.AddComponent<RawSceneResources>();
                    CreatePrefabTemplates();
                    foreach (var record in manifest.objects)
                    {
                        if (record.classId != 1) continue;
                        var data = ReadData<GameObjectData>(record.dataJson);
                        bool rect = false;
                        foreach (var componentRecord in manifest.objects)
                            if (componentRecord.classId == 224 && componentRecord.gameObjectId == record.fileId) { rect = true; break; }
                        var gameObject = rect ? new GameObject(data.m_Name ?? "Raw GameObject", typeof(RectTransform)) : new GameObject(data.m_Name ?? "Raw GameObject");
                        ownedSceneObjects.Add(gameObject);
                        gameObject.SetActive(false);
                        SceneManager.MoveGameObjectToScene(gameObject, scene);
                        gameObject.layer = data.m_Layer;
                        if (!string.IsNullOrEmpty(data.m_TagString) && data.m_TagString != "Untagged") gameObject.tag = data.m_TagString;
                        gameObject.isStatic = data.m_StaticEditorFlags != 0;
                        sceneObjects.Add(record.fileId, gameObject);
                    }
                    // Bind existing native Transforms before adding the remaining components.
                    foreach (var record in manifest.objects)
                        if (IsTransform(record.classId)) sceneObjects.Add(record.fileId, ((GameObject)sceneObjects[record.gameObjectId]).transform);
                    // Establish hierarchy before custom scripts exist: even an inactive
                    // component can receive transform-change messages from Unity.
                    foreach (var record in manifest.objects)
                        if (IsTransform(record.classId)) ConfigureTransform(record);
                    foreach (var record in manifest.objects)
                        if (IsTransform(record.classId)) ConfigureTransformChildren(record);
                    CreateAuthoredComponents();
                    foreach (var record in manifest.objects)
                        ConfigureComponent(record);
                    if (manifest.assets != null)
                        foreach (var asset in manifest.assets)
                            if (asset.kind == "scriptableObject") ConfigureSerializedObject(asset.dataJson, assets[AssetKey(asset.guid, asset.fileId)], false);
                    VerifyHostVolumeCaches();
                    ConfigurePrefabTemplates();
                    ConfigureImportedBindings();
                    RestoreNativeMeshReadability();
                    scriptableLifecycle.Complete();
                    if (commit != null) commit();
                    if (!SceneManager.SetActiveScene(scene)) throw new InvalidOperationException("Unity refused to activate the raw scene.");
                    foreach (var record in manifest.objects)
                        if (record.classId == 104) ConfigureRenderSettings(record);
                    if (!resourceLifetime.TryGetComponent<RawSceneResources>(out var resources))
                        throw new InvalidOperationException("Raw asset lifetime owner is missing.");
                    resources.Initialize(ownedAssets.ToArray(), nativeCache == null ? importedBundles.ToArray() : null, preloadedAssets, registeredPrefabTransforms.ToArray());
                    resourceLifetime.SetActive(true);
                    if (runInitializers) InvokeRuntimeInitializers(manifest, 1);
                    if (initializePreloads) InitializePreloadedRootScopes();
                    // Set descendant activeSelf bits while their actual roots are
                    // inactive, then activate roots through Unity's public batch API.
                    // No temporary parents are visible to Awake or transform messages.
                    foreach (var record in manifest.objects)
                    {
                        if (record.classId != 1) continue;
                        var gameObject = (GameObject)sceneObjects[record.fileId];
                        bool active = ReadData<GameObjectData>(record.dataJson).m_IsActive != 0;
                        if (gameObject.transform.parent != null) gameObject.SetActive(active);
                    }
                    var activeRoots = new EntityId[activationOrder.Length];
                    for (int index = 0; index < activationOrder.Length; index++)
                        activeRoots[index] = ((GameObject)sceneObjects[activationOrder[index]]).GetEntityId();
                    using (var ids = new NativeArray<EntityId>(activeRoots, Allocator.Temp))
                        GameObject.SetGameObjectsActive(ids, true);
                    if (lifecycle != null) lifecycle.Complete();
                    if (runInitializers) InvokeRuntimeInitializers(manifest, 0);
                    return scene;
                }
                catch (Exception loadFailure)
                {
                    Exception cleanupFailure = null;
                    RawCleanup.Run(RestoreNativeMeshReadability, ref cleanupFailure);
                    RawCleanup.Run(ReleaseOwnedSceneObjects, ref cleanupFailure);
                    RawCleanup.Run(ReleasePreloadedRootScopes, ref cleanupFailure);
                    foreach (var snapshot in importedBindingSnapshots)
                        if (snapshot.Key != null) JsonUtility.FromJsonOverwrite(snapshot.Value, snapshot.Key);
                    RawPrefabTransforms.Unregister(registeredPrefabTransforms.ToArray());
                    if (lifecycle != null) lifecycle.Dispose();
                    scriptableLifecycle.Dispose();
                    if (previous.IsValid() && previous.isLoaded) SceneManager.SetActiveScene(previous);
                    if (scene.IsValid() && scene.isLoaded) SceneManager.UnloadSceneAsync(scene);
                    foreach (Object asset in ownedAssets) if (asset != null) Object.Destroy(asset);
                    if (nativeCache == null) foreach (AssetBundle bundle in importedBundles) if (bundle != null) bundle.Unload(true);
                    if (cleanupFailure != null)
                        throw new AggregateException("Raw load and owned object rollback failed.", loadFailure, cleanupFailure);
                    throw;
                }
                finally { if (lifecycle != null) lifecycle.Dispose(); scriptableLifecycle.Dispose(); }
            }

            internal bool TryGetPrefab(string guid, out GameObject prefab)
            {
                prefab = null;
                foreach (RawSceneAsset asset in manifest.assets ?? Array.Empty<RawSceneAsset>())
                {
                    if (asset.guid != guid || asset.kind != "prefab") continue;
                    if (!assets.TryGetValue(AssetKey(asset.guid, asset.fileId), out Object value) || !(value is GameObject root))
                        throw new InvalidDataException("Raw runtime prefab is unavailable: " + guid);
                    prefab = root;
                    return true;
                }
                return false;
            }

            internal void ReleaseOwnedSceneObjects()
            {
                // Source callbacks may still use scene resources during shutdown.
                GameObject resources = resourceLifetime;
                resourceLifetime = null;
                try
                {
                    ownedSceneObjects.Release(value => value != null,
                        value => value.SetActive(false), value => Object.DestroyImmediate(value));
                }
                finally
                {
                    if (resources != null) Object.DestroyImmediate(resources);
                }
            }

            private void LoadAssets()
            {
                LoadImportedAssets();
                if (manifest.assets == null) return;
                foreach (var asset in manifest.assets)
                {
                    if (asset.kind != "mesh") continue;
                    Mesh mesh = RawAuthoredMesh.Create(asset);
                    ownedAssets.Add(mesh);
                    assets.Add(AssetKey(asset.guid, asset.fileId), mesh);
                }
                foreach (var asset in manifest.assets)
                {
                    if (asset.kind != "cubemap") continue;
                    Cubemap cubemap = RawAuthoredCubemap.Create(asset);
                    ownedAssets.Add(cubemap);
                    assets.Add(AssetKey(asset.guid, asset.fileId), cubemap);
                }
                foreach (var asset in manifest.assets)
                {
                    if (asset.kind != "texture") continue;
                    var importer = string.IsNullOrEmpty(asset.importerJson) ? new TextureImporterData() : ReadData<TextureImporterData>(asset.importerJson);
                    var texture = DecodeTexture(asset, importer);
                    texture.name = string.IsNullOrEmpty(asset.name) ? Path.GetFileNameWithoutExtension(asset.path) : asset.name;
                    texture.filterMode = (FilterMode)importer.filterMode;
                    texture.wrapModeU = (TextureWrapMode)importer.wrapU;
                    texture.wrapModeV = (TextureWrapMode)importer.wrapV;
                    texture.anisoLevel = importer.aniso;
                    assets.Add(AssetKey(asset.guid, asset.fileId), texture);
                }
                foreach (var asset in manifest.assets)
                {
                    if (asset.kind != "material") continue;
                    Shader shader = Shader.Find(asset.shaderName);
                    if (shader == null || !shader.isSupported) throw new InvalidDataException("Prepared host has no supported shader " + asset.shaderName);
                    var material = new Material(shader) { name = string.IsNullOrEmpty(asset.name) ? Path.GetFileNameWithoutExtension(asset.path) : asset.name, color = asset.color };
                    ownedAssets.Add(material);
                    if (!string.IsNullOrEmpty(asset.textureGuid)) material.mainTexture = (Texture)assets[AssetKey(asset.textureGuid, asset.textureFileId)];
                    material.mainTextureScale = asset.textureScale;
                    material.mainTextureOffset = asset.textureOffset;
                    assets.Add(AssetKey(asset.guid, asset.fileId), material);
                }
                var authoredMaterials = new Dictionary<string, RawSceneAsset>(StringComparer.Ordinal);
                foreach (var asset in manifest.assets)
                    if (asset.kind == "authoredMaterial") authoredMaterials.Add(AssetKey(asset.guid, asset.fileId), asset);
                var creatingMaterials = new HashSet<string>(StringComparer.Ordinal);
                foreach (var asset in manifest.assets)
                    if (asset.kind == "authoredMaterial") LoadAuthoredMaterial(asset, authoredMaterials, creatingMaterials);
                foreach (var asset in manifest.assets)
                {
                    if (asset.kind != "scriptableObject") continue;
                    var instance = ScriptableObject.CreateInstance(types[asset.typeName]);
                    if (instance == null) throw new InvalidDataException("Could not instantiate ScriptableObject " + asset.typeName);
                    instance.name = string.IsNullOrEmpty(asset.name) ? Path.GetFileNameWithoutExtension(asset.path) : asset.name;
                    ownedAssets.Add(instance); assets.Add(AssetKey(asset.guid, asset.fileId), instance);
                }
            }

            private Material LoadAuthoredMaterial(RawSceneAsset asset, Dictionary<string, RawSceneAsset> declared, HashSet<string> creating)
            {
                string key = AssetKey(asset.guid, asset.fileId);
                if (assets.TryGetValue(key, out Object existing))
                {
                    if (existing is Material material) return material;
                    throw new InvalidDataException("Authored material identity resolves to another asset type: " + asset.path);
                }
                if (!creating.Add(key)) throw new InvalidDataException("Material parent cycle: " + asset.path);
                try
                {
                    ReferenceData parent = RawAuthoredMaterial.Parent(asset);
                    if (!IsNull(parent.fileID) && declared.TryGetValue(AssetKey(parent.guid, parent.fileID), out RawSceneAsset parentAsset))
                        LoadAuthoredMaterial(parentAsset, declared, creating);
                    Material material = RawAuthoredMaterial.Create(asset, Resolve);
                    ownedAssets.Add(material);
                    assets.Add(key, material);
                    return material;
                }
                finally { creating.Remove(key); }
            }

            private void VerifyHostVolumeCaches()
            {
                foreach (Object asset in ownedAssets)
                {
                    if (!(asset is VolumeComponent component) || !TrustedHostScriptableObject(component.GetType())) continue;
                    foreach (FieldInfo field in component.GetType().GetFields(BindingFlags.Public | BindingFlags.Instance))
                    {
                        if (!typeof(VolumeParameter).IsAssignableFrom(field.FieldType)) continue;
                        var parameter = field.GetValue(component) as VolumeParameter;
                        if (parameter == null || !component.parameters.Contains(parameter))
                            throw new InvalidDataException("Restored VolumeParameter disagrees with the host's public parameter cache: " + component.GetType().FullName + "." + field.Name);
                    }
                }
            }

            private Texture2D DecodeTexture(RawSceneAsset asset, TextureImporterData importer)
            {
                bool linear = importer.sRGBTexture == 0;
                var decoded = new Texture2D(2, 2, TextureFormat.RGBA32, false, linear);
                Texture2D final = null;
                try
                {
                    if (!ImageConversion.LoadImage(decoded, File.ReadAllBytes(AssetPath(manifest, asset.path)), false))
                        throw new InvalidDataException("Unity could not decode raw texture " + asset.path);
                    Color32[] pixels = decoded.GetPixels32();
                    bool alpha = false;
                    foreach (Color32 pixel in pixels)
                        if (pixel.a != 255) { alpha = true; break; }
                    // The observed FromInput importer chooses RGB24 for entirely
                    // opaque images, including PNG files with an unused alpha channel.
                    // LoadImage alone keeps decoder ARGB32, so create the final public
                    // Texture2D format explicitly instead of preserving that staging format.
                    final = new Texture2D(decoded.width, decoded.height, alpha ? TextureFormat.RGBA32 : TextureFormat.RGB24,
                        importer.mipmapEnabled != 0, linear);
                    ownedAssets.Add(final);
                    int channels = alpha ? 4 : 3;
                    int packedBytes = 0;
                    int width = decoded.width, height = decoded.height;
                    // LoadRawTextureData requires storage for the full allocated mip
                    // chain. Apply then generates those levels from the packed base.
                    for (int mip = 0; mip < final.mipmapCount; mip++)
                    {
                        packedBytes = checked(packedBytes + checked(width * height * channels));
                        width = Math.Max(1, width / 2); height = Math.Max(1, height / 2);
                    }
                    var packed = new byte[packedBytes];
                    int offset = 0;
                    foreach (Color32 pixel in pixels)
                    {
                        packed[offset++] = pixel.r; packed[offset++] = pixel.g; packed[offset++] = pixel.b;
                        if (alpha) packed[offset++] = pixel.a;
                    }
                    final.LoadRawTextureData(packed);
                    final.Apply(importer.mipmapEnabled != 0, importer.isReadable == 0);
                    return final;
                }
                finally { Object.Destroy(decoded); }
            }

            private Component CreateComponent(GameObject owner, RawSceneObject record)
            {
                Type expected = NativeComponentType(record.classId);
                if (expected != null)
                    foreach (Component present in owner.GetComponents<Component>())
                        if (present != null && present.GetType() == expected && !sceneObjects.ContainsValue(present)) return present;
                switch (record.classId)
                {
                    case 20: return owner.AddComponent<Camera>();
                    case 23: return owner.AddComponent<MeshRenderer>();
                    case 33: return owner.AddComponent<MeshFilter>();
                    case 54: return owner.AddComponent<Rigidbody>();
                    case 65: return owner.AddComponent<BoxCollider>();
                    case 108: return owner.AddComponent<Light>();
                    case 114: return owner.AddComponent(types[record.typeName]);
                    case 135: return owner.AddComponent<SphereCollider>();
                    case 136: return owner.AddComponent<CapsuleCollider>();
                    default:
                        Type native = NativeComponentType(record.classId);
                        if (native == null) throw Error(record, "Unsupported component.");
                        return owner.AddComponent(native);
                }
            }

            private void ConfigureTransform(RawSceneObject record)
            {
                var data = ReadData<TransformData>(record.dataJson);
                var transform = (Transform)sceneObjects[record.fileId];
                if (data.m_Father != null && !IsNull(data.m_Father.fileID)) transform.SetParent((Transform)Resolve(data.m_Father), false);
                transform.localPosition = data.m_LocalPosition;
                transform.localRotation = data.m_LocalRotation;
                transform.localScale = data.m_LocalScale;
                if (record.classId == 224) ConfigureRectTransform((RectTransform)transform, record);
            }

            private void ConfigureTransformChildren(RawSceneObject record)
            {
                var data = ReadData<TransformData>(record.dataJson);
                var transform = (Transform)sceneObjects[record.fileId];
                if (data.m_Children != null)
                    for (int index = 0; index < data.m_Children.Length; index++)
                    {
                        var child = Resolve(data.m_Children[index]) as Transform;
                        if (child == null || child.parent != transform) throw Error(record, "Transform children/parent records disagree.");
                        child.SetSiblingIndex(index);
                    }
            }

            private void ConfigureComponent(RawSceneObject record)
            {
                switch (record.classId)
                {
                    case 20:
                    {
                        var data = ReadCameraData(record.dataJson); var camera = (Camera)sceneObjects[record.fileId];
                        camera.enabled = data.m_Enabled != 0; camera.clearFlags = (CameraClearFlags)data.m_ClearFlags;
                        camera.backgroundColor = data.m_BackGroundColor; camera.nearClipPlane = data.nearClipPlane;
                        camera.farClipPlane = data.farClipPlane; camera.fieldOfView = data.fieldOfView;
                        camera.orthographic = data.orthographic != 0; camera.orthographicSize = data.orthographicSize;
                        camera.depth = data.m_Depth; camera.cullingMask = data.m_CullingMask == null ? -1 : unchecked((int)data.m_CullingMask.m_Bits);
                        camera.rect = new Rect(data.m_NormalizedViewPortRect.x, data.m_NormalizedViewPortRect.y, data.m_NormalizedViewPortRect.width, data.m_NormalizedViewPortRect.height);
                        camera.allowHDR = data.m_HDR != 0; camera.allowMSAA = data.m_AllowMSAA != 0; camera.targetDisplay = data.m_TargetDisplay;
                        var map = Map(RawJson.Parse(record.dataJson));
                        camera.targetTexture = Resolve(Reference(Value(map, "m_TargetTexture", null))) as RenderTexture;
                        camera.allowDynamicResolution = Bool(map, "m_AllowDynamicResolution");
                        camera.forceIntoRenderTexture = Bool(map, "m_ForceIntoRT");
                        camera.useOcclusionCulling = Bool(map, "m_OcclusionCulling", true);
                        camera.renderingPath = (RenderingPath)Int(map, "m_RenderingPath", -1);
                        break;
                    }
                    case 23:
                    {
                        ConfigureRenderer((MeshRenderer)sceneObjects[record.fileId], record); break;
                    }
                    case 33:
                        ((MeshFilter)sceneObjects[record.fileId]).sharedMesh = (Mesh)Resolve(ReadData<MeshFilterData>(record.dataJson).m_Mesh); break;
                    case 54:
                    {
                        var map = (Dictionary<string, object>)RawJson.Parse(record.dataJson);
                        Rename(map, "m_LinearDamping", "m_Drag"); Rename(map, "m_AngularDamping", "m_AngularDrag");
                        var data = ReadData<RigidbodyData>(RawJson.Write(map)); var body = (Rigidbody)sceneObjects[record.fileId];
                        body.mass = data.m_Mass; body.linearDamping = data.m_Drag; body.angularDamping = data.m_AngularDrag;
                        body.useGravity = data.m_UseGravity != 0; body.isKinematic = data.m_IsKinematic != 0;
                        body.interpolation = (RigidbodyInterpolation)data.m_Interpolate; body.constraints = (RigidbodyConstraints)data.m_Constraints;
                        body.collisionDetectionMode = (CollisionDetectionMode)data.m_CollisionDetection; break;
                    }
                    case 65:
                    {
                        var data = ReadData<BoxColliderData>(record.dataJson); var collider = (BoxCollider)sceneObjects[record.fileId];
                        ConfigureCollider(collider, Map(RawJson.Parse(record.dataJson))); collider.center = data.m_Center; collider.size = data.m_Size; break;
                    }
                    case 135:
                    {
                        var data = ReadData<SphereColliderData>(record.dataJson); var collider = (SphereCollider)sceneObjects[record.fileId];
                        ConfigureCollider(collider, Map(RawJson.Parse(record.dataJson))); collider.center = data.m_Center; collider.radius = data.m_Radius; break;
                    }
                    case 136:
                    {
                        var data = ReadData<CapsuleColliderData>(record.dataJson); var collider = (CapsuleCollider)sceneObjects[record.fileId];
                        ConfigureCollider(collider, Map(RawJson.Parse(record.dataJson))); collider.center = data.m_Center;
                        collider.radius = data.m_Radius; collider.height = data.m_Height; collider.direction = data.m_Direction; break;
                    }
                    case 108:
                    {
                        var data = ReadData<LightData>(record.dataJson); var light = (Light)sceneObjects[record.fileId];
                        light.enabled = data.m_Enabled != 0; light.type = (LightType)data.m_Type; light.color = data.m_Color;
                        light.intensity = data.m_Intensity; light.range = data.m_Range; light.spotAngle = data.m_SpotAngle;
                        light.innerSpotAngle = data.m_InnerSpotAngle; light.shadows = (LightShadows)data.m_Shadows.m_Type;
                        light.shadowStrength = data.m_Shadows.m_Strength; light.cullingMask = data.m_CullingMask == null ? -1 : unchecked((int)data.m_CullingMask.m_Bits);
                        light.bounceIntensity = data.m_BounceIntensity;
                        var map = Map(RawJson.Parse(record.dataJson));
                        var shadows = Map(Value(map, "m_Shadows", null));
                        light.shadowBias = Float(shadows, "m_Bias", .05f);
                        light.shadowNormalBias = Float(shadows, "m_NormalBias", .4f);
                        light.shadowNearPlane = Float(shadows, "m_NearPlane", .2f);
                        light.shadowResolution = (LightShadowResolution)Int(shadows, "m_Resolution", -1);
                        light.shadowCustomResolution = Int(shadows, "m_CustomResolution", -1);
                        light.useColorTemperature = Bool(map, "m_UseColorTemperature");
                        light.colorTemperature = Float(map, "m_ColorTemperature", 6570);
                        light.renderMode = (LightRenderMode)Int(map, "m_RenderMode");
                        light.renderingLayerMask = unchecked((int)Bits(map, "m_RenderingLayerMask", 1));
                        light.cookie = Resolve(Reference(Value(map, "m_Cookie", null))) as Texture;
                        light.cookieSize = Float(map, "m_CookieSize", 10); break;
                    }
                    case 114:
                    {
                        if (record.skipMissingScript) break;
                        var behaviour = (MonoBehaviour)sceneObjects[record.fileId];
                        ConfigureSerializedObject(record.dataJson, behaviour, true);
                        behaviour.enabled = ReadData<EnabledData>(record.dataJson).m_Enabled != 0;
                        break;
                    }
                    default: ConfigureNativeComponent(record); break;
                }
            }

            private static void Rename(Dictionary<string, object> map, string original, string replacement)
            { if (map.TryGetValue(original, out object value)) { map.Remove(original); map[replacement] = value; } }

            private void ConfigureSerializedObject(string json, Object target, bool behaviour)
            {
                var map = (Dictionary<string, object>)RawJson.Parse(json);
                foreach (string property in new[] { "m_ObjectHideFlags", "m_CorrespondingSourceObject", "m_PrefabInstance", "m_PrefabAsset", "m_GameObject", "m_Script", "m_EditorClassIdentifier", "m_EditorHideFlags" }) map.Remove(property);
                if (behaviour) map.Remove("m_Enabled");
                map.Remove("m_Name");
                var compactCollections = RawCompactCollections.Normalize(target.GetType(), map);
                ReplaceReferences(map);
                JsonUtility.FromJsonOverwrite(RawJson.Write(map), target);
                if (compactCollections.Count != 0) RawCompactCollections.Verify(compactCollections, RawJson.Parse(JsonUtility.ToJson(target)));
            }

            private void ConfigureRenderSettings(RawSceneObject record)
            {
                var data = ReadData<RenderSettingsData>(record.dataJson);
                RenderSettings.fog = data.m_Fog != 0; RenderSettings.fogColor = data.m_FogColor; RenderSettings.fogMode = (FogMode)data.m_FogMode;
                RenderSettings.fogDensity = data.m_FogDensity; RenderSettings.fogStartDistance = data.m_LinearFogStart; RenderSettings.fogEndDistance = data.m_LinearFogEnd;
                RenderSettings.ambientMode = (AmbientMode)data.m_AmbientMode; RenderSettings.ambientSkyColor = data.m_AmbientSkyColor;
                RenderSettings.ambientEquatorColor = data.m_AmbientEquatorColor; RenderSettings.ambientGroundColor = data.m_AmbientGroundColor;
                RenderSettings.ambientIntensity = data.m_AmbientIntensity; RenderSettings.reflectionIntensity = data.m_ReflectionIntensity;
                RenderSettings.defaultReflectionMode = (DefaultReflectionMode)data.m_DefaultReflectionMode;
                RenderSettings.defaultReflectionResolution = data.m_DefaultReflectionResolution;
                RenderSettings.reflectionBounces = data.m_ReflectionBounces;
                ReferenceData reflection = data.m_CustomReflectionTexture;
                if (reflection == null || IsNull(reflection.fileID)) reflection = data.m_CustomReflection;
                RenderSettings.customReflectionTexture = Resolve(reflection) as Texture;
                RenderSettings.subtractiveShadowColor = data.m_SubtractiveShadowColor;
                if (data.m_Sun != null && !IsNull(data.m_Sun.fileID)) RenderSettings.sun = (Light)Resolve(data.m_Sun);
                if (data.m_SkyboxMaterial == null || IsNull(data.m_SkyboxMaterial.fileID)) RenderSettings.skybox = null;
                else RenderSettings.skybox = (Material)Resolve(data.m_SkyboxMaterial);
                if (RenderSettings.skybox != null && RenderSettings.ambientMode == AmbientMode.Skybox)
                    DynamicGI.UpdateEnvironment();
            }

            private object ReplaceReferences(object value)
            {
                if (value is Dictionary<string, object> map)
                {
                    if (map.TryGetValue("fileID", out object fileId))
                    {
                        var reference = new ReferenceData { fileID = Convert.ToString(fileId, CultureInfo.InvariantCulture) };
                        if (map.TryGetValue("guid", out object guid)) reference.guid = Convert.ToString(guid, CultureInfo.InvariantCulture);
                        Object target = Resolve(reference);
                        // Ask the public serializer for this engine version's reference
                        // representation. Unity 6.6 uses EntityId rather than old int IDs.
                        var wrapper = (Dictionary<string, object>)RawJson.Parse(JsonUtility.ToJson(new RuntimeReference { target = target }));
                        if (!wrapper.TryGetValue("target", out object serialized)
                            || (serialized != null && !(serialized is Dictionary<string, object>)))
                            throw new InvalidDataException("Unity serializer returned an unsupported object-reference representation.");
                        return serialized;
                    }
                    foreach (string key in new List<string>(map.Keys)) map[key] = ReplaceReferences(map[key]);
                }
                else if (value is List<object> list)
                    for (int index = 0; index < list.Count; index++) list[index] = ReplaceReferences(list[index]);
                return value;
            }

            private Object Resolve(ReferenceData reference)
            {
                if (reference == null || IsNull(reference.fileID)) return null;
                if (string.IsNullOrEmpty(reference.guid))
                {
                    if (!sceneObjects.TryGetValue(reference.fileID, out Object target)) throw new InvalidDataException("Reference is not an instantiated scene object: " + reference.fileID);
                    return target;
                }
                if (reference.guid == BuiltinResourceGuid && reference.fileID == "108")
                {
                    if (assets.TryGetValue(AssetKey(reference.guid, reference.fileID), out Object retained))
                    {
                        if (!(retained is Shader nativeShader) || nativeShader.name != "Skybox/Panoramic" || !nativeShader.isSupported)
                            throw new InvalidDataException("Retained Unity builtin Shader 108 must be supported Skybox/Panoramic.");
                        return nativeShader;
                    }
                    Shader shader = Shader.Find("Skybox/Panoramic");
                    if (shader == null || !shader.isSupported)
                        throw new InvalidDataException("Prepared host does not retain Unity builtin Shader 108 (Skybox/Panoramic).");
                    return shader;
                }
                if (reference.guid == BuiltinMeshGuid && Primitive(reference.fileID, out PrimitiveType primitive))
                {
                    if (!builtinMeshes.TryGetValue(reference.fileID, out Mesh mesh))
                    {
                        GameObject temporary = GameObject.CreatePrimitive(primitive);
                        temporary.SetActive(false);
                        try
                        {
                            if (!temporary.TryGetComponent<MeshFilter>(out var filter)) throw new InvalidOperationException("Unity primitive has no MeshFilter.");
                            mesh = filter.sharedMesh; builtinMeshes.Add(reference.fileID, mesh);
                        }
                        finally { Object.Destroy(temporary); }
                    }
                    return mesh;
                }
                if (reference.guid == BuiltinResourceGuid && reference.fileID == "10303")
                {
                    string key = AssetKey(reference.guid, reference.fileID);
                    if (!assets.TryGetValue(key, out var material))
                    {
                        Shader shader = Shader.Find("Standard");
                        if (shader == null || !shader.isSupported) throw new InvalidDataException("Prepared host lacks Standard shader.");
                        material = new Material(shader); ownedAssets.Add(material); assets.Add(key, material);
                    }
                    return material;
                }
                if (reference.guid == BuiltinResourceGuid && reference.fileID == "10304")
                {
                    string key = AssetKey(reference.guid, reference.fileID);
                    if (!assets.TryGetValue(key, out Object material))
                    {
                        // A prepared host can retain the exact native built-in
                        // material when Unity omits it from an empty player.
                        var skybox = Resources.Load<Material>("UnityPlayHostBuiltins/DefaultSkybox");
                        if (skybox == null) skybox = Resources.GetBuiltinResource<Material>("Default-Skybox.mat");
                        if (skybox == null || skybox.shader == null || !skybox.shader.isSupported
                            || skybox.shader.name == "Hidden/InternalErrorShader")
                            throw new InvalidDataException("Prepared host cannot resolve native Default-Skybox material 10304 with its supported shader. Prepare the host's UnityPlayHostBuiltins/DefaultSkybox resource from this engine's native built-in material.");
                        assets.Add(key, skybox); material = skybox;
                    }
                    return material;
                }
                if (!assets.TryGetValue(AssetKey(reference.guid, reference.fileID), out var asset))
                    throw new InvalidDataException("Reference is not a loaded raw asset: " + reference.guid + "/" + reference.fileID);
                return asset;
            }
        }

        [Serializable] private sealed class ReferenceData { public string fileID; public string guid; }
        [Serializable] private sealed class RuntimeReference { public Object target; }
        [Serializable] private class EnabledData { public int m_Enabled = 1; }
        [Serializable] private sealed class GameObjectData
        { public string m_Name; public int m_Layer; public string m_TagString; public int m_IsActive = 1; public int m_StaticEditorFlags; }
        [Serializable] private sealed class TransformData
        {
            public Vector3 m_LocalPosition; public Quaternion m_LocalRotation = Quaternion.identity;
            public Vector3 m_LocalScale = Vector3.one; public ReferenceData m_Father; public ReferenceData[] m_Children;
        }
        [Serializable] private sealed class BitsData { public uint m_Bits = uint.MaxValue; }
        [Serializable] private sealed class ViewportData { public float x; public float y; public float width = 1; public float height = 1; }
        [Serializable] private sealed class CameraData : EnabledData
        {
            public int m_ClearFlags = 1; public Color m_BackGroundColor = Color.black; public float nearClipPlane = .3f;
            public float farClipPlane = 1000; public float fieldOfView = 60; public int orthographic; public float orthographicSize = 5;
            public float m_Depth; public BitsData m_CullingMask; public ViewportData m_NormalizedViewPortRect = new ViewportData();
            public int m_HDR = 1; public int m_AllowMSAA = 1; public int m_TargetDisplay; public int ProjectionMatrixMode = 1;
        }
        [Serializable] private sealed class RendererData : EnabledData
        {
            public int m_CastShadows = 1; public int m_ReceiveShadows = 1; public int m_LightProbeUsage = 1;
            public int m_ReflectionProbeUsage = 1; public ReferenceData[] m_Materials; public int m_LightmapIndex = 65535;
        }
        [Serializable] private sealed class MeshFilterData { public ReferenceData m_Mesh; }
        [Serializable] private sealed class RigidbodyData
        {
            public float m_Mass = 1; public float m_Drag; public float m_AngularDrag = .05f; public int m_UseGravity = 1;
            public int m_IsKinematic; public int m_Interpolate; public int m_Constraints; public int m_CollisionDetection;
        }
        [Serializable] private class ColliderData : EnabledData { public int m_IsTrigger; public Vector3 m_Center; }
        [Serializable] private sealed class BoxColliderData : ColliderData { public Vector3 m_Size = Vector3.one; }
        [Serializable] private sealed class SphereColliderData : ColliderData { public float m_Radius = .5f; }
        [Serializable] private sealed class CapsuleColliderData : ColliderData { public float m_Radius = .5f; public float m_Height = 2; public int m_Direction = 1; }
        [Serializable] private sealed class ShadowData { public int m_Type; public float m_Strength = 1; }
        [Serializable] private sealed class LightData : EnabledData
        {
            public int m_Type = 1; public Color m_Color = Color.white; public float m_Intensity = 1; public float m_Range = 10;
            public float m_SpotAngle = 30; public float m_InnerSpotAngle = 21.80208f; public ShadowData m_Shadows = new ShadowData();
            public BitsData m_CullingMask; public float m_BounceIntensity = 1; public int m_Lightmapping = 4;
        }
        [Serializable] private sealed class RenderSettingsData
        {
            public int m_Fog; public Color m_FogColor; public int m_FogMode = 3; public float m_FogDensity = .01f;
            public float m_LinearFogStart; public float m_LinearFogEnd = 300; public int m_AmbientMode;
            public Color m_AmbientSkyColor; public Color m_AmbientEquatorColor; public Color m_AmbientGroundColor;
            public float m_AmbientIntensity = 1; public float m_ReflectionIntensity = 1; public ReferenceData m_Sun; public ReferenceData m_SkyboxMaterial;
            public int m_DefaultReflectionMode; public int m_DefaultReflectionResolution = 128; public int m_ReflectionBounces = 1;
            public ReferenceData m_CustomReflectionTexture; public ReferenceData m_CustomReflection; public Color m_SubtractiveShadowColor;
        }
        [Serializable] private sealed class TextureImporterData
        {
            public int textureType; public int textureShape = 1; public int sRGBTexture = 1; public int mipmapEnabled = 1;
            public int isReadable; public int filterMode = 1; public int wrapU; public int wrapV; public int aniso = 1;
        }
    }
}
