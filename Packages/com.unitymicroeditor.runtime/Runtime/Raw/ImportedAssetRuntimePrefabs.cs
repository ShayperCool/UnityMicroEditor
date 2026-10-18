using System;
using System.Collections.Generic;
using System.IO;
using UnityEngine;
using UnityEngine.Rendering;
using UnityEngine.SceneManagement;
using Object = UnityEngine.Object;

namespace UnityMicroEditor.Raw
{
    public static partial class RawSceneLoader
    {
        private static IEnumerable<RawSceneObject> AllScriptRecords(RawSceneManifest manifest)
        {
            foreach (RawSceneObject record in manifest.objects) yield return record;
            if (manifest.assets != null)
                foreach (RawSceneAsset asset in manifest.assets)
                    if (asset.kind == "prefab" && asset.objects != null)
                        foreach (RawSceneObject record in asset.objects) yield return record;
        }

        private static void AddPrefabValidationAliases(RawSceneManifest manifest, Dictionary<string, RawSceneAsset> assets)
        {
            if (manifest.assets == null) return;
            foreach (RawSceneAsset asset in manifest.assets)
            {
                if (asset.kind != "prefab") continue;
                if (asset.objects == null || string.IsNullOrEmpty(asset.rootFileId))
                    throw new InvalidDataException("Raw prefab object graph or root is missing: " + asset.path);
                foreach (RawSceneObject record in asset.objects)
                    AddAlias(record.fileId);
                if (asset.objectAliases != null)
                    foreach (RawPrefabAlias alias in asset.objectAliases)
                    {
                        if (alias == null || IsNull(alias.fileId) || string.IsNullOrEmpty(alias.objectId))
                            throw new InvalidDataException("Invalid raw prefab alias: " + asset.path);
                        AddAlias(alias.fileId);
                    }
                void AddAlias(string id)
                {
                    if (string.IsNullOrEmpty(id)) throw new InvalidDataException("Raw prefab object ID is empty: " + asset.path);
                    string key = AssetKey(asset.guid, id);
                    if (assets.TryGetValue(key, out RawSceneAsset present))
                    {
                        if (!ReferenceEquals(present, asset)) throw new InvalidDataException("Raw prefab asset alias collides with another asset: " + key);
                    }
                    else assets.Add(key, asset);
                }
            }
        }

        private static void ValidatePrefabGraphs(RawSceneManifest manifest, Dictionary<string, RawSceneAsset> assets)
        {
            if (manifest.assets == null) return;
            foreach (RawSceneAsset asset in manifest.assets)
            {
                if (asset.kind != "prefab") continue;
                var records = new Dictionary<string, RawSceneObject>(StringComparer.Ordinal);
                foreach (RawSceneObject record in asset.objects)
                {
                    if (record == null || string.IsNullOrEmpty(record.fileId) || records.ContainsKey(record.fileId))
                        throw new InvalidDataException("Raw prefab IDs must be nonempty and unique: " + asset.path);
                    if ((!SupportedClass(record.classId) && record.templateComponent <= 0) || !(RawJson.Parse(record.dataJson) is Dictionary<string, object>))
                        throw Error(record, "Unsupported raw prefab object record.");
                    if (record.classId == 114 && !record.skipMissingScript && string.IsNullOrEmpty(record.typeName)) throw Error(record, "Raw prefab script type is missing.");
                    records.Add(record.fileId, record);
                }
                if (!records.TryGetValue(asset.rootFileId, out RawSceneObject root) || root.classId != 1)
                    throw new InvalidDataException("Raw prefab root is not a GameObject: " + asset.path);
                foreach (RawSceneObject record in records.Values)
                {
                    if (IsComponent(record)
                        && (!records.TryGetValue(record.gameObjectId ?? "", out RawSceneObject owner) || owner.classId != 1))
                        throw Error(record, "Raw prefab component owner is missing.");
                    if (record.references != null)
                        foreach (RawSceneReference reference in record.references) ValidateReference(record, reference, records, assets);
                    ValidateRecordSettings(record);
                }
                ValidateHierarchy(records);
                foreach (RawSceneObject record in records.Values)
                    if (IsTransform(record.classId))
                    {
                        TransformData data = ReadData<TransformData>(record.dataJson);
                        if ((data.m_Father == null || IsNull(data.m_Father.fileID)) && record.gameObjectId != asset.rootFileId)
                            throw Error(record, "A raw prefab must have exactly one root GameObject.");
                    }
                if (asset.objectAliases != null)
                    foreach (RawPrefabAlias alias in asset.objectAliases)
                        if (!records.ContainsKey(alias.objectId)) throw new InvalidDataException("Raw prefab alias target is missing: " + asset.path + "/" + alias.objectId);
            }
        }

