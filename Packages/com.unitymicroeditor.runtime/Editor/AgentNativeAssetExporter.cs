using System;
using System.Collections;
using System.Collections.Generic;
using System.Diagnostics;
using System.Globalization;
using System.IO;
using System.Linq;
using System.Security.Cryptography;
using System.Text;
using System.Text.RegularExpressions;
using UnityMicroEditor.Raw;
using UnityEditor;
using UnityEditor.Build;
using UnityEditor.Compilation;
using UnityEditor.SceneManagement;
using UnityEngine;
using UnityEngine.Rendering;
using Object = UnityEngine.Object;
using PackageInfo = UnityEditor.PackageManager.PackageInfo;
using Debug = UnityEngine.Debug;

namespace UnityMicroEditor.Editor
{
    /// <summary>Native import/export of assets. Scene construction stays outside BuildPipeline.</summary>
    public static class AgentNativeAssetExporter
    {
        private const string RegistryPath = "Assets/__UnityPlayImports/registry.asset";
        private const string BundleName = "unity-play-native-assets";
        private static readonly HashSet<string> NativeExtensions = new HashSet<string>(StringComparer.OrdinalIgnoreCase)
        {
            ".fbx", ".obj", ".dae", ".blend", ".3ds", ".max", ".ma", ".mb",
            ".wav", ".ogg", ".mp3", ".aif", ".aiff", ".flac", ".mixer", ".anim", ".controller", ".overridecontroller",
            ".mat", ".shader", ".shadergraph", ".shadersubgraph", ".compute", ".inputactions",
            ".png", ".jpg", ".jpeg", ".tga", ".psd", ".exr", ".hdr", ".tif", ".tiff", ".dds", ".bmp",
            ".rendertexture", ".cubemap", ".physicmaterial", ".physicsmaterial2d", ".mask",
            ".uxml", ".uss", ".ttf", ".otf", ".fontsettings", ".spriteatlas", ".spriteatlasv2",
        };
        private static readonly Dictionary<Type, int> ClassIds = new Dictionary<Type, int>
        {
            {typeof(GameObject), 1}, {typeof(Transform), 4}, {typeof(Camera), 20}, {typeof(Material), 21},
            {typeof(MeshRenderer), 23}, {typeof(Texture2D), 28}, {typeof(MeshFilter), 33}, {typeof(Mesh), 43},
            {typeof(Shader), 48}, {typeof(Rigidbody), 54}, {typeof(MeshCollider), 64}, {typeof(BoxCollider), 65},
            {typeof(AudioClip), 83}, {typeof(AudioSource), 82}, {typeof(AnimationClip), 74}, {typeof(Animator), 95},
            {typeof(Light), 108}, {typeof(Animation), 111}, {typeof(SphereCollider), 135},
            {typeof(CapsuleCollider), 136}, {typeof(SkinnedMeshRenderer), 137}, {typeof(LODGroup), 205},
            {typeof(Avatar), 90}, {typeof(AnimatorOverrideController), 221},
        };

        [Serializable] private sealed class Request
        {
            public int schemaVersion = 0;
            public string exportId = null;
            public string[] scenes = null;
            public string[] assetPaths = null;
            public string[] settingsGuids = null;
            public string outputDirectory = null;
            public string catalogPath = null;
            public string graphicsApi = null;
            public OverrideProbe[] overrideProbes = null;
        }

        [Serializable] private sealed class OverrideProbe
        {
            public string path = null;
            public string instanceFileId = null;
            public string targetGuid = null;
            public string targetFileId = null;
            public string propertyPath = null;
            public string arrayPath = null;
            public int expectedArraySize = 0;
        }

        private sealed class CurveWrapCarrier : ScriptableObject
        {
            public AnimationCurve curve = AnimationCurve.Linear(0f, 0f, 1f, 1f);
        }

        private sealed class RegistrySourceSnapshot
        {
            public string guid;
            public string fileId;
            public string typeName;
            public string sourceTypeName;
            public string assetPath;
            public bool globalSettings;
            public Dictionary<string, object> observation;
        }

        /// <summary>Runs only in the bridge's isolated batch staging project.</summary>
        public static Dictionary<string, object> Execute(Dictionary<string, object> arguments, string mailbox, string requestId)
        {
            var watch = Stopwatch.StartNew();
            string output = Path.GetFullPath(Path.Combine("Builds/UnityMicroEditor", "native-" + requestId));
            string catalogPath = Path.Combine(mailbox, "native-catalog-" + requestId + ".json");
            const string scratch = "Assets/__UnityPlayImports";
            if (Directory.Exists(scratch) || File.Exists(scratch + ".meta") || Directory.Exists(output) || File.Exists(output) || File.Exists(catalogPath))
                throw new IOException("Native export scratch, output or catalog already exists; refusing to overwrite it.");
            AgentEditorBridge.Canonical(output); AgentEditorBridge.Canonical(catalogPath);
            AgentEditorBridge.Canonical(Path.GetFullPath(scratch));
            var body = new Dictionary<string, object>(arguments) { ["schemaVersion"] = 1, ["exportId"] = Guid.NewGuid().ToString("N"),
                ["outputDirectory"] = output, ["catalogPath"] = catalogPath, ["graphicsApi"] = "glcore" };
            string requestJson = AgentEditorJson.Write(body);
            var request = JsonUtility.FromJson<Request>(requestJson);
            if (request.scenes == null || request.scenes.Length < 1 || request.scenes.Length > 64 || (request.assetPaths?.Length ?? 0) > 16384 || (request.settingsGuids?.Length ?? 0) > 4096)
                throw new ArgumentException("Native export requires 1 to 64 saved scenes and bounded asset/settings lists.");
            foreach (string guid in request.settingsGuids ?? Array.Empty<string>())
                if (guid == null || !Regex.IsMatch(guid, "^[0-9a-f]{32}$")) throw new ArgumentException("Settings GUIDs must be lowercase 32-digit hex identities.");
            SceneSetup[] setup = EditorSceneManager.GetSceneManagerSetup();
            var report = new Dictionary<string, object>
            {
                ["schemaVersion"] = 1, ["success"] = false, ["engineVersion"] = Application.unityVersion,
                ["target"] = "StandaloneLinux64", ["scriptingBackend"] = "Mono2x", ["graphicsApi"] = "glcore",
                ["containsScenes"] = false, ["sceneLoader"] = "raw", ["failure"] = "",
                ["backend"] = "unity_editor", ["catalogPath"] = catalogPath, ["outputDirectory"] = output,
                ["exportId"] = request.exportId, ["requestSha256"] = HashBytes(Encoding.UTF8.GetBytes(requestJson)),
            };
            try
            {
                if (!Application.isBatchMode || EditorUserBuildSettings.activeBuildTarget != BuildTarget.StandaloneLinux64)
                    throw new InvalidOperationException("Native import requires -batchmode -buildTarget Linux64.");
                if (AssetDatabase.CreateFolder("Assets", "__UnityPlayImports").Length == 0) throw new IOException("Cannot create exclusive native export scratch.");
                Build(request, output, report);
                report["success"] = true;
            }
            finally
            {
                try { AgentEditorBridge.RestoreSceneSetup(setup); }
                finally
                {
                    if (AssetDatabase.IsValidFolder(scratch) && !AssetDatabase.DeleteAsset(scratch)) throw new IOException("Cannot remove owned native export scratch.");
                }
            }
            report["elapsedSeconds"] = watch.Elapsed.TotalSeconds;
            byte[] catalog = new UTF8Encoding(false).GetBytes(AgentEditorJson.Write(report) + "\n");
            if (catalog.Length > 32 * 1024 * 1024) throw new InvalidDataException("Native catalog exceeds the protocol response bound.");
            using (var stream = new FileStream(catalogPath, FileMode.CreateNew, FileAccess.Write, FileShare.None)) stream.Write(catalog, 0, catalog.Length);
            return report;
        }

        private static string HashBytes(byte[] bytes)
        { using (var hash = SHA256.Create()) return BitConverter.ToString(hash.ComputeHash(bytes)).Replace("-", "").ToLowerInvariant(); }

