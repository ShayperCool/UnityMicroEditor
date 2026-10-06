using System;
using System.IO;
using System.Text;
using UnityEditor;
using UnityEditor.Build;
using UnityEditor.Build.Reporting;
using UnityEditor.SceneManagement;
using UnityEngine;
using UnityEngine.Rendering;

namespace Dungeon.CLIPlay.Editor
{
    /// <summary>Batch entrypoint for an incremental Linux Mono development player.</summary>
    public static class PlayerBuild
    {
        private const string SmokeScene = "Assets/Dungeon/CLIPlay/GeneratedSmoke.unity";

        public static void BuildFromCommandLine()
        {
            if (!Application.isBatchMode)
                throw new InvalidOperationException("CLI player builds must run in Unity batchmode with -buildTarget Linux64.");

            var manifest = new BuildManifest
            {
                engineVersion = Application.unityVersion,
                target = BuildTarget.StandaloneLinux64.ToString(),
                scriptingBackend = ScriptingImplementation.Mono2x.ToString(),
                development = true,
                notes = "Builds the selected scene assets directly. Project build processors and existing Addressables settings/content still apply; this tool does not generate or rebuild Addressables content itself.",
            };
            string manifestPath = null;
            try
            {
                manifestPath = RequiredAbsolutePath("UNITY_CLI_PLAY_BUILD_MANIFEST");
                manifest.outputPath = RequiredAbsolutePath("UNITY_CLI_PLAY_OUTPUT");
                manifest.graphicsApi = Environment.GetEnvironmentVariable("UNITY_CLI_PLAY_GRAPHICS") ?? "glcore";
                if (manifest.graphicsApi != "glcore" && manifest.graphicsApi != "project")
                    throw new ArgumentException("UNITY_CLI_PLAY_GRAPHICS must be glcore or project.");
                bool smoke = Environment.GetEnvironmentVariable("UNITY_CLI_PLAY_SMOKE") == "1";
                manifest.scenes = smoke ? new[] { SmokeScene } : ParseScenes(Environment.GetEnvironmentVariable("UNITY_CLI_PLAY_SCENES"));
                if (!BuildPipeline.IsBuildTargetSupported(BuildTargetGroup.Standalone, BuildTarget.StandaloneLinux64))
                    throw new InvalidOperationException("Linux Standalone build support is not installed for this Unity editor.");
                if (EditorUserBuildSettings.activeBuildTarget != BuildTarget.StandaloneLinux64)
                    throw new InvalidOperationException("Start the batch editor with -buildTarget Linux64. Changing build targets inside executeMethod is not supported by this tool.");

                BuildPlayer(manifest, smoke);
            }
            catch (Exception exception)
            {
                manifest.success = false;
                manifest.failure = exception.ToString();
                Debug.LogError("[unity-cli-play-build] failed " + exception.Message);
            }

            manifest.exitCode = manifest.success ? 0 : 1;
            try
            {
                if (!string.IsNullOrEmpty(manifestPath))
                {
                    Directory.CreateDirectory(Path.GetDirectoryName(manifestPath));
                    File.WriteAllText(manifestPath, JsonUtility.ToJson(manifest, true) + "\n", new UTF8Encoding(false));
                    Debug.Log("[unity-cli-play-build] manifest-written " + manifestPath);
                }
            }
            catch (Exception exception)
            {
                manifest.exitCode = 1;
                Debug.LogError("[unity-cli-play-build] manifest-write-error " + exception.Message);
            }
            EditorApplication.Exit(manifest.exitCode);
        }