        private sealed partial class ImportContext
        {
            private sealed class PrefabTemplate
            {
                public RawSceneAsset asset;
                public Dictionary<string, Object> objects;
                public GameObject root;
            }
            private readonly List<PrefabTemplate> prefabTemplates = new List<PrefabTemplate>();

            // The pipeline's global settings and asset, when published from their
            // authored fields, get their data before the pipeline takes them; the
            // other raw ScriptableObjects get theirs after the prefab templates
            // exist, which these two do not name.
            private readonly HashSet<string> configuredEarly = new HashSet<string>(StringComparer.Ordinal);

            private void ConfigureRawPipelineObject(string guid, string fileId)
            {
                string key = AssetKey(guid, fileId);
                if (manifest.assets == null || configuredEarly.Contains(key)) return;
                foreach (var asset in manifest.assets)
                    if (asset.kind == "scriptableObject" && asset.guid == guid && asset.fileId == fileId)
                    {
                        ConfigureSerializedObject(asset.dataJson, assets[key], false);
                        configuredEarly.Add(key);
                        return;
                    }
            }

            private void ConfigurePipeline()
            {
                if (manifest.renderPipelineGlobalSettings != null)
                    foreach (RawRenderPipelineGlobalSettings settings in manifest.renderPipelineGlobalSettings)
                        ConfigureRawPipelineObject(settings.guid, settings.fileId);
                if (!string.IsNullOrEmpty(manifest.renderPipelineGuid))
                    ConfigureRawPipelineObject(manifest.renderPipelineGuid, manifest.renderPipelineFileId);
                if (manifest.renderPipelineGlobalSettings != null)
                    foreach (RawRenderPipelineGlobalSettings settings in manifest.renderPipelineGlobalSettings)
                    {
                        var globalSettings = Resolve(new ReferenceData { guid = settings.guid, fileID = settings.fileId }) as RenderPipelineGlobalSettings;
                        if (globalSettings == null) throw new InvalidDataException("Native render pipeline global settings reference has the wrong type.");
                        RegisterPipelineGlobalSettings(settings, globalSettings);
                    }
                if (string.IsNullOrEmpty(manifest.renderPipelineGuid)) return;
                var pipeline = Resolve(new ReferenceData { guid = manifest.renderPipelineGuid, fileID = manifest.renderPipelineFileId }) as RenderPipelineAsset;
                if (pipeline == null) throw new InvalidDataException("Native render pipeline reference is not a RenderPipelineAsset.");
                GraphicsSettings.defaultRenderPipeline = pipeline;
                QualitySettings.renderPipeline = pipeline;
                if (manifest.renderPipelineGlobalSettings != null)
                    foreach (RawRenderPipelineGlobalSettings settings in manifest.renderPipelineGlobalSettings)
                        if (settings.pipelineTypeName == pipeline.pipelineType.FullName)
                        {
                            var globalSettings = Resolve(new ReferenceData { guid = settings.guid, fileID = settings.fileId }) as RenderPipelineGlobalSettings;
                            ActivatePipelineGlobalSettings(settings, globalSettings);
                        }
            }