        private static void Build(Request request, string output, Dictionary<string, object> report)
        {
            var roots = new SortedSet<string>(StringComparer.Ordinal);
            var discoveryRoots = new SortedSet<string>(StringComparer.Ordinal);
            var preloads = new List<object>();
            foreach (Object value in PlayerSettings.GetPreloadedAssets())
            {
                if (value == null) continue;
                string path = AssetDatabase.GetAssetPath(value);
                ValidateLogicalPath(path);
                if (path.EndsWith(".unity", StringComparison.OrdinalIgnoreCase))
                    throw new InvalidOperationException("Preloaded scenes cannot enter a native asset pack: " + path);
                if (!AssetDatabase.TryGetGUIDAndLocalFileIdentifier(value, out string guid, out long localId))
                    throw new InvalidOperationException("Preloaded asset has no persistent identity: " + path);
                bool rawPrefab = path.EndsWith(".prefab", StringComparison.OrdinalIgnoreCase);
                if (!rawPrefab) roots.Add(path);
                discoveryRoots.Add(path);
                preloads.Add(new Dictionary<string, object>
                {
                    ["guid"] = guid, ["fileId"] = localId.ToString(CultureInfo.InvariantCulture), ["path"] = path,
                    ["typeName"] = value.GetType().FullName, ["kind"] = rawPrefab ? "rawPrefab" : "imported",
                });
            }
            report["preloadedAssets"] = preloads;
            foreach (string scene in request.scenes ?? Array.Empty<string>())
            {
                ValidateLogicalPath(scene);
                if (!scene.EndsWith(".unity", StringComparison.OrdinalIgnoreCase) || AssetDatabase.LoadAssetAtPath<SceneAsset>(scene) == null)
                    throw new ArgumentException("Scene used for dependency discovery is missing: " + scene);
                discoveryRoots.Add(scene);
            }
            foreach (string guid in request.settingsGuids ?? Array.Empty<string>())
            {
                string path = AssetDatabase.GUIDToAssetPath(guid);
                if (!string.IsNullOrEmpty(path)) discoveryRoots.Add(path);
            }
            foreach (string path in request.assetPaths ?? Array.Empty<string>())
            {
                ValidateLogicalPath(path);
                if (path.EndsWith(".unity", StringComparison.OrdinalIgnoreCase) || path.EndsWith(".prefab", StringComparison.OrdinalIgnoreCase))
                    throw new ArgumentException("Authored scenes and text prefabs belong to the raw loader: " + path);
                if (AssetDatabase.LoadMainAssetAtPath(path) == null) throw new ArgumentException("Requested asset is missing: " + path);
                roots.Add(path);
                discoveryRoots.Add(path);
            }
            // Discovery follows the complete authored graph, including preloaded
            // settings -> raw root prefab -> raw game data -> audio/model roots.
            // Only the selected native roots enter NativeDependencies below.
            // Scenes, authored prefabs and plain game data stay with the raw loader.
            foreach (string dependency in AssetDatabase.GetDependencies(discoveryRoots.ToArray(), true))
                if (IsNativeRoot(dependency)) roots.Add(dependency);
            report["nativeSceneMetadata"] = SceneMetadata(request.scenes ?? Array.Empty<string>());
            report["ignoredPrefabOverrides"] = ProbeIgnoredOverrides(request.overrideProbes ?? Array.Empty<OverrideProbe>());
            report["prefabObservations"] = ObserveAuthoredPrefabs(request, roots);
            report["curveWrapModes"] = ObserveCurveWrapModes();
            report["nativeAssetsPresent"] = roots.Count != 0;
            if (roots.Count == 0)
            {
                // A project may contain unused materials while its selected
                // scene needs only built-in resources. Public dependency
                // discovery confirms that no native pack is required.
                foreach (string name in new[] { "packs", "assets", "modelPrefabs", "deferredBindings", "excludedRuntimeAssets", "modelAnimationClips", "globalSettingsAssets", "sourceDependencies", "rootAssets", "requiredAssemblies", "nativeBuildPasses" })
                    report[name] = Array.Empty<object>();
                report["sourceRoots"] = PackageInfo.GetAllRegisteredPackages().Where(package => !string.IsNullOrEmpty(package.name) && !string.IsNullOrEmpty(package.resolvedPath))
                    .OrderBy(package => package.name, StringComparer.Ordinal).Select(package => (object)new Dictionary<string, object> { ["path"] = "Packages/" + package.name, ["source"] = Path.GetFullPath(package.resolvedPath) }).ToArray();
                report["cookedRegistryVerified"] = true;
                Directory.CreateDirectory(output);
                return;
            }
            string[] dependencyPaths = NativeDependencies(roots);
            var cloneMap = CloneAnimationData(dependencyPaths, out List<object> deferredBindings);
            var values = new SortedDictionary<string, ImportedAssetRegistry.Entry>(StringComparer.Ordinal);
            var pathsByIdentity = new Dictionary<string, string>(StringComparer.Ordinal);
            var assetRecords = new List<object>();
            var modelRecords = new List<object>();
            var assemblies = new SortedSet<string>(StringComparer.Ordinal);
            var excludedRuntimeAssets = new SortedDictionary<string, object>(StringComparer.Ordinal);
            var modelAnimationClips = new SortedDictionary<string, object>(StringComparer.Ordinal);
            foreach (string path in dependencyPaths)
            {
                string extension = Path.GetExtension(path);
                if (!path.StartsWith("Assets/", StringComparison.Ordinal) && !path.StartsWith("Packages/", StringComparison.Ordinal)) continue;
                if (extension == ".cs" || extension == ".asmdef" || extension == ".asmref" || AssetDatabase.IsValidFolder(path)) continue;
                bool isModel = AssetImporter.GetAtPath(path) is ModelImporter;
                foreach (Object value in AssetDatabase.LoadAllAssetsAtPath(path))
                {
                    if (isModel && value is AnimationClip)
                    {
                        var observation = RuntimeAssetObservation(value, path);
                        string identity = (string)observation["guid"] + ":" + (string)observation["fileId"];
                        modelAnimationClips[identity] = observation;
                    }
                    AddValue(value, path, values, assemblies, pathsByIdentity, cloneMap, excludedRuntimeAssets);
                }
                if (isModel)
                {
                    var model = AssetDatabase.LoadAssetAtPath<GameObject>(path);
                    if (model == null) throw new InvalidOperationException("ModelImporter returned no GameObject: " + path);
                    string guid = AssetDatabase.AssetPathToGUID(path);
                    var objects = new List<object>();
                    foreach (Transform transform in model.GetComponentsInChildren<Transform>(true))
                    {
                        if ((transform.gameObject.hideFlags & HideFlags.DontSaveInBuild) != 0 || (transform.hideFlags & HideFlags.DontSaveInBuild) != 0)
                            throw new NotSupportedException("Native model hierarchy has a build-excluded GameObject/Transform: " + path + " " + transform.name);
                        AddValue(transform.gameObject, path, values, assemblies, pathsByIdentity, cloneMap, excludedRuntimeAssets);
                        objects.Add(ModelObject(transform.gameObject, guid));
                        foreach (Component component in transform.GetComponents<Component>())
                        {
                            if (component == null) throw new InvalidOperationException("Missing component on native model: " + path);
                            AddValue(component, path, values, assemblies, pathsByIdentity, cloneMap, excludedRuntimeAssets);
                            if ((component.hideFlags & HideFlags.DontSaveInBuild) != 0) continue;
                            objects.Add(ModelObject(component, guid));
                        }
                    }
                    if (!AssetDatabase.TryGetGUIDAndLocalFileIdentifier(model, out string _, out long rootId))
                        throw new InvalidOperationException("Native model root has no stable identifier: " + path);
                    modelRecords.Add(new Dictionary<string, object>
                    {
                        ["guid"] = guid, ["path"] = path, ["rootFileId"] = rootId.ToString(CultureInfo.InvariantCulture), ["objects"] = objects,
                    });
                }
            }
            report["excludedRuntimeAssets"] = excludedRuntimeAssets.Values.ToArray();
            report["modelAnimationClips"] = modelAnimationClips.Values.ToArray();
            if (values.Count == 0) throw new InvalidOperationException("No runtime objects can be exported.");
            foreach (ImportedAssetRegistry.Entry entry in values.Values)
            {
                assetRecords.Add(new Dictionary<string, object>
                {
                    ["kind"] = "imported", ["guid"] = entry.guid, ["fileId"] = entry.fileId,
                    ["classId"] = ClassId(entry.value.GetType()), ["typeName"] = entry.typeName,
                    ["name"] = entry.name, ["path"] = pathsByIdentity[entry.guid + ":" + entry.fileId],
                });
            }
            var registry = AssetDatabase.LoadAssetAtPath<ImportedAssetRegistry>(RegistryPath);
            if (registry == null)
            {
                if (File.Exists(RegistryPath)) throw new IOException("Importer registry path contains a different asset type.");
                registry = ScriptableObject.CreateInstance<ImportedAssetRegistry>();
                AssetDatabase.CreateAsset(registry, RegistryPath);
            }
            registry.schemaVersion = 1;
            registry.entries = values.Values.ToArray();
            EditorUtility.SetDirty(registry);
            AssetDatabase.SaveAssetIfDirty(registry);
            // URP renderer feature OnEnable requests shader settings while the
            // registry is still being deserialized. Settings must be loadable
            // and registered first, rather than only an implicit dependency.
            var explicitAssets = new SortedDictionary<string, string>(StringComparer.Ordinal)
            {
                [RegistryPath] = RegistryPath.ToLowerInvariant(),
            };
            var globalSettingsAssets = new List<object>();
            var addresses = new HashSet<string>(explicitAssets.Values, StringComparer.Ordinal);
            foreach (ImportedAssetRegistry.Entry entry in values.Values)
            {
                if (!(entry.value is RenderPipelineGlobalSettings)) continue;
                string key = entry.guid + ":" + entry.fileId;
                string originalPath = pathsByIdentity[key];
                string exportedPath = AssetDatabase.GetAssetPath(entry.value);
                ValidateLogicalPath(exportedPath);
                if (!exportedPath.EndsWith(".asset", StringComparison.OrdinalIgnoreCase)
                    || AssetDatabase.LoadMainAssetAtPath(exportedPath) != entry.value)
                    throw new InvalidOperationException("Pipeline global settings must be an addressable main asset: " + originalPath);
                string address = originalPath.ToLowerInvariant();
                if (!addresses.Add(address) || explicitAssets.ContainsKey(exportedPath))
                    throw new InvalidOperationException("Pipeline global-settings bundle address is ambiguous: " + originalPath);
                // A native clone keeps the original logical address. The raw
                // manifest resolves identities from the authored GUID/fileID.
                explicitAssets.Add(exportedPath, address);
                globalSettingsAssets.Add(new Dictionary<string, object>
                {
                    ["guid"] = entry.guid, ["fileId"] = entry.fileId,
                    ["path"] = originalPath, ["assetName"] = address,
                });
            }
            report["globalSettingsAssets"] = globalSettingsAssets;
            string[] registryDependencies = AssetDatabase.GetDependencies(explicitAssets.Keys.ToArray(), true);
            foreach (string path in registryDependencies)
                if (path.EndsWith(".unity", StringComparison.OrdinalIgnoreCase) || path.EndsWith(".prefab", StringComparison.OrdinalIgnoreCase))
                    throw new InvalidOperationException("Registry would embed a raw-loader scene/prefab: " + path);
            bool defaultApis = PlayerSettings.GetUseDefaultGraphicsAPIs(BuildTarget.StandaloneLinux64);
            GraphicsDeviceType[] previousApis = PlayerSettings.GetGraphicsAPIs(BuildTarget.StandaloneLinux64);
            ScriptingImplementation previousBackend = PlayerSettings.GetScriptingBackend(NamedBuildTarget.Standalone);
            try
            {
                PlayerSettings.SetScriptingBackend(NamedBuildTarget.Standalone, ScriptingImplementation.Mono2x);
                PlayerSettings.SetUseDefaultGraphicsAPIs(BuildTarget.StandaloneLinux64, false);
                PlayerSettings.SetGraphicsAPIs(BuildTarget.StandaloneLinux64, new[] { GraphicsDeviceType.OpenGLCore });
                Directory.CreateDirectory(output);
                var passes = new List<object>();
                for (int pass = 0; pass < 2; pass++)
                {
                    var passWatch = Stopwatch.StartNew();
                    // BuildPipeline may unload/reimport source objects. Persistent
                    // identities and concrete-type proof must survive that boundary.
                    var sourceSnapshot = CaptureRegistrySources(values, pathsByIdentity);
                    List<object> packs = BuildNativePacks(output, explicitAssets);
                    var cookedOmissions = ProbeCookedRegistry(packs, sourceSnapshot, globalSettingsAssets);
                    passes.Add(new Dictionary<string, object>
                    {
                        ["pass"] = pass + 1, ["elapsedSeconds"] = passWatch.Elapsed.TotalSeconds,
                        ["registryEntries"] = values.Count, ["cookedNullEntries"] = cookedOmissions.Count,
                    });
                    report["nativeBuildPasses"] = passes;
                    if (cookedOmissions.Count == 0)
                    {
                        report["packs"] = packs;
                        report["cookedRegistryVerified"] = true;
                        break;
                    }
                    if (pass != 0) throw new InvalidOperationException("Native registry still contains missing objects after its verified omission rebuild.");
                    foreach (var omission in cookedOmissions)
                    {
                        string key = (string)omission["guid"] + ":" + (string)omission["fileId"];
                        if (sourceSnapshot[key].globalSettings)
                            throw new InvalidOperationException("Native build omitted required pipeline global settings: " + key);
                        excludedRuntimeAssets.Add(key, omission);
                        values.Remove(key);
                        assetRecords.RemoveAll(record =>
                        {
                            var fields = (Dictionary<string, object>)record;
                            return (string)fields["guid"] + ":" + (string)fields["fileId"] == key;
                        });
                    }
                    // Do not write the old source handles after bundle readback:
                    // Unity can invalidate them during build or Unload(true).
                    AssetDatabase.ImportAsset(RegistryPath, ImportAssetOptions.ForceSynchronousImport | ImportAssetOptions.ForceUpdate);
                    registry = AssetDatabase.LoadAssetAtPath<ImportedAssetRegistry>(RegistryPath);
                    if (registry == null || registry.schemaVersion != 1 || registry.entries == null)
                        throw new InvalidOperationException("Source registry could not be reloaded after cooked readback.");
                    var freshValues = new SortedDictionary<string, ImportedAssetRegistry.Entry>(StringComparer.Ordinal);
                    foreach (ImportedAssetRegistry.Entry entry in registry.entries)
                    {
                        if (entry == null) throw new InvalidOperationException("Reloaded source registry has a null metadata row.");
                        string key = entry.guid + ":" + entry.fileId;
                        if (!sourceSnapshot.TryGetValue(key, out RegistrySourceSnapshot captured) || entry.typeName != captured.typeName)
                            throw new InvalidOperationException("Reloaded source registry metadata differs from its pre-build snapshot: " + key);
                        if (values.ContainsKey(key)) freshValues.Add(key, entry);
                    }
                    if (freshValues.Count != values.Count)
                        throw new InvalidOperationException("Reloaded source registry lost retained identities.");
                    values = freshValues;
                    registry.entries = values.Values.ToArray();
                    EditorUtility.SetDirty(registry);
                    AssetDatabase.SaveAssetIfDirty(registry);
                    report["excludedRuntimeAssets"] = excludedRuntimeAssets.Values.ToArray();
                }
                report["typePreservationManifest"] = Path.Combine(output, new DirectoryInfo(output).Name + ".manifest");
            }
            finally
            {
                try { PlayerSettings.SetGraphicsAPIs(BuildTarget.StandaloneLinux64, previousApis); }
                finally
                {
                    try { PlayerSettings.SetUseDefaultGraphicsAPIs(BuildTarget.StandaloneLinux64, defaultApis); }
                    finally
                    {
                        try { PlayerSettings.SetScriptingBackend(NamedBuildTarget.Standalone, previousBackend); }
                        finally { AgentEditorBridge.SavePlayerSettings(); }
                    }
                }
            }
            var sourceRoots = new SortedDictionary<string, string>(StringComparer.Ordinal);
            foreach (PackageInfo package in PackageInfo.GetAllRegisteredPackages())
                if (!string.IsNullOrEmpty(package.name) && !string.IsNullOrEmpty(package.resolvedPath))
                    sourceRoots["Packages/" + package.name] = Path.GetFullPath(package.resolvedPath);
            var dependencies = new List<object>();
            foreach (string path in dependencyPaths)
            {
                string physical = ResolvePhysical(path, sourceRoots);
                if (!File.Exists(physical)) continue; // Built-in Unity resources are part of the versioned engine.
                dependencies.Add(SourceRecord(path, physical));
                if (File.Exists(physical + ".meta")) dependencies.Add(SourceRecord(path + ".meta", physical + ".meta"));
            }
            report["assets"] = assetRecords;
            report["modelPrefabs"] = modelRecords;
            report["deferredBindings"] = deferredBindings;
            report["sourceDependencies"] = dependencies;
            report["sourceRoots"] = sourceRoots.Select(pair => (object)new Dictionary<string, object> { ["path"] = pair.Key, ["source"] = pair.Value }).ToArray();
            report["rootAssets"] = roots.ToArray();
            report["requiredAssemblies"] = assemblies.ToArray();
        }