        private static void BuildPlayer(BuildManifest manifest, bool smoke)
        {
            ScriptingImplementation backend = PlayerSettings.GetScriptingBackend(NamedBuildTarget.Standalone);
            bool background = PlayerSettings.runInBackground;
            bool defaultGraphics = PlayerSettings.GetUseDefaultGraphicsAPIs(BuildTarget.StandaloneLinux64);
            GraphicsDeviceType[] graphicsApis = PlayerSettings.GetGraphicsAPIs(BuildTarget.StandaloneLinux64);
            bool smokeCreated = false;
            SceneSetup[] scenes = smoke ? EditorSceneManager.GetSceneManagerSetup() : null;
            try
            {
                if (smoke)
                {
                    if (File.Exists(SmokeScene) || AssetDatabase.LoadAssetAtPath<SceneAsset>(SmokeScene) != null)
                        throw new IOException("The temporary smoke scene already exists; refusing to overwrite it: " + SmokeScene);
                    var scene = EditorSceneManager.NewScene(NewSceneSetup.EmptyScene, NewSceneMode.Single);
                    if (!EditorSceneManager.SaveScene(scene, SmokeScene))
                        throw new IOException("Could not save the temporary smoke scene.");
                    smokeCreated = true;
                }

                for (int index = 0; index < manifest.scenes.Length; index++)
                {
                    if (AssetDatabase.LoadAssetAtPath<SceneAsset>(manifest.scenes[index]) == null)
                        throw new ArgumentException("Scene asset does not exist: " + manifest.scenes[index]);
                }
                PlayerSettings.SetScriptingBackend(NamedBuildTarget.Standalone, ScriptingImplementation.Mono2x);
                PlayerSettings.runInBackground = true;
                if (manifest.graphicsApi == "glcore")
                {
                    PlayerSettings.SetUseDefaultGraphicsAPIs(BuildTarget.StandaloneLinux64, false);
                    PlayerSettings.SetGraphicsAPIs(BuildTarget.StandaloneLinux64, new[] { GraphicsDeviceType.OpenGLCore });
                }
                Directory.CreateDirectory(Path.GetDirectoryName(manifest.outputPath));

                BuildReport report = BuildPipeline.BuildPlayer(new BuildPlayerOptions
                {
                    scenes = manifest.scenes,
                    locationPathName = manifest.outputPath,
                    target = BuildTarget.StandaloneLinux64,
                    targetGroup = BuildTargetGroup.Standalone,
                    subtarget = (int)StandaloneBuildSubtarget.Player,
                    options = BuildOptions.Development,
                    assetBundleManifestPath = Environment.GetEnvironmentVariable("UNITY_CLI_PLAY_ASSET_BUNDLE_MANIFEST") ?? string.Empty,
                });
                manifest.success = report.summary.result == BuildResult.Succeeded;
                manifest.totalSizeBytes = report.summary.totalSize;
                manifest.elapsedSeconds = report.summary.totalTime.TotalSeconds;
                manifest.buildErrors = report.summary.totalErrors;
                manifest.buildWarnings = report.summary.totalWarnings;
                if (!manifest.success) manifest.failure = "Unity build result: " + report.summary.result;
                Debug.Log("[unity-cli-play-build] result=" + report.summary.result + " output=" + manifest.outputPath);
            }
            finally
            {
                try
                {
                    PlayerSettings.SetScriptingBackend(NamedBuildTarget.Standalone, backend);
                    PlayerSettings.runInBackground = background;
                    PlayerSettings.SetGraphicsAPIs(BuildTarget.StandaloneLinux64, graphicsApis);
                    PlayerSettings.SetUseDefaultGraphicsAPIs(BuildTarget.StandaloneLinux64, defaultGraphics);
                }
                finally
                {
                    if (smokeCreated)
                    {
                        try
                        {
                            if (scenes != null && scenes.Length > 0) EditorSceneManager.RestoreSceneManagerSetup(scenes);
                        }
                        finally
                        {
                            if (!AssetDatabase.DeleteAsset(SmokeScene))
                                throw new IOException("Could not delete the temporary smoke scene: " + SmokeScene);
                        }
                    }
                }
            }
        }

        private static string RequiredAbsolutePath(string name)
        {
            string path = Environment.GetEnvironmentVariable(name);
            if (string.IsNullOrWhiteSpace(path) || !Path.IsPathRooted(path))
                throw new ArgumentException(name + " must contain an absolute file path.");
            return Path.GetFullPath(path);
        }

        private static string[] ParseScenes(string json)
        {
            if (string.IsNullOrWhiteSpace(json)) throw new ArgumentException("UNITY_CLI_PLAY_SCENES must contain a JSON array of scene asset paths.");
            SceneList list = JsonUtility.FromJson<SceneList>("{\"scenes\":" + json + "}");
            if (list == null || list.scenes == null || list.scenes.Length == 0)
                throw new ArgumentException("At least one scene is required.");
            for (int index = 0; index < list.scenes.Length; index++)
            {
                string path = list.scenes[index];
                if (string.IsNullOrWhiteSpace(path) || Path.IsPathRooted(path) || !path.EndsWith(".unity", StringComparison.Ordinal))
                    throw new ArgumentException("Each scene must be a project-relative .unity asset path.");
                list.scenes[index] = path.Replace('\\', '/');
                for (int earlier = 0; earlier < index; earlier++)
                {
                    if (list.scenes[earlier] == list.scenes[index]) throw new ArgumentException("Duplicate scene: " + list.scenes[index]);
                }
            }
            return list.scenes;
        }

        [Serializable]
        private sealed class SceneList
        {
            public string[] scenes;
        }

        [Serializable]
        private sealed class BuildManifest
        {
            public int schemaVersion = 1;
            public bool success;
            public int exitCode;
            public string engineVersion;
            public string target;
            public string scriptingBackend;
            public string graphicsApi;
            public string outputPath;
            public string[] scenes;
            public bool development;
            public ulong totalSizeBytes;
            public double elapsedSeconds;
            public int buildErrors;
            public int buildWarnings;
            public string failure = string.Empty;
            public string notes;
        }
    }
}