            private void RegisterPipelineGlobalSettings(RawRenderPipelineGlobalSettings settings, RenderPipelineGlobalSettings globalSettings)
            {
                Type pipelineType = null;
                var loaded = new List<System.Reflection.Assembly>(AppDomain.CurrentDomain.GetAssemblies());
                foreach (var assembly in loaded)
                {
                    Type candidate = assembly.GetType(settings.pipelineTypeName, false);
                    if (candidate == null) continue;
                    if (pipelineType != null && candidate != pipelineType)
                        throw new InvalidDataException("Ambiguous render pipeline type: " + settings.pipelineTypeName);
                    pipelineType = candidate;
                }
                if (pipelineType == null) pipelineType = LoadHostScriptType(settings.pipelineTypeName, loaded);
                if (pipelineType == null || !typeof(RenderPipeline).IsAssignableFrom(pipelineType) || pipelineType.ContainsGenericParameters)
                    throw new InvalidDataException("Prepared host has no matching render pipeline type: " + settings.pipelineTypeName);
                GraphicsSettings.RegisterRenderPipelineSettings(pipelineType, globalSettings);
                ActivatePipelineGlobalSettings(settings, globalSettings);
            }

            private static void ActivatePipelineGlobalSettings(RawRenderPipelineGlobalSettings settings, RenderPipelineGlobalSettings globalSettings)
            {
                if (!GraphicsSettings.TrySetCurrentRenderPipelineGlobalSettings(globalSettings))
                    throw new InvalidDataException("Unity refused runtime render pipeline global settings: " + settings.guid + "/" + settings.fileId);
                bool active = GraphicsSettings.TryGetCurrentRenderPipelineGlobalSettings(out RenderPipelineGlobalSettings current) && current == globalSettings;
                bool ssao = HasPublicGraphicsSettings("UnityEngine.Rendering.Universal.ScreenSpaceAmbientOcclusionCoreResources");
                Debug.Log("[unity-play-graphics-settings] asset=" + settings.guid + "/" + settings.fileId
                    + " active=" + active + " runtimeSettings=" + SerializedRuntimeGraphicsSettingsCount(globalSettings) + " ssaoResources=" + ssao);
                if (!active) throw new InvalidDataException("Unity did not retain the selected runtime render pipeline global settings.");
            }

            private static bool HasPublicGraphicsSettings(string typeName)
            {
                Type settingsType = null;
                foreach (var assembly in AppDomain.CurrentDomain.GetAssemblies())
                {
                    settingsType = assembly.GetType(typeName, false);
                    if (settingsType != null) break;
                }
                if (settingsType == null) return false;
                var method = typeof(GraphicsSettings).GetMethod("TryGetRenderPipelineSettings", System.Reflection.BindingFlags.Public | System.Reflection.BindingFlags.Static);
                if (method == null || !method.IsGenericMethodDefinition)
                    throw new InvalidOperationException("Unity does not expose its public graphics settings query.");
                var arguments = new object[] { null };
                return (bool)method.MakeGenericMethod(settingsType).Invoke(null, arguments) && arguments[0] != null;
            }

            private static int SerializedRuntimeGraphicsSettingsCount(RenderPipelineGlobalSettings settings)
            {
                // This is a diagnostic of the public serialized representation;
                // no foreign private fields or methods are accessed.
                var root = RawJson.Parse(JsonUtility.ToJson(settings)) as Dictionary<string, object>;
                if (root == null || !root.TryGetValue("m_Settings", out object containerValue)
                    || !(containerValue is Dictionary<string, object> container)
                    || !container.TryGetValue("m_RuntimeSettings", out object runtimeValue)
                    || !(runtimeValue is Dictionary<string, object> runtime)
                    || !runtime.TryGetValue("m_List", out object listValue)
                    || !(listValue is List<object> list)) return -1;
                return list.Count;
            }