        private static List<object> BuildNativePacks(string output, SortedDictionary<string, string> explicitAssets)
        {
            var built = BuildPipeline.BuildAssetBundles(new BuildAssetBundlesParameters
            {
                outputPath = output, targetPlatform = BuildTarget.StandaloneLinux64,
                subtarget = (int)StandaloneBuildSubtarget.Player,
                extraScriptingDefines = new[] { "DEVELOPMENT_BUILD", "UNITY_ASSERTIONS", "ENABLE_PROFILER" },
                options = BuildAssetBundleOptions.ChunkBasedCompression | BuildAssetBundleOptions.StrictMode,
                bundleDefinitions = new[] { new AssetBundleBuild
                {
                    assetBundleName = BundleName, assetNames = explicitAssets.Keys.ToArray(),
                    addressableNames = explicitAssets.Values.ToArray(),
                } },
            });
            if (built == null) throw new InvalidOperationException("Unity native asset build returned no manifest.");
            var packs = new List<object>();
            foreach (string name in built.GetAllAssetBundles().OrderBy(value => value, StringComparer.Ordinal))
            {
                string file = Path.Combine(output, name);
                if (!BuildPipeline.GetCRCForAssetBundle(file, out uint crc)) throw new IOException("No native bundle CRC: " + file);
                packs.Add(new Dictionary<string, object>
                {
                    ["bundlePath"] = file, ["crc"] = (long)crc, ["sha256"] = Sha256(file),
                    ["bytes"] = new FileInfo(file).Length, ["registryAsset"] = RegistryPath.ToLowerInvariant(),
                    ["addressableAssets"] = explicitAssets.Values.ToArray(), ["dependencies"] = built.GetAllDependencies(name),
                });
            }
            return packs;
        }

