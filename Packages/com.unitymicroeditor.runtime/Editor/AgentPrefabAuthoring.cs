using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using UnityEditor;
using UnityEditor.SceneManagement;
using UnityEngine;
using UnityEngine.SceneManagement;

namespace Dungeon.CLIPlay.Editor
{
    /// <summary>PrefabUtility operations in an isolated authored project snapshot.</summary>
    public static class AgentPrefabAuthoring
    {
        [Serializable] private sealed class StagingMarker { public int schemaVersion; public string sourceProject; }

        public static Dictionary<string, object> Execute(Dictionary<string, object> arguments, string mailbox, string requestId)
        {
            RequireStaging();
            string action = Text(arguments, "action"), path = AssetPath(Text(arguments, "path"));
            bool sceneAsset = path.EndsWith(".unity", StringComparison.Ordinal);
            if (!sceneAsset && !path.EndsWith(".prefab", StringComparison.Ordinal))
                throw new ArgumentException("Prefab authoring requires an authored scene or prefab path.");
            if (!File.Exists(path)) throw new FileNotFoundException("Authored asset is absent.", path);
            GameObject prefabContents = null;
            Scene scene = default;
            try
            {
                if (sceneAsset) scene = EditorSceneManager.OpenScene(path, OpenSceneMode.Single);
                else { prefabContents = PrefabUtility.LoadPrefabContents(path); scene = prefabContents.scene; }
                GameObject target = null;
                if (action != "instantiate" && action != "create_variant") target = FindTarget(scene, arguments, "fileId", "targetPath");
                string generated = null;
                switch (action)
                {
                    case "add_component":
                    {
                        Type type = AgentEditorAuthoring.ResolveBuiltin(Text(arguments, "componentType"));
                        if (target.GetComponent(type) != null) throw new ArgumentException("The requested native component already exists on the target.");
                        Component component = target.AddComponent(type);
                        if (component == null) throw new InvalidOperationException("Unity rejected the native component.");
                        if (arguments.TryGetValue("fields", out object fieldsValue) && fieldsValue is Dictionary<string, object> fields)
                        {
                            var serialized = new SerializedObject(component);
                            foreach (var field in fields)
                            {
                                if (field.Key == "m_GameObject" || field.Key == "m_Script" || field.Key.StartsWith("m_Prefab", StringComparison.Ordinal) || field.Key == "m_CorrespondingSourceObject" || field.Key == "m_ObjectHideFlags")
                                    throw new ArgumentException("Component ownership fields cannot be edited.");
                                SerializedProperty property = serialized.FindProperty(field.Key);
                                if (property == null) throw new ArgumentException("Unknown serialized component field: " + field.Key);
                                SetProperty(property, field.Value);
                            }
                            serialized.ApplyModifiedPropertiesWithoutUndo();
                        }
                        if (PrefabUtility.IsPartOfPrefabInstance(component)) PrefabUtility.RecordPrefabInstancePropertyModifications(component);
                        break;
                    }
                    case "apply":
                        PrefabUtility.ApplyPrefabInstance(InstanceRoot(target), InteractionMode.AutomatedAction);
                        break;
                    case "revert":
                        PrefabUtility.RevertPrefabInstance(InstanceRoot(target), InteractionMode.AutomatedAction);
                        break;
                    case "unpack":
                        PrefabUtility.UnpackPrefabInstance(InstanceRoot(target),
                            Flag(arguments, "completely") ? PrefabUnpackMode.Completely : PrefabUnpackMode.OutermostRoot,
                            InteractionMode.AutomatedAction);
                        break;
                    case "instantiate":
                    {
                        string source = AssetPath(Text(arguments, "prefabPath"));
                        GameObject asset = AssetDatabase.LoadAssetAtPath<GameObject>(source);
                        if (asset == null) throw new ArgumentException("The requested prefab asset is absent.");
                        target = PrefabUtility.InstantiatePrefab(asset, scene) as GameObject;
                        if (target == null) throw new InvalidOperationException("Prefab instantiation failed.");
                        if (arguments.ContainsKey("parentFileId") || arguments.ContainsKey("parentPath"))
                            target.transform.SetParent(FindTarget(scene, arguments, "parentFileId", "parentPath").transform, false);
                        if (arguments.TryGetValue("name", out object name) && name is string label && label.Length > 0)
                        { target.name = label; PrefabUtility.RecordPrefabInstancePropertyModifications(target); }
                        break;
                    }
                    case "create_prefab":
                    {
                        generated = NewPrefabPath(Text(arguments, "outputPath"));
                        if (PrefabUtility.SaveAsPrefabAsset(target, generated, out bool success) == null || !success)
                            throw new InvalidOperationException("Could not save the new prefab asset.");
                        break;
                    }
                    case "create_variant":
                    {
                        generated = NewPrefabPath(Text(arguments, "outputPath"));
                        string source = AssetPath(Text(arguments, "prefabPath"));
                        GameObject asset = AssetDatabase.LoadAssetAtPath<GameObject>(source);
                        if (asset == null) throw new ArgumentException("The requested base prefab is absent.");
                        GameObject variant = PrefabUtility.InstantiatePrefab(asset, scene) as GameObject;
                        if (variant == null) throw new InvalidOperationException("Variant instantiation failed.");
                        try
                        {
                            if (PrefabUtility.SaveAsPrefabAsset(variant, generated, out bool success) == null || !success)
                                throw new InvalidOperationException("Could not save the prefab variant.");
                        }
                        finally { UnityEngine.Object.DestroyImmediate(variant); }
                        break;
                    }
                    case "set_override":
                    {
                        UnityEngine.Object owner = target;
                        if (arguments.TryGetValue("componentType", out object typeValue) && typeValue is string typeName)
                        {
                            Component selected = null;
                            foreach (Component component in target.GetComponents<Component>())
                                if (component != null && component.GetType().FullName == typeName)
                                { if (selected != null) throw new ArgumentException("Component target is ambiguous."); selected = component; }
                            owner = selected ?? throw new ArgumentException("The requested component is absent.");
                        }
                        if (!PrefabUtility.IsPartOfPrefabInstance(owner)) throw new ArgumentException("Target is not a prefab instance.");
                        string propertyPath = Text(arguments, "property");
                        if (propertyPath.StartsWith("m_CorrespondingSourceObject", StringComparison.Ordinal) ||
                            propertyPath.StartsWith("m_Prefab", StringComparison.Ordinal) || propertyPath == "m_Script" ||
                            propertyPath == "m_GameObject" || propertyPath.StartsWith("m_Children", StringComparison.Ordinal) || propertyPath == "m_Father")
                            throw new ArgumentException("Unity ownership and prefab linkage fields require structural operations.");
                        var serialized = new SerializedObject(owner);
                        SerializedProperty property = serialized.FindProperty(propertyPath);
                        if (property == null || !arguments.TryGetValue("value", out object value)) throw new ArgumentException("Serialized property/value is absent.");
                        SetProperty(property, value);
                        serialized.ApplyModifiedPropertiesWithoutUndo();
                        PrefabUtility.RecordPrefabInstancePropertyModifications(owner);
                        break;
                    }
                    default: throw new ArgumentException("Unsupported prefab operation: " + action);
                }
                if (sceneAsset)
                {
                    if (!EditorSceneManager.SaveScene(scene, path)) throw new IOException("Could not save staged scene.");
                }
                else if (PrefabUtility.SaveAsPrefabAsset(prefabContents, path, out bool saved) == null || !saved)
                    throw new IOException("Could not save staged prefab.");
                AssetDatabase.SaveAssets();
                AssetDatabase.Refresh(ImportAssetOptions.ForceSynchronousImport);
                return new Dictionary<string, object> { ["backend"] = "unity_editor", ["publicationRequired"] = true,
                    ["action"] = action, ["path"] = path, ["generatedPath"] = generated,
                    ["targetPath"] = target == null ? null : AgentRuntimeSnapshot.PathOf(target.transform), ["stagingProject"] = Directory.GetCurrentDirectory() };
            }
            finally { if (prefabContents != null) PrefabUtility.UnloadPrefabContents(prefabContents); }
        }

