using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Security.Cryptography;
using UnityMicroEditor;
using MCPForUnity.Editor.Services;
using MCPForUnity.Editor.Tools;
using Newtonsoft.Json.Linq;
using UnityEditor;
using UnityEditor.Compilation;
using UnityEditor.SceneManagement;
using UnityEngine;
using UnityEngine.SceneManagement;
using Object = UnityEngine.Object;

namespace AgentParity.Editor
{
    /// <summary>Reference fixture, snapshots and compiler inventory through public Unity/Coplay APIs.</summary>
    [McpForUnityTool("agent_parity_reference", Description = "Capture deterministic Editor parity snapshots or export the public Player compilation graph.")]
    public static class EditorParityReference
    {
        public const string ScenePath = "Assets/Scenes/AgentParity.unity";
        private const string OutputRelative = "Temp/EditorParity";

        public sealed class Parameters
        {
            [ToolParameter("snapshot, scene_proof, export_graph or reload", Required = false, DefaultValue = "snapshot")]
            public string action { get; set; }
            [ToolParameter("Optional simple output filename under Temp/EditorParity", Required = false)]
            public string output { get; set; }
        }

        [InitializeOnLoadMethod]
        private static void ResumeStartup()
        {
            if (string.IsNullOrEmpty(Environment.GetEnvironmentVariable("AGENT_PARITY_COPLAY_URL"))) return;
            if (SessionState.GetBool("AgentParity.Started", false)) return;
            EditorApplication.delayCall += StartFromCommandLine;
        }

        public static async void StartFromCommandLine()
        {
            if (SessionState.GetBool("AgentParity.Started", false)) return;
            SessionState.SetBool("AgentParity.Started", true);
            try
            {
                if (!File.Exists(ScenePath)) CreateFixture();
                else EditorSceneManager.OpenScene(ScenePath, OpenSceneMode.Single);
                Write("authoring.json", Snapshot());
                Write("scene-proof.json", SceneProof());
                Write("graph.json", ExportGraph());
                var config = EditorConfigurationCache.Instance;
                config.SetUseHttpTransport(true);
                config.SetHttpTransportScope("local");
                config.SetHttpBaseUrl(Environment.GetEnvironmentVariable("AGENT_PARITY_COPLAY_URL") ?? "http://127.0.0.1:8087");
                string uvx = Environment.GetEnvironmentVariable("AGENT_PARITY_UVX_PATH");
                if (!string.IsNullOrEmpty(uvx)) config.SetUvxPathOverride(uvx);
                bool connected = await MCPServiceLocator.Bridge.StartAsync();
                Write("connection.json", new JObject { ["success"] = connected, ["url"] = config.HttpBaseUrl,
                    ["pid"] = System.Diagnostics.Process.GetCurrentProcess().Id, ["unityVersion"] = Application.unityVersion });
                Debug.Log("[agent-parity] reference fixture ready; Coplay connected=" + connected);
            }
            catch (Exception exception) { Write("startup-error.json", new JObject { ["success"] = false, ["error"] = exception.ToString() }); Debug.LogException(exception); }
        }

        public static object HandleCommand(JObject parameters)
        {
            try
            {
                string action = (string)parameters["action"] ?? "snapshot";
                JObject result;
                switch (action)
                {
                    case "snapshot": result = Snapshot(); break;
                    case "scene_proof": result = SceneProof(); break;
                    case "export_graph": result = ExportGraph(); break;
                    case "reload":
                        if (EditorApplication.isPlayingOrWillChangePlaymode) throw new InvalidOperationException("Reload requires Edit Mode.");
                        EditorSceneManager.OpenScene(ScenePath, OpenSceneMode.Single); result = Snapshot(); break;
                    default: throw new ArgumentException("Unknown reference action " + action);
                }
                string output = (string)parameters["output"];
                if (output != null) Write(output, result);
                return new JObject { ["success"] = true, ["data"] = result };
            }
            catch (Exception exception) { return new JObject { ["success"] = false, ["error"] = exception.Message }; }
        }