        private static SortedDictionary<string, RegistrySourceSnapshot> CaptureRegistrySources(
            SortedDictionary<string, ImportedAssetRegistry.Entry> values, Dictionary<string, string> pathsByIdentity)
        {
            var captured = new SortedDictionary<string, RegistrySourceSnapshot>(StringComparer.Ordinal);
            foreach (var pair in values)
            {
                ImportedAssetRegistry.Entry entry = pair.Value;
                if (entry.value == null)
                    throw new InvalidOperationException("Pre-build source registry object is null: " + pair.Key + " (" + entry.typeName + ")");
                Type actual = entry.value.GetType();
                string normalized = RuntimeTypeName(entry.value);
                if (normalized != entry.typeName || actual == typeof(ScriptableObject) || actual == typeof(MonoBehaviour))
                    throw new InvalidOperationException("Pre-build source class differs from registry: " + pair.Key
                        + "; expected=" + entry.typeName + "; actual=" + actual.FullName + "; runtime=" + normalized);
                var observation = RuntimeAssetObservation(entry.value, pathsByIdentity[pair.Key]);
                // Native clones have owned IDs; catalog keys keep authored IDs.
                observation["guid"] = entry.guid;
                observation["fileId"] = entry.fileId;
                captured.Add(pair.Key, new RegistrySourceSnapshot
                {
                    guid = entry.guid, fileId = entry.fileId, typeName = entry.typeName,
                    sourceTypeName = actual.FullName, assetPath = AssetDatabase.GetAssetPath(entry.value),
                    globalSettings = entry.value is RenderPipelineGlobalSettings, observation = observation,
                });
            }
            return captured;
        }

        private static List<Dictionary<string, object>> ProbeCookedRegistry(List<object> packs,
            SortedDictionary<string, RegistrySourceSnapshot> expected,
            List<object> globalSettingsAssets)
        {
            var omissions = new List<Dictionary<string, object>>();
            var loaded = new List<AssetBundle>();
            try
            {
                foreach (object value in packs)
                {
                    var pack = (Dictionary<string, object>)value;
                    string file = (string)pack["bundlePath"];
                    var bundle = AssetBundle.LoadFromFile(file, (uint)(long)pack["crc"]);
                    if (bundle == null) throw new InvalidOperationException("Cannot inspect native cooked pack: " + file);
                    loaded.Add(bundle);
                }
                foreach (object value in globalSettingsAssets)
                {
                    var settings = (Dictionary<string, object>)value;
                    string address = (string)settings["assetName"];
                    RenderPipelineGlobalSettings global = null;
                    foreach (AssetBundle bundle in loaded)
                        if (bundle.Contains(address)) global = bundle.LoadAsset<RenderPipelineGlobalSettings>(address);
                    if (global == null) throw new InvalidOperationException("Cooked global settings are not addressable: " + address);
                    RegisterProbeGlobalSettings(global);
                }
                var seen = new HashSet<string>(StringComparer.Ordinal);
                for (int packIndex = 0; packIndex < packs.Count; packIndex++)
                {
                    var pack = (Dictionary<string, object>)packs[packIndex];
                    string registryName = (string)pack["registryAsset"];
                    var cooked = loaded[packIndex].LoadAsset<ImportedAssetRegistry>(registryName);
                    if (cooked == null || cooked.schemaVersion != 1 || cooked.entries == null)
                        throw new InvalidOperationException("Cooked native registry has an invalid type/schema: " + registryName);
                    foreach (ImportedAssetRegistry.Entry entry in cooked.entries)
                    {
                        if (entry == null) throw new InvalidOperationException("Cooked registry contains a null metadata row.");
                        string key = entry.guid + ":" + entry.fileId;
                        if (!seen.Add(key)) throw new InvalidOperationException("Cooked registry repeats an identity: " + key);
                        if (!expected.TryGetValue(key, out RegistrySourceSnapshot original))
                            throw new InvalidOperationException("Cooked registry contains an unknown identity: " + key);
                        if (entry.typeName != original.typeName)
                            throw new InvalidOperationException("Cooked registry type metadata differs from source: " + key
                                + "; expected=" + original.typeName + "; actual=" + entry.typeName);
                        if (entry.value != null)
                        {
                            string actual = RuntimeTypeName(entry.value);
                            if (actual != entry.typeName)
                                throw new InvalidOperationException("Cooked native object type differs from registry: " + key
                                    + "; expected=" + entry.typeName + "; actual=" + entry.value.GetType().FullName
                                    + "; runtime=" + actual + "; source=" + original.sourceTypeName);
                            continue;
                        }
                        var observation = new Dictionary<string, object>(original.observation);
                        observation["reason"] = "nativeCookedOmission";
                        observation["cookedProof"] = new Dictionary<string, object>
                        {
                            ["method"] = "AssetBundle.LoadFromFile/LoadAsset<ImportedAssetRegistry>",
                            ["engineVersion"] = Application.unityVersion, ["target"] = "StandaloneLinux64",
                            ["bundleSha256"] = pack["sha256"], ["bundleCrc"] = pack["crc"],
                            ["registryAsset"] = registryName, ["nativeNullObserved"] = true,
                            ["sourceTypeAvailable"] = true, ["retainedTypesVerified"] = true,
                        };
                        omissions.Add(observation);
                    }
                }
                if (seen.Count != expected.Count) throw new InvalidOperationException("Cooked registry lost identity metadata rows.");
                return omissions;
            }
            finally
            {
                try
                {
                    foreach (RegistrySourceSnapshot original in expected.Values)
                    {
                        if (!original.globalSettings) continue;
                        var global = AssetDatabase.LoadAssetAtPath<RenderPipelineGlobalSettings>(original.assetPath);
                        if (global == null) throw new InvalidOperationException("Source global settings could not be restored after cooked readback: " + original.assetPath);
                        RegisterProbeGlobalSettings(global);
                    }
                }
                finally { foreach (AssetBundle bundle in loaded) bundle.Unload(true); }
            }
        }