        internal static void RequireStaging()
        {
            string project = Path.GetFullPath(Path.Combine(Application.dataPath, ".."));
            string markerPath = Path.Combine(project, "ProjectSettings/UnityMicroEditorStaging.json");
            if (!Application.isBatchMode || !File.Exists(markerPath)) throw new InvalidOperationException("Prefab planning requires an isolated batch staging project.");
            StagingMarker marker = JsonUtility.FromJson<StagingMarker>(File.ReadAllText(markerPath));
            if (marker == null || marker.schemaVersion != 1 || !Path.IsPathRooted(marker.sourceProject) || Path.GetFullPath(marker.sourceProject) == project)
                throw new InvalidDataException("Invalid isolated staging identity.");
        }

        private static GameObject InstanceRoot(GameObject target)
        {
            GameObject root = PrefabUtility.GetNearestPrefabInstanceRoot(target);
            if (root == null) throw new ArgumentException("Target is not a prefab instance.");
            return root;
        }

        private static GameObject FindTarget(Scene scene, Dictionary<string, object> arguments, string idKey, string pathKey)
        {
            string id = arguments.TryGetValue(idKey, out object idValue) ? idValue as string : null;
            string path = arguments.TryGetValue(pathKey, out object pathValue) ? pathValue as string : null;
            if (string.IsNullOrEmpty(id) && string.IsNullOrEmpty(path)) throw new ArgumentException("Target requires " + idKey + " or " + pathKey + ".");
            if (id != null && !ulong.TryParse(id, NumberStyles.None, CultureInfo.InvariantCulture, out _)) throw new ArgumentException("Target fileID must be a decimal string.");
            GameObject found = null;
            foreach (GameObject root in scene.GetRootGameObjects())
                foreach (Transform transform in root.GetComponentsInChildren<Transform>(true))
                {
                    GameObject value = transform.gameObject;
                    GlobalObjectId identity = GlobalObjectId.GetGlobalObjectIdSlow(value);
                    bool matches = id != null ? identity.targetObjectId.ToString(CultureInfo.InvariantCulture) == id : AgentRuntimeSnapshot.PathOf(transform) == path;
                    if (!matches) continue;
                    if (found != null) throw new ArgumentException("Object target is ambiguous.");
                    found = value;
                }
            return found ?? throw new ArgumentException("Object target is absent.");
        }

