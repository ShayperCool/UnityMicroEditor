using System;
using System.IO;
using UnityEditor;
using UnityEditor.Build;
using UnityEditor.Build.Reporting;
using UnityEditor.SceneManagement;
using UnityEngine;
using UnityEngine.Rendering;

namespace UnityMicroEditor.HostBuild
{
    /// <summary>Creates only missing host content in a dedicated tool project.</summary>
    public static partial class Build
    {
        static partial void PreparePackageProfile();

        public static void FromCommandLine()
        {
            string output = Environment.GetEnvironmentVariable("UNITY_RAW_HOST_OUTPUT");
            string reportPath = Environment.GetEnvironmentVariable("UNITY_RAW_HOST_REPORT");
            var manifest = new Manifest { engineVersion = Application.unityVersion };
            try
            {
                if (Application.unityVersion != "6000.6.3f1")
                    throw new InvalidOperationException("This host profile requires Unity 6000.6.3f1. Port and verify a new engine profile before building with another version.");
                if (!Application.isBatchMode || !Path.IsPathRooted(output) || !Path.IsPathRooted(reportPath))
                    throw new ArgumentException("Host build requires batchmode and absolute output/report paths.");
                const string scenePath = "Assets/Bootstrap.unity";
                if (!File.Exists(scenePath))
                {
                    var scene = EditorSceneManager.NewScene(NewSceneSetup.EmptyScene, NewSceneMode.Single);
                    if (!EditorSceneManager.SaveScene(scene, scenePath)) throw new IOException("Cannot save host bootstrap scene.");
                }
                // Resources retain a fixed material/shader baseline, with no input-project assets.
                Directory.CreateDirectory("Assets/Resources/HostShaders");
                foreach (string shaderName in new[] { "Standard", "Unlit/Texture", "Unlit/Color" })
                {
                    string path = "Assets/Resources/HostShaders/" + shaderName.Replace('/', '_') + ".mat";
                    if (File.Exists(path)) continue;
                    Shader shader = Shader.Find(shaderName);
                    if (shader == null) throw new InvalidOperationException("Missing host shader: " + shaderName);
                    AssetDatabase.CreateAsset(new Material(shader), path);
                }
                const string skyboxPath = "Assets/Resources/UnityPlayHostBuiltins/DefaultSkybox.mat";
                if (!File.Exists(skyboxPath))
                {
                    Material builtin = AssetDatabase.GetBuiltinExtraResource<Material>("Default-Skybox.mat");
                    if (builtin == null || builtin.shader == null)
                        throw new InvalidOperationException("The installed engine has no native Default-Skybox material.");
                    Directory.CreateDirectory(Path.GetDirectoryName(skyboxPath));
                    AssetDatabase.CreateAsset(new Material(builtin) { name = builtin.name }, skyboxPath);
                }
                PreparePackageProfile();
                PlayerSettings.SetScriptingBackend(NamedBuildTarget.Standalone, ScriptingImplementation.Mono2x);
                PlayerSettings.SetManagedStrippingLevel(NamedBuildTarget.Standalone, ManagedStrippingLevel.Disabled);
                PlayerSettings.stripEngineCode = false;
                PlayerSettings.runInBackground = true;
                PlayerSettings.defaultIsNativeResolution = false;
                PlayerSettings.defaultScreenWidth = 960;
                PlayerSettings.defaultScreenHeight = 540;
                PlayerSettings.fullScreenMode = FullScreenMode.Windowed;
                PlayerSettings.SetUseDefaultGraphicsAPIs(BuildTarget.StandaloneLinux64, false);
                PlayerSettings.SetGraphicsAPIs(BuildTarget.StandaloneLinux64, new[] { GraphicsDeviceType.OpenGLCore });
                Directory.CreateDirectory(Path.GetDirectoryName(output));
                BuildReport report = BuildPipeline.BuildPlayer(new BuildPlayerOptions
                {
                    scenes = new[] { scenePath }, locationPathName = output,
                    target = BuildTarget.StandaloneLinux64, targetGroup = BuildTargetGroup.Standalone,
                    subtarget = (int)StandaloneBuildSubtarget.Player, options = BuildOptions.Development,
                });
                manifest.success = report.summary.result == BuildResult.Succeeded;
                manifest.buildSeconds = report.summary.totalTime.TotalSeconds;
                manifest.bytes = report.summary.totalSize;
                manifest.failure = manifest.success ? string.Empty : report.summary.result.ToString();
                manifest.output = output;
            }
            catch (Exception error)
            {
                manifest.failure = error.ToString();
                Debug.LogException(error);
            }
            finally
            {
                if (Path.IsPathRooted(reportPath))
                {
                    Directory.CreateDirectory(Path.GetDirectoryName(reportPath));
                    File.WriteAllText(reportPath, JsonUtility.ToJson(manifest, true) + "\n");
                }
                EditorApplication.Exit(manifest.success ? 0 : 1);
            }
        }

        [Serializable] private sealed class Manifest
        {
            public int schemaVersion = 1;
            public string engineVersion;
            public bool success;
            public string output;
            public string failure;
            public double buildSeconds;
            public ulong bytes;
        }
    }
}