        private static void RegisterProbeGlobalSettings(RenderPipelineGlobalSettings settings)
        {
            // Public generic base metadata exposes the pipeline type without
            // referencing a particular render-pipeline package or private API.
            for (Type type = settings.GetType(); type != null; type = type.BaseType)
            {
                if (!type.IsGenericType || type.GetGenericTypeDefinition().FullName != "UnityEngine.Rendering.RenderPipelineGlobalSettings`2") continue;
                Type pipelineType = type.GetGenericArguments()[1];
                if (!typeof(RenderPipeline).IsAssignableFrom(pipelineType)) break;
                UnityEditor.Rendering.EditorGraphicsSettings.SetRenderPipelineGlobalSettingsAsset(pipelineType, settings);
                return;
            }
            throw new NotSupportedException("Cooked global-settings probe requires a public typed render-pipeline base: " + settings.GetType().FullName);
        }

        private static string RuntimeTypeName(Object value)
        {
            Type type = value.GetType();
            if (!IsEditorOnly(type)) return type.FullName;
            if (value is RuntimeAnimatorController) return typeof(RuntimeAnimatorController).FullName;
            if (value is UnityEngine.Audio.AudioMixer) return typeof(UnityEngine.Audio.AudioMixer).FullName;
            if (value is UnityEngine.Audio.AudioMixerGroup) return typeof(UnityEngine.Audio.AudioMixerGroup).FullName;
            if (value is UnityEngine.Audio.AudioMixerSnapshot) return typeof(UnityEngine.Audio.AudioMixerSnapshot).FullName;
            return null;
        }

        private static bool IsNativeRoot(string path)
        {
            if (!path.StartsWith("Assets/", StringComparison.Ordinal) && !path.StartsWith("Packages/", StringComparison.Ordinal)) return false;
            if (NativeExtensions.Contains(Path.GetExtension(path))) return true;
            if (!path.EndsWith(".asset", StringComparison.OrdinalIgnoreCase)) return false;
            Object value = AssetDatabase.LoadMainAssetAtPath(path);
            if (value == null) return false;
            string space = value.GetType().Namespace ?? "";
            return space.StartsWith("UnityEngine.Rendering", StringComparison.Ordinal)
                || space.StartsWith("UnityEngine.InputSystem", StringComparison.Ordinal)
                || space.StartsWith("UnityEngine.UIElements", StringComparison.Ordinal)
                || value.GetType().Assembly.GetName().Name.StartsWith("UnityEngine.", StringComparison.Ordinal)
                || space.StartsWith("KINEMATION.", StringComparison.Ordinal)
                || space.StartsWith("CAS_Demo.", StringComparison.Ordinal);
        }

        private static void AddValue(Object value, string path, SortedDictionary<string, ImportedAssetRegistry.Entry> values,
            SortedSet<string> assemblies, Dictionary<string, string> pathsByIdentity, Dictionary<Object, Object> cloneMap,
            SortedDictionary<string, object> excludedRuntimeAssets)
        {
            if (value == null || value is MonoScript || value is SceneAsset || value is DefaultAsset) return;
            if ((value.hideFlags & HideFlags.DontSaveInBuild) != 0)
            {
                var observation = RuntimeAssetObservation(value, path);
                observation["reason"] = "HideFlags.DontSaveInBuild";
                string identity = (string)observation["guid"] + ":" + (string)observation["fileId"];
                excludedRuntimeAssets[identity] = observation;
                return;
            }
            Type type = value.GetType();
            string runtimeName = type.FullName;
            if (IsEditorOnly(type))
            {
                if (value is RuntimeAnimatorController) runtimeName = typeof(RuntimeAnimatorController).FullName;
                else if (value is UnityEngine.Audio.AudioMixer) runtimeName = typeof(UnityEngine.Audio.AudioMixer).FullName;
                else if (value is UnityEngine.Audio.AudioMixerGroup) runtimeName = typeof(UnityEngine.Audio.AudioMixerGroup).FullName;
                else if (value is UnityEngine.Audio.AudioMixerSnapshot) runtimeName = typeof(UnityEngine.Audio.AudioMixerSnapshot).FullName;
                else return; // ShaderGraph and importer source helper objects are not Player assets.
            }
            if (!AssetDatabase.TryGetGUIDAndLocalFileIdentifier(value, out string guid, out long fileId) || string.IsNullOrEmpty(guid))
                throw new InvalidOperationException("Asset has no GUID/local file identifier: " + path + " " + value.name);
            string key = guid + ":" + fileId.ToString(CultureInfo.InvariantCulture);
            Object exported = cloneMap.TryGetValue(value, out Object nativeClone) ? nativeClone : value;
            if (values.TryGetValue(key, out ImportedAssetRegistry.Entry previous))
            {
                if (previous.value != exported) throw new InvalidOperationException("Duplicate native asset identity: " + key);
                return;
            }
            values.Add(key, new ImportedAssetRegistry.Entry
            {
                guid = guid, fileId = fileId.ToString(CultureInfo.InvariantCulture), typeName = runtimeName, name = value.name,
                value = exported,
            });
            pathsByIdentity.Add(key, path);
            if (!IsEditorOnly(type)) assemblies.Add(type.Assembly.GetName().Name);
        }

        private static Dictionary<string, object> RuntimeAssetObservation(Object value, string path)
        {
            if (!AssetDatabase.TryGetGUIDAndLocalFileIdentifier(value, out string guid, out long fileId) || string.IsNullOrEmpty(guid))
                throw new InvalidOperationException("Observed native asset has no persistent identity: " + path + " " + value.name);
            return new Dictionary<string, object>
            {
                ["guid"] = guid, ["fileId"] = fileId.ToString(CultureInfo.InvariantCulture), ["path"] = path,
                ["typeName"] = value.GetType().FullName, ["name"] = value.name,
                ["hideFlags"] = (int)value.hideFlags, ["hideFlagsName"] = value.hideFlags.ToString(),
                ["dontSaveInBuild"] = (value.hideFlags & HideFlags.DontSaveInBuild) != 0,
            };
        }

        private static string[] NativeDependencies(IEnumerable<string> rootPaths)
        {
            var visited = new SortedSet<string>(StringComparer.Ordinal);
            var pending = new Queue<string>(rootPaths);
            while (pending.Count != 0)
            {
                string path = pending.Dequeue();
                if (!visited.Add(path)) continue;
                foreach (string dependency in AssetDatabase.GetDependencies(path, false))
                {
                    if (dependency.EndsWith(".unity", StringComparison.OrdinalIgnoreCase))
                        throw new InvalidOperationException("Native assets cannot reference an authored scene: " + dependency);
                    // ScriptableObject clones rebind these to raw templates. Do not
                    // drag the prefab's entire scene-like dependency graph into a pack.
                    if (dependency.EndsWith(".prefab", StringComparison.OrdinalIgnoreCase)) continue;
                    if (!visited.Contains(dependency)) pending.Enqueue(dependency);
                }
            }
            return visited.ToArray();
        }