        private static void SetProperty(SerializedProperty property, object value)
        {
            switch (property.propertyType)
            {
                case SerializedPropertyType.Boolean: if (!(value is bool flag)) throw new ArgumentException("Expected a boolean."); property.boolValue = flag; break;
                case SerializedPropertyType.Integer: property.longValue = Convert.ToInt64(value, CultureInfo.InvariantCulture); break;
                case SerializedPropertyType.Float: property.doubleValue = Convert.ToDouble(value, CultureInfo.InvariantCulture); break;
                case SerializedPropertyType.String: property.stringValue = value as string ?? throw new ArgumentException("Expected a string."); break;
                case SerializedPropertyType.Enum: property.intValue = Convert.ToInt32(value, CultureInfo.InvariantCulture); break;
                case SerializedPropertyType.ObjectReference:
                {
                    if (value == null) { property.objectReferenceValue = null; break; }
                    if (!(value is Dictionary<string, object> reference)) throw new ArgumentException("Expected an asset reference object.");
                    string assetPath = AssetPath(Text(reference, "path"));
                    if (reference.TryGetValue("fileId", out object localValue))
                    {
                        if (!(localValue is string localId) || !long.TryParse(localId, NumberStyles.Integer, CultureInfo.InvariantCulture, out long fileId)) throw new ArgumentException("Reference fileID must be a decimal string.");
                        UnityEngine.Object found = null;
                        foreach (UnityEngine.Object asset in AssetDatabase.LoadAllAssetsAtPath(assetPath))
                            if (asset != null && AssetDatabase.TryGetGUIDAndLocalFileIdentifier(asset, out string _, out long candidate) && candidate == fileId) found = asset;
                        property.objectReferenceValue = found ?? throw new ArgumentException("Asset reference identity is absent.");
                    }
                    else property.objectReferenceValue = AssetDatabase.LoadMainAssetAtPath(assetPath) ?? throw new ArgumentException("Asset reference is absent.");
                    break;
                }
                default: throw new ArgumentException("Select a scalar child property or explicit asset reference; whole " + property.propertyType + " writes are unsupported.");
            }
        }

        private static string Text(Dictionary<string, object> arguments, string key)
        { if (!arguments.TryGetValue(key, out object value) || !(value is string text) || string.IsNullOrEmpty(text)) throw new ArgumentException(key + " is required."); return text; }
        private static bool Flag(Dictionary<string, object> arguments, string key) => arguments.TryGetValue(key, out object value) && value is bool flag && flag;
        internal static string AssetPath(string path)
        {
            if (!path.StartsWith("Assets/", StringComparison.Ordinal) || path.Contains("\\") || path.Contains("//")) throw new ArgumentException("Use a normalized authored Assets path.");
            foreach (string part in path.Split('/')) if (part == "." || part == ".." || part.Length == 0) throw new ArgumentException("Asset traversal is prohibited.");
            string absolute = Path.GetFullPath(path);
            string project = Path.GetFullPath(Path.Combine(Application.dataPath, ".."));
            if (!absolute.StartsWith(project + Path.DirectorySeparatorChar + "Assets" + Path.DirectorySeparatorChar, StringComparison.Ordinal)) throw new ArgumentException("Asset is outside staging project.");
            for (string current = absolute; current != project; current = Path.GetDirectoryName(current))
                if ((File.Exists(current) || Directory.Exists(current)) && (File.GetAttributes(current) & FileAttributes.ReparsePoint) != 0) throw new IOException("Asset paths cannot contain symlinks.");
            return path;
        }
        private static string NewPrefabPath(string path)
        {
            path = AssetPath(path);
            if (!path.EndsWith(".prefab", StringComparison.Ordinal) || File.Exists(path) || File.Exists(path + ".meta") || !Directory.Exists(Path.GetDirectoryName(path)))
                throw new ArgumentException("New prefab requires an absent .prefab path in an existing authored directory.");
            return path;
        }
    }
}