        private static void CreateFixture()
        {
            Scene scene = EditorSceneManager.NewScene(NewSceneSetup.EmptyScene, NewSceneMode.Single);
            var root = new GameObject("ParityRoot");
            root.transform.localPosition = new Vector3(10, 2, 3);
            root.AddComponent<AgentParityProbe>();
            var cubeSource = GameObject.CreatePrimitive(PrimitiveType.Cube);
            cubeSource.name = "ParityCube";
            Directory.CreateDirectory("Assets/Prefabs");
            var prefab = PrefabUtility.SaveAsPrefabAsset(cubeSource, "Assets/Prefabs/AgentParityCube.prefab");
            Object.DestroyImmediate(cubeSource);
            var cube = (GameObject)PrefabUtility.InstantiatePrefab(prefab, scene);
            cube.name = "ParityCube";
            cube.transform.SetParent(root.transform, false);
            cube.transform.localPosition = new Vector3(1, 0, 0);
            cube.transform.localScale = new Vector3(2, 1, 1);
            PrefabUtility.RecordPrefabInstancePropertyModifications(cube.transform);
            PrefabUtility.RecordPrefabInstancePropertyModifications(cube);
            var child = new GameObject("ParityChild");
            child.transform.SetParent(root.transform, false);
            child.transform.localPosition = new Vector3(0, 1, 0);
            child.SetActive(false);
            var camera = new GameObject("ParityCamera").AddComponent<Camera>();
            camera.tag = "MainCamera";
            camera.transform.position = new Vector3(10, 5, -8);
            camera.transform.LookAt(cube.transform);
            camera.clearFlags = CameraClearFlags.SolidColor;
            camera.backgroundColor = new Color(0.12f, 0.18f, 0.24f, 1);
            var light = new GameObject("ParityLight").AddComponent<Light>();
            light.type = LightType.Directional;
            light.transform.rotation = Quaternion.Euler(40, -30, 0);
            RenderSettings.ambientLight = new Color(0.5f, 0.5f, 0.5f, 1);
            RenderSettings.skybox = null;
            Directory.CreateDirectory("Assets/Scenes");
            EditorSceneManager.SaveScene(scene, ScenePath);
            AssetDatabase.SaveAssets();
        }

        public static JObject Snapshot()
        {
            var scene = SceneManager.GetActiveScene();
            var objects = new JArray();
            foreach (var root in AgentRuntimeSnapshot.LiveRoots()) Capture(root.transform, "", objects);
            return new JObject { ["schemaVersion"] = 1, ["unityVersion"] = Application.unityVersion,
                ["scenePath"] = scene.path, ["sceneName"] = scene.name, ["isPlaying"] = EditorApplication.isPlaying,
                ["objects"] = objects, ["sceneProof"] = SceneProof() };
        }

        public static JObject SceneProof()
        {
            var scene = SceneManager.GetActiveScene();
            var lighting = Lightmapping.lightingDataAsset;
            string guid = string.Empty;
            long fileId = 0;
            bool identityKnown = lighting != null && AssetDatabase.TryGetGUIDAndLocalFileIdentifier(lighting, out guid, out fileId);
            return new JObject { ["schemaVersion"] = 1, ["projectRoot"] = Directory.GetParent(Application.dataPath).FullName,
                ["unityVersion"] = Application.unityVersion, ["scenePath"] = scene.path, ["sceneSHA256"] = Hash(scene.path),
                ["lightmapCount"] = LightmapSettings.lightmaps.Length, ["lightProbeCount"] = LightmapSettings.lightProbes == null ? 0 : LightmapSettings.lightProbes.count,
                ["activeSceneCount"] = SceneManager.sceneCount, ["isPlaying"] = EditorApplication.isPlaying, ["sceneDirty"] = scene.isDirty,
                ["lightingDataAssetPath"] = lighting == null ? null : AssetDatabase.GetAssetPath(lighting),
                ["lightingDataIdentityKnown"] = identityKnown, ["lightingDataGuid"] = guid, ["lightingDataFileId"] = fileId };
        }