        private static Dictionary<Object, Object> CloneAnimationData(string[] paths, out List<object> bindings)
        {
            var originals = new List<ScriptableObject>();
            foreach (string path in paths)
            {
                if (!path.StartsWith("Assets/", StringComparison.Ordinal) && !path.StartsWith("Packages/", StringComparison.Ordinal)) continue;
                foreach (Object value in AssetDatabase.LoadAllAssetsAtPath(path))
                {
                    if (!(value is ScriptableObject data) || value is MonoScript) continue;
                    Type type = value.GetType();
                    string space = type.Namespace ?? "";
                    if (IsEditorOnly(type)) continue;
                    // Native pipeline/input assets normally remain untouched. A custom
                    // data object keeps its native curves and private serialized fields
                    // in a clone, while authored prefab references are deferred.
                    if (!space.StartsWith("UnityEngine", StringComparison.Ordinal) || HasRawPrefabReference(data)) originals.Add(data);
                }
            }
            // A native SO can reference a custom clone transitively, so include its
            // referrer as well rather than silently preserving an original graph.
            bool changed;
            do
            {
                changed = false;
                foreach (string path in paths)
                    foreach (Object value in AssetDatabase.LoadAllAssetsAtPath(path))
                    {
                        if (!(value is ScriptableObject data) || value is MonoScript || originals.Contains(data)
                            || IsEditorOnly(value.GetType())) continue;
                        using (var serialized = new SerializedObject(data))
                        {
                            SerializedProperty property = serialized.GetIterator();
                            while (property.Next(true))
                                if (property.propertyType == SerializedPropertyType.ObjectReference && property.objectReferenceValue is ScriptableObject target && originals.Contains(target))
                                { originals.Add(data); changed = true; break; }
                        }
                    }
            } while (changed);
            // Instantiate/CopySerialized/Apply can invoke user OnValidate, which
            // may mutate referenced children. Capture the complete original
            // graph before constructing or changing any clone.
            originals = originals.Distinct().ToList();
            bindings = new List<object>();
            var originalReferences = new Dictionary<Object, List<KeyValuePair<string, Object>>>();
            var prefabProperties = new Dictionary<Object, List<string>>();
            foreach (ScriptableObject original in originals)
            {
                if (!AssetDatabase.TryGetGUIDAndLocalFileIdentifier(original, out string guid, out long fileId))
                    throw new InvalidOperationException("Animation data has no persistent identifier: " + original.name);
                var references = new List<KeyValuePair<string, Object>>();
                var prefabs = new List<string>();
                using (var serialized = new SerializedObject(original))
                {
                    SerializedProperty property = serialized.GetIterator();
                    while (property.Next(true))
                    {
                        if (property.propertyType != SerializedPropertyType.ObjectReference || property.objectReferenceValue == null) continue;
                        Object target = property.objectReferenceValue;
                        if (target is ScriptableObject data && originals.Contains(data))
                            references.Add(new KeyValuePair<string, Object>(property.propertyPath, target));
                        string targetPath = AssetDatabase.GetAssetPath(target);
                        if (targetPath.EndsWith(".unity", StringComparison.OrdinalIgnoreCase))
                            throw new InvalidOperationException("Animation data references an authored scene: " + targetPath);
                        if (!targetPath.EndsWith(".prefab", StringComparison.OrdinalIgnoreCase)) continue;
                        if (!AssetDatabase.TryGetGUIDAndLocalFileIdentifier(target, out string targetGuid, out long targetId))
                            throw new InvalidOperationException("Deferred prefab reference has no persistent identity: " + targetPath);
                        prefabs.Add(property.propertyPath);
                        bindings.Add(new Dictionary<string, object>
                        {
                            ["ownerGuid"] = guid, ["ownerFileId"] = fileId.ToString(CultureInfo.InvariantCulture),
                            ["propertyPath"] = property.propertyPath, ["targetGuid"] = targetGuid,
                            ["targetFileId"] = targetId.ToString(CultureInfo.InvariantCulture), ["targetPath"] = targetPath,
                        });
                    }
                }
                originalReferences.Add(original, references);
                prefabProperties.Add(original, prefabs);
            }
            string folder = "Assets/__UnityPlayImports/Data";
            Directory.CreateDirectory(folder);
            AssetDatabase.Refresh(ImportAssetOptions.ForceSynchronousImport);
            var clones = new Dictionary<Object, Object>();
            foreach (ScriptableObject original in originals)
            {
                if (!AssetDatabase.TryGetGUIDAndLocalFileIdentifier(original, out string guid, out long fileId))
                    throw new InvalidOperationException("Animation data has no persistent identifier: " + original.name);
                string clonePath = folder + "/" + guid + "-" + fileId.ToString(CultureInfo.InvariantCulture) + ".asset";
                var temporary = Object.Instantiate(original);
                temporary.name = original.name;
                ScriptableObject clone = AssetDatabase.LoadAssetAtPath<ScriptableObject>(clonePath);
                if (clone != null && clone.GetType() == original.GetType())
                {
                    EditorUtility.CopySerialized(temporary, clone);
                    Object.DestroyImmediate(temporary);
                }
                else
                {
                    if (File.Exists(clonePath) && !AssetDatabase.DeleteAsset(clonePath))
                        throw new IOException("Could not replace the tool-owned data clone: " + clonePath);
                    clone = temporary;
                    AssetDatabase.CreateAsset(clone, clonePath);
                }
                clone.name = original.name;
                clones.Add(original, clone);
            }
            foreach (var pair in clones)
            {
                using (var serialized = new SerializedObject(pair.Value))
                {
                    foreach (var reference in originalReferences[pair.Key])
                    {
                        SerializedProperty property = RequireObjectProperty(serialized, reference.Key);
                        property.objectReferenceValue = clones[reference.Value];
                    }
                    foreach (string path in prefabProperties[pair.Key])
                    {
                        SerializedProperty property = RequireObjectProperty(serialized, path);
                        property.objectReferenceValue = null;
                    }
                    serialized.ApplyModifiedPropertiesWithoutUndo();
                }
                EditorUtility.SetDirty(pair.Value);
                AssetDatabase.SaveAssetIfDirty(pair.Value);
            }
            foreach (var pair in clones)
                using (var serialized = new SerializedObject(pair.Value))
                    foreach (string path in prefabProperties[pair.Key])
                        if (RequireObjectProperty(serialized, path).objectReferenceValue != null)
                            throw new InvalidOperationException("A cloned deferred prefab reference was restored during native validation: " + path);
            return clones;
        }

        private static SerializedProperty RequireObjectProperty(SerializedObject serialized, string path)
        {
            SerializedProperty property = serialized.FindProperty(path);
            if (property == null || property.propertyType != SerializedPropertyType.ObjectReference)
                throw new InvalidOperationException("A native clone lost its original serialized reference property: " + path);
            return property;
        }

        private static bool HasRawPrefabReference(ScriptableObject value)
        {
            using (var serialized = new SerializedObject(value))
            {
                SerializedProperty property = serialized.GetIterator();
                while (property.Next(true))
                    if (property.propertyType == SerializedPropertyType.ObjectReference && property.objectReferenceValue != null
                        && AssetDatabase.GetAssetPath(property.objectReferenceValue).EndsWith(".prefab", StringComparison.OrdinalIgnoreCase)) return true;
            }
            return false;
        }

        private static object SceneMetadata(string[] paths)
        {
            SceneSetup[] previous = EditorSceneManager.GetSceneManagerSetup();
            var records = new List<object>();
            try
            {
                foreach (string path in paths)
                {
                    var scene = EditorSceneManager.OpenScene(path, OpenSceneMode.Single);
                    if (!scene.IsValid() || !scene.isLoaded) throw new InvalidOperationException("Could not open source scene for native metadata: " + path);
                    int baked = 0;
                    foreach (GameObject root in scene.GetRootGameObjects())
                        foreach (Renderer renderer in root.GetComponentsInChildren<Renderer>(true))
                            if (renderer.lightmapIndex >= 0 && renderer.lightmapIndex < 65534) baked++;
                    records.Add(new Dictionary<string, object>
                    {
                        ["scenePath"] = path, ["lightmapsCount"] = LightmapSettings.lightmaps == null ? 0 : LightmapSettings.lightmaps.Length,
                        ["bakedRenderersCount"] = baked, ["capturedInPlayMode"] = EditorApplication.isPlaying,
                    });
                }
            }
            finally
            {
                // No source scene is saved. These are tool-owned copies even if
                // an ExecuteAlways component changes their in-memory state.
                if (previous.Length != 0) EditorSceneManager.RestoreSceneManagerSetup(previous);
                else EditorSceneManager.NewScene(NewSceneSetup.EmptyScene, NewSceneMode.Single);
            }
            return records;
        }