            private void CreatePrefabTemplates()
            {
                if (manifest.assets == null) return;
                Dictionary<string, Object> sceneMap = sceneObjects;
                try
                {
                    foreach (RawSceneAsset asset in manifest.assets)
                    {
                        if (asset.kind != "prefab") continue;
                        var template = new PrefabTemplate { asset = asset, objects = new Dictionary<string, Object>(StringComparer.Ordinal) };
                        prefabTemplates.Add(template); sceneObjects = template.objects; currentRecords = asset.objects;
                        foreach (RawSceneObject record in currentRecords)
                        {
                            if (record.classId != 1) continue;
                            var data = ReadData<GameObjectData>(record.dataJson);
                            bool rect = false;
                            foreach (RawSceneObject candidate in currentRecords)
                                if (candidate.classId == 224 && candidate.gameObjectId == record.fileId) { rect = true; break; }
                            var gameObject = NewGameObject(record, data.m_Name ?? "Raw prefab", rect);
                            gameObject.SetActive(false); SceneManager.MoveGameObjectToScene(gameObject, scene);
                            gameObject.layer = data.m_Layer; gameObject.isStatic = data.m_StaticEditorFlags != 0;
                            if (!string.IsNullOrEmpty(data.m_TagString) && data.m_TagString != "Untagged") gameObject.tag = data.m_TagString;
                            sceneObjects.Add(record.fileId, gameObject);
                        }
                        foreach (RawSceneObject record in currentRecords)
                            if (IsTransform(record.classId)) sceneObjects.Add(record.fileId, ((GameObject)sceneObjects[record.gameObjectId]).transform);
                        foreach (RawSceneObject record in currentRecords)
                            if (IsTransform(record.classId)) ConfigureTransform(record);
                        foreach (RawSceneObject record in currentRecords)
                            if (IsTransform(record.classId)) ConfigureTransformChildren(record);
                        template.root = (GameObject)sceneObjects[asset.rootFileId];
                        var container = new GameObject("Raw prefab template " + asset.path) { hideFlags = HideFlags.HideInHierarchy | HideFlags.DontSave };
                        container.SetActive(false); SceneManager.MoveGameObjectToScene(container, scene);
                        container.transform.SetParent(resourceLifetime.transform, false);
                        template.root.transform.SetParent(container.transform, false);
                        foreach (RawSceneObject record in currentRecords)
                            if (IsTransform(record.classId))
                            {
                                var transform = (Transform)sceneObjects[record.fileId];
                                RawPrefabTransforms.Register(transform, template.root.transform);
                                registeredPrefabTransforms.Add(transform);
                            }
                        CreateAuthoredComponents();
                        foreach (RawSceneObject record in currentRecords)
                            Publish(record.fileId, sceneObjects[record.fileId]);
                        if (asset.objectAliases != null)
                            foreach (RawPrefabAlias alias in asset.objectAliases) Publish(alias.fileId, sceneObjects[alias.objectId]);
                        Publish(asset.fileId, template.root);
                        void Publish(string id, Object value)
                        {
                            string key = AssetKey(asset.guid, id);
                            if (assets.TryGetValue(key, out Object existing))
                            { if (existing != value) throw new InvalidDataException("Raw prefab identity collides with a loaded asset: " + key); }
                            else assets.Add(key, value);
                        }
                    }
                }
                finally { sceneObjects = sceneMap; currentRecords = manifest.objects; }
            }

            private void ConfigurePrefabTemplates()
            {
                Dictionary<string, Object> sceneMap = sceneObjects;
                try
                {
                    foreach (PrefabTemplate template in prefabTemplates)
                    {
                        sceneObjects = template.objects; currentRecords = template.asset.objects;
                        foreach (RawSceneObject record in currentRecords) ConfigureComponent(record);
                        foreach (RawSceneObject record in currentRecords)
                            if (record.classId == 1)
                                ((GameObject)sceneObjects[record.fileId]).SetActive(ReadData<GameObjectData>(record.dataJson).m_IsActive != 0);
                    }
                }
                finally { sceneObjects = sceneMap; currentRecords = manifest.objects; }
            }
        }
    }
}