        private static void Capture(Transform transform, string parent, JArray objects)
        {
            var go = transform.gameObject;
            string path = parent.Length == 0 ? go.name : parent + "/" + go.name;
            var result = JObject.Parse(AgentRuntimeSnapshot.CaptureJson(go));
            var identity = GlobalObjectId.GetGlobalObjectIdSlow(go);
            result["globalObjectId"] = identity.ToString();
            result["prefabInstance"] = PrefabUtility.IsPartOfPrefabInstance(go);
            objects.Add(result);
            foreach (Transform child in transform) Capture(child, path, objects);
        }

        public static JObject ExportGraph()
        {
            string project = Directory.GetParent(Application.dataPath).FullName;
            var assemblies = new JArray();
            var scripts = new JArray();
            var hashes = new JObject();
            foreach (var assembly in CompilationPipeline.GetAssemblies(AssembliesType.Player).OrderBy(x => x.name, StringComparer.Ordinal))
            {
                if (!assembly.sourceFiles.Any(x => x.StartsWith("Assets/", StringComparison.Ordinal))) continue;
                var sources = assembly.sourceFiles.Select(x => Path.GetFullPath(x)).OrderBy(x => x, StringComparer.Ordinal).ToArray();
                foreach (var source in sources)
                {
                    hashes[source] = Hash(source);
                    if (File.Exists(source + ".meta")) hashes[source + ".meta"] = Hash(source + ".meta");
                    string relative = "Assets" + source.Substring(Application.dataPath.Length);
                    var script = AssetDatabase.LoadAssetAtPath<MonoScript>(relative);
                    var type = script?.GetClass();
                    if (type == null) continue;
                    scripts.Add(new JObject { ["guid"] = AssetDatabase.AssetPathToGUID(relative), ["path"] = source,
                        ["assembly"] = assembly.name, ["typeName"] = type.FullName, ["hostProvided"] = false });
                }
                string path = Path.GetFullPath(assembly.outputPath);
                assemblies.Add(new JObject { ["name"] = assembly.name, ["path"] = path, ["sha256"] = Hash(path),
                    ["sources"] = new JArray(sources), ["references"] = new JArray(assembly.assemblyReferences.Select(x => x.name).OrderBy(x => x, StringComparer.Ordinal)),
                    ["compiledReferences"] = new JArray(assembly.compiledAssemblyReferences.Select(Path.GetFullPath)),
                    ["defines"] = new JArray(assembly.defines.OrderBy(x => x, StringComparer.Ordinal)),
                    ["nativeBaseAssemblies"] = new JArray(), ["config"] = new JObject {
                        ["allowUnsafeCode"] = assembly.compilerOptions.AllowUnsafeCode, ["noEngineReferences"] = false,
                        ["overrideReferences"] = false, ["precompiledReferences"] = new JArray() } });
            }
            foreach (string relative in new[] { "ProjectSettings/ProjectVersion.txt", "ProjectSettings/ProjectSettings.asset", "Packages/manifest.json", "Packages/packages-lock.json" })
            { string path = Path.Combine(project, relative); if (File.Exists(path)) hashes[path] = Hash(path); }
            return new JObject { ["schemaVersion"] = 1, ["project"] = project, ["unity"] = EditorApplication.applicationPath,
                ["unityVersion"] = Application.unityVersion, ["assemblies"] = assemblies, ["scripts"] = scripts, ["compilerInputs"] = hashes };
        }

        private static string Hash(string path)
        { using (var sha = SHA256.Create()) using (var file = File.OpenRead(path)) return BitConverter.ToString(sha.ComputeHash(file)).Replace("-", "").ToLowerInvariant(); }

        private static void Write(string filename, JObject data)
        {
            if (Path.IsPathRooted(filename) || filename != Path.GetFileName(filename)) throw new ArgumentException("Output must be a simple filename.");
            Directory.CreateDirectory(OutputRelative);
            File.WriteAllText(Path.Combine(OutputRelative, filename), data.ToString() + "\n");
        }
    }
}