        private static object ProbeIgnoredOverrides(OverrideProbe[] probes)
        {
            var records = new List<object>();
            foreach (var group in probes.GroupBy(probe => probe.path, StringComparer.Ordinal))
            {
                ValidateLogicalPath(group.Key);
                var root = AssetDatabase.LoadAssetAtPath<GameObject>(group.Key);
                if (root == null) throw new InvalidOperationException("Prefab override probe source is missing: " + group.Key);
                Component[] components = root.GetComponentsInChildren<Component>(true);
                foreach (OverrideProbe probe in group)
                {
                    var matches = new List<Component>();
                    foreach (Component component in components)
                    {
                        if (component == null) continue;
                        Object current = component;
                        var visited = new HashSet<EntityId>();
                        while (current != null && visited.Add(current.GetEntityId()))
                        {
                            if (AssetDatabase.TryGetGUIDAndLocalFileIdentifier(current, out string guid, out long localId)
                                && guid == probe.targetGuid && localId.ToString(CultureInfo.InvariantCulture) == probe.targetFileId)
                            { matches.Add(component); break; }
                            current = PrefabUtility.GetCorrespondingObjectFromSource(current);
                        }
                    }
                    if (matches.Count != 1) continue; // Ambiguous matches never justify ignoring authored data.
                    using (var serialized = new SerializedObject(matches[0]))
                    {
                        SerializedProperty array = serialized.FindProperty(probe.arrayPath);
                        SerializedProperty property = serialized.FindProperty(probe.propertyPath);
                        if (array == null || !array.isArray || array.arraySize != probe.expectedArraySize || property != null) continue;
                        records.Add(new Dictionary<string, object>
                        {
                            ["path"] = probe.path, ["instanceFileId"] = probe.instanceFileId,
                            ["targetGuid"] = probe.targetGuid, ["targetFileId"] = probe.targetFileId,
                            ["propertyPath"] = probe.propertyPath, ["arrayPath"] = probe.arrayPath,
                            ["observedArraySize"] = array.arraySize, ["propertyExists"] = false,
                        });
                    }
                }
            }
            return records;
        }

        private static object ObserveAuthoredPrefabs(Request request, SortedSet<string> nativeRoots)
        {
            var paths = new SortedSet<string>(StringComparer.Ordinal);
            foreach (string dependency in AssetDatabase.GetDependencies(nativeRoots.Concat(request.scenes ?? Array.Empty<string>()).ToArray(), true))
                if (dependency.EndsWith(".prefab", StringComparison.OrdinalIgnoreCase)) paths.Add(dependency);
            var observations = new List<object>();
            foreach (string path in paths)
            {
                var root = AssetDatabase.LoadAssetAtPath<GameObject>(path);
                if (root == null) throw new InvalidOperationException("Cannot observe authored prefab identifiers: " + path);
                string guid = AssetDatabase.AssetPathToGUID(path);
                var ids = new SortedSet<string>(StringComparer.Ordinal);
                bool complete = true;
                int count = 0;
                var missing = new List<object>();
                foreach (Transform transform in root.GetComponentsInChildren<Transform>(true))
                {
                    var objects = new List<Object> { transform.gameObject };
                    Component[] components = transform.GetComponents<Component>();
                    objects.AddRange(components);
                    if (AssetDatabase.TryGetGUIDAndLocalFileIdentifier(transform.gameObject, out string ownerGuid, out long ownerId) && ownerGuid == guid)
                    {
                        for (int index = 0; index < components.Length; index++)
                            if (components[index] == null)
                                missing.Add(new Dictionary<string, object>
                                {
                                    ["gameObjectFileId"] = ownerId.ToString(CultureInfo.InvariantCulture),
                                    ["componentIndex"] = index, ["observedMissing"] = true,
                                });
                    }
                    foreach (Object value in objects)
                    {
                        count++;
                        if (value == null || !AssetDatabase.TryGetGUIDAndLocalFileIdentifier(value, out string valueGuid, out long id) || valueGuid != guid)
                        { complete = false; continue; }
                        ids.Add(id.ToString(CultureInfo.InvariantCulture));
                    }
                }
                observations.Add(new Dictionary<string, object>
                {
                    ["guid"] = guid, ["path"] = path, ["fileIds"] = ids.ToArray(), ["complete"] = complete, ["observedObjectCount"] = count,
                    ["missingComponents"] = missing,
                });
            }
            return observations;
        }

        private static object ObserveCurveWrapModes()
        {
            var records = new List<object>();
            var carrier = ScriptableObject.CreateInstance<CurveWrapCarrier>();
            carrier.hideFlags = HideFlags.HideAndDontSave;
            try
            {
                foreach (WrapMode mode in new[] { WrapMode.Default, WrapMode.Once, WrapMode.Loop, WrapMode.PingPong, WrapMode.ClampForever })
                {
                    carrier.curve.preWrapMode = mode;
                    carrier.curve.postWrapMode = mode;
                    string json = EditorJsonUtility.ToJson(carrier);
                    int pre = CurveInfinity(json, "m_PreInfinity");
                    int post = CurveInfinity(json, "m_PostInfinity");
                    records.Add(new Dictionary<string, object>
                    {
                        ["requestedName"] = mode.ToString(), ["requestedPublicCode"] = (int)mode,
                        ["actualPublicPreWrapMode"] = (int)carrier.curve.preWrapMode,
                        ["actualPublicPostWrapMode"] = (int)carrier.curve.postWrapMode,
                        ["serializedPreInfinity"] = pre, ["serializedPostInfinity"] = post,
                        ["serializedJson"] = json,
                    });
                }
            }
            finally { Object.DestroyImmediate(carrier); }
            return records;
        }

        private static int CurveInfinity(string json, string field)
        {
            // EditorJsonUtility wraps ScriptableObjects in a native class key,
            // unlike JsonUtility. This controlled carrier has exactly one curve.
            MatchCollection matches = Regex.Matches(json, "\\\"" + Regex.Escape(field) + "\\\"\\s*:\\s*(-?[0-9]+)");
            if (matches.Count != 1 || !int.TryParse(matches[0].Groups[1].Value, NumberStyles.Integer, CultureInfo.InvariantCulture, out int value))
                throw new InvalidOperationException("Cannot observe native curve " + field + ": " + json);
            return value;
        }

        private static bool IsEditorOnly(Type type)
        {
            string name = type.Assembly.GetName().Name;
            return name.StartsWith("UnityEditor", StringComparison.Ordinal)
                || name.EndsWith(".Editor", StringComparison.Ordinal) || name.Contains(".Editor.")
                || (type.Namespace ?? "").StartsWith("UnityEditor", StringComparison.Ordinal);
        }

        private static object ModelObject(Object value, string modelGuid)
        {
            if (!AssetDatabase.TryGetGUIDAndLocalFileIdentifier(value, out string guid, out long fileId) || guid != modelGuid)
                throw new InvalidOperationException("Model object has an unexpected persistent identifier.");
            int classId = ClassId(value.GetType());
            if (classId == 0) throw new NotSupportedException("Native model component is not described yet: " + value.GetType().FullName);
            var data = new Dictionary<string, object>(StringComparer.Ordinal);
            using (var serialized = new SerializedObject(value))
            {
                SerializedProperty iterator = serialized.GetIterator();
                if (iterator.Next(true))
                    do { data[iterator.name] = PropertyValue(iterator, modelGuid, 0); } while (iterator.Next(false));
            }
            if (value is GameObject go)
            {
                data["m_Name"] = go.name;
                data["m_IsActive"] = go.activeSelf ? 1 : 0;
                data["m_Layer"] = go.layer;
                data["m_TagString"] = go.tag;
                data["m_Component"] = go.GetComponents<Component>().Select(component => (object)new Dictionary<string, object>
                    { ["component"] = Reference(component, modelGuid) }).ToArray();
            }
            if (value is Transform transform) data["m_RootOrder"] = transform.GetSiblingIndex();
            return new Dictionary<string, object>
            {
                ["fileId"] = fileId.ToString(CultureInfo.InvariantCulture), ["classId"] = classId,
                ["typeName"] = value.GetType().FullName, ["data"] = data,
            };
        }

        private static object PropertyValue(SerializedProperty property, string modelGuid, int depth)
        {
            if (depth > 64) throw new NotSupportedException("Model serialized data nesting exceeds 64: " + property.propertyPath);
            if (property.isArray && property.propertyType != SerializedPropertyType.String)
            {
                var array = new object[property.arraySize];
                for (int index = 0; index < array.Length; index++) array[index] = PropertyValue(property.GetArrayElementAtIndex(index), modelGuid, depth + 1);
                return array;
            }
            switch (property.propertyType)
            {
                case SerializedPropertyType.Integer: return property.longValue;
                case SerializedPropertyType.Boolean: return property.boolValue ? 1 : 0;
                case SerializedPropertyType.Float: return property.doubleValue;
                case SerializedPropertyType.String: return property.stringValue;
                case SerializedPropertyType.Enum: return property.intValue;
                case SerializedPropertyType.LayerMask: return new Dictionary<string, object> { ["m_Bits"] = property.intValue };
                case SerializedPropertyType.ObjectReference: return Reference(property.objectReferenceValue, modelGuid);
                case SerializedPropertyType.Vector2: return Vector(property.vector2Value.x, property.vector2Value.y);
                case SerializedPropertyType.Vector3: return Vector(property.vector3Value.x, property.vector3Value.y, property.vector3Value.z);
                case SerializedPropertyType.Vector4: return Vector(property.vector4Value.x, property.vector4Value.y, property.vector4Value.z, property.vector4Value.w);
                case SerializedPropertyType.Quaternion: return Vector(property.quaternionValue.x, property.quaternionValue.y, property.quaternionValue.z, property.quaternionValue.w);
                case SerializedPropertyType.Color:
                    Color color = property.colorValue;
                    return new Dictionary<string, object> { ["r"] = color.r, ["g"] = color.g, ["b"] = color.b, ["a"] = color.a };
                case SerializedPropertyType.Bounds:
                    Bounds bounds = property.boundsValue;
                    return new Dictionary<string, object> { ["m_Center"] = Vector(bounds.center.x, bounds.center.y, bounds.center.z), ["m_Extent"] = Vector(bounds.extents.x, bounds.extents.y, bounds.extents.z) };
                case SerializedPropertyType.Generic:
                    var result = new Dictionary<string, object>(StringComparer.Ordinal);
                    SerializedProperty child = property.Copy();
                    SerializedProperty end = property.GetEndProperty();
                    if (child.Next(true))
                        while (!SerializedProperty.EqualContents(child, end) && child.depth > property.depth)
                        {
                            result[child.name] = PropertyValue(child, modelGuid, depth + 1);
                            if (!child.Next(false)) break;
                        }
                    return result;
                case SerializedPropertyType.Character: return property.intValue;
                case SerializedPropertyType.Vector2Int: return Vector(property.vector2IntValue.x, property.vector2IntValue.y);
                case SerializedPropertyType.Vector3Int: return Vector(property.vector3IntValue.x, property.vector3IntValue.y, property.vector3IntValue.z);
                default: throw new NotSupportedException("Native model property is not described yet: " + property.propertyPath + " " + property.propertyType);
            }
        }

        private static object Reference(Object value, string modelGuid)
        {
            if (value == null) return new Dictionary<string, object> { ["fileID"] = "0" };
            if (!AssetDatabase.TryGetGUIDAndLocalFileIdentifier(value, out string guid, out long fileId))
                throw new NotSupportedException("Model property points to a non-persistent object: " + value.name);
            var result = new Dictionary<string, object> { ["fileID"] = fileId.ToString(CultureInfo.InvariantCulture) };
            if (guid != modelGuid)
            {
                result["guid"] = guid;
                string path = AssetDatabase.GetAssetPath(value);
                result["type"] = path.EndsWith(".mat", StringComparison.OrdinalIgnoreCase) || path.EndsWith(".asset", StringComparison.OrdinalIgnoreCase) ? 2 : 3;
            }
            return result;
        }

        private static object Vector(params double[] values)
        {
            string[] keys = { "x", "y", "z", "w" };
            var result = new Dictionary<string, object>();
            for (int index = 0; index < values.Length; index++) result[keys[index]] = values[index];
            return result;
        }

        private static int ClassId(Type type)
        {
            if (ClassIds.TryGetValue(type, out int value)) return value;
            if (typeof(MonoBehaviour).IsAssignableFrom(type) || typeof(ScriptableObject).IsAssignableFrom(type)) return 114;
            if (typeof(RuntimeAnimatorController).IsAssignableFrom(type)) return 91;
            return 0;
        }

        private static object SourceRecord(string path, string physical)
        {
            return new Dictionary<string, object> { ["path"] = path, ["physicalPath"] = physical, ["sha256"] = Sha256(physical), ["bytes"] = new FileInfo(physical).Length };
        }

        private static string ResolvePhysical(string path, SortedDictionary<string, string> roots)
        {
            if (!path.StartsWith("Packages/", StringComparison.Ordinal)) return Path.GetFullPath(path);
            PackageInfo info = PackageInfo.FindForAssetPath(path);
            if (info == null) throw new IOException("Cannot resolve imported package asset: " + path);
            string prefix = "Packages/" + info.name;
            string root = Path.GetFullPath(info.resolvedPath);
            roots[prefix] = root;
            string physical = Path.GetFullPath(Path.Combine(root, path.Substring(prefix.Length).TrimStart('/')));
            if (!physical.StartsWith(root + Path.DirectorySeparatorChar, StringComparison.Ordinal)) throw new IOException("Package asset escapes package root: " + path);
            return physical;
        }

        private static string RequireAbsolute(string path, string label)
        {
            if (string.IsNullOrWhiteSpace(path) || !Path.IsPathRooted(path)) throw new ArgumentException(label + " must be an absolute path.");
            return Path.GetFullPath(path);
        }

        private static void ValidateLogicalPath(string path)
        {
            if (string.IsNullOrWhiteSpace(path) || Path.IsPathRooted(path) || path.Contains("\\")
                || path.Split('/').Any(part => part == "..") || !(path.StartsWith("Assets/", StringComparison.Ordinal) || path.StartsWith("Packages/", StringComparison.Ordinal)))
                throw new ArgumentException("Asset path must be project-relative Assets/ or Packages/: " + path);
            if (path.Split('/').Any(part => part.Length == 0 || part == ".")) throw new ArgumentException("Asset path must be canonical: " + path);
            var roots = new SortedDictionary<string, string>(StringComparer.Ordinal);
            AgentEditorBridge.Canonical(ResolvePhysical(path, roots));
        }

        private static string Sha256(string path)
        {
            using (var algorithm = SHA256.Create())
            using (var stream = File.OpenRead(path)) return BitConverter.ToString(algorithm.ComputeHash(stream)).Replace("-", "").ToLowerInvariant();
        }

        // Exact data shapes only. No reflection into engine objects or user components.
        private static class Json
        {
            public static string Write(object value)
            {
                var buffer = new StringBuilder();
                Append(buffer, value);
                return buffer.ToString();
            }

            private static void Append(StringBuilder buffer, object value)
            {
                if (value == null) { buffer.Append("null"); return; }
                if (value is string text) { String(buffer, text); return; }
                if (value is bool flag) { buffer.Append(flag ? "true" : "false"); return; }
                if (value is IDictionary<string, object> dictionary)
                {
                    buffer.Append('{'); bool first = true;
                    foreach (var pair in dictionary.OrderBy(pair => pair.Key, StringComparer.Ordinal))
                    {
                        if (!first) buffer.Append(','); first = false;
                        String(buffer, pair.Key); buffer.Append(':'); Append(buffer, pair.Value);
                    }
                    buffer.Append('}'); return;
                }
                if (value is IEnumerable sequence)
                {
                    buffer.Append('['); bool first = true;
                    foreach (object item in sequence) { if (!first) buffer.Append(','); first = false; Append(buffer, item); }
                    buffer.Append(']'); return;
                }
                if (value is float single)
                {
                    if (float.IsNaN(single) || float.IsInfinity(single)) throw new InvalidOperationException("Non-finite model property.");
                    buffer.Append(single.ToString("R", CultureInfo.InvariantCulture)); return;
                }
                if (value is double number)
                {
                    if (double.IsNaN(number) || double.IsInfinity(number)) throw new InvalidOperationException("Non-finite model property.");
                    buffer.Append(number.ToString("R", CultureInfo.InvariantCulture)); return;
                }
                if (value is int || value is long || value is uint || value is ulong || value is short || value is byte)
                { buffer.Append(Convert.ToString(value, CultureInfo.InvariantCulture)); return; }
                throw new NotSupportedException("JSON writer does not support " + value.GetType().FullName);
            }

            private static void String(StringBuilder buffer, string text)
            {
                buffer.Append('"');
                foreach (char character in text)
                {
                    switch (character)
                    {
                        case '"': buffer.Append("\\\""); break;
                        case '\\': buffer.Append("\\\\"); break;
                        case '\n': buffer.Append("\\n"); break;
                        case '\r': buffer.Append("\\r"); break;
                        case '\t': buffer.Append("\\t"); break;
                        default:
                            if (character < 32) buffer.Append("\\u").Append(((int)character).ToString("x4")); else buffer.Append(character);
                            break;
                    }
                }
                buffer.Append('"');
            }
        }
    }
}
