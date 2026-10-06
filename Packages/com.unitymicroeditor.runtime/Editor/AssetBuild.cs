using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Security.Cryptography;
using System.Text;
using UnityEditor;
using UnityEditor.Build;
using UnityEditor.Compilation;
using PackageInfo = UnityEditor.PackageManager.PackageInfo;
using UnityEngine;
using UnityEngine.Rendering;
using Debug = UnityEngine.Debug;

namespace Dungeon.CLIPlay.Editor
{
    /// <summary>Exports explicitly selected scenes without rebuilding the Player.</summary>
    public static class AssetBuild
    {
        private const string SceneBundle = "cli-play-scenes";

        public static void BuildFromCommandLine()
        {
            var result = new ContentManifest
            {
                engineVersion = Application.unityVersion,
                target = BuildTarget.StandaloneLinux64.ToString(),
                scriptingBackend = ScriptingImplementation.Mono2x.ToString(),
                development = true,
                sceneBundle = SceneBundle,
                notes = "Scene dependencies are embedded by Unity. Scripts and stripped Player types/modules cannot be added by a content-only build. Addressables/dynamically located assets are not rebuilt.",
            };
            string manifestPath = null;
            var watch = Stopwatch.StartNew();
            try
            {
                if (!Application.isBatchMode)
                    throw new InvalidOperationException("Asset export requires Unity batchmode.");
                if (EditorUserBuildSettings.activeBuildTarget != BuildTarget.StandaloneLinux64)
                    throw new InvalidOperationException("Start the Editor with -buildTarget Linux64.");
                BenchmarkAssets.ApplyRevisionFromEnvironment();
                string output = CachePath("UNITY_CLI_CONTENT_OUTPUT");
                manifestPath = CachePath("UNITY_CLI_CONTENT_MANIFEST");
                result.graphicsApi = Environment.GetEnvironmentVariable("UNITY_CLI_CONTENT_GRAPHICS") ?? "glcore";
                if (result.graphicsApi != "glcore" && result.graphicsApi != "project")
                    throw new ArgumentException("Content graphics API must be glcore or project.");
                result.scenes = ParseScenes(Environment.GetEnvironmentVariable("UNITY_CLI_CONTENT_SCENES"));
                foreach (string scene in result.scenes)
                {
                    if (AssetDatabase.LoadAssetAtPath<SceneAsset>(scene) == null)
                        throw new ArgumentException("Scene asset does not exist: " + scene);
                }
                Build(result, output);
                result.success = true;
            }
            catch (Exception exception)
            {
                result.failure = exception.ToString();
                Debug.LogError("[unity-cli-content] build-failed " + exception.Message);
            }
            result.elapsedSeconds = watch.Elapsed.TotalSeconds;
            try
            {
                if (manifestPath != null)
                {
                    Directory.CreateDirectory(Path.GetDirectoryName(manifestPath));
                    File.WriteAllText(manifestPath, JsonUtility.ToJson(result, true) + "\n", new UTF8Encoding(false));
                }
            }
            catch (Exception exception)
            {
                result.success = false;
                Debug.LogError("[unity-cli-content] manifest-write-failed " + exception.Message);
            }
            EditorApplication.Exit(result.success ? 0 : 1);
        }

        private static void Build(ContentManifest result, string output)
        {
            bool defaultGraphics = PlayerSettings.GetUseDefaultGraphicsAPIs(BuildTarget.StandaloneLinux64);
            GraphicsDeviceType[] previousApis = PlayerSettings.GetGraphicsAPIs(BuildTarget.StandaloneLinux64);
            ScriptingImplementation previousBackend = PlayerSettings.GetScriptingBackend(NamedBuildTarget.Standalone);
            try
            {
                PlayerSettings.SetScriptingBackend(NamedBuildTarget.Standalone, ScriptingImplementation.Mono2x);
                if (result.graphicsApi == "glcore")
                {
                    PlayerSettings.SetUseDefaultGraphicsAPIs(BuildTarget.StandaloneLinux64, false);
                    PlayerSettings.SetGraphicsAPIs(BuildTarget.StandaloneLinux64, new[] { GraphicsDeviceType.OpenGLCore });
                }
                Directory.CreateDirectory(output);
                // Stable output directory and no ForceRebuild flag preserve Unity's
                // incremental bundle/ShaderCache artifacts between exports.
                AssetBundleManifest built = BuildPipeline.BuildAssetBundles(new BuildAssetBundlesParameters
                {
                    outputPath = output,
                    targetPlatform = BuildTarget.StandaloneLinux64,
                    subtarget = (int)StandaloneBuildSubtarget.Player,
                    extraScriptingDefines = new[] { "DEVELOPMENT_BUILD", "UNITY_ASSERTIONS", "ENABLE_PROFILER" },
                    options = BuildAssetBundleOptions.ChunkBasedCompression | BuildAssetBundleOptions.StrictMode,
                    bundleDefinitions = new[]
                    {
                        new AssetBundleBuild { assetBundleName = SceneBundle, assetNames = result.scenes },
                    },
                });
                if (built == null) throw new InvalidOperationException("Unity returned no AssetBundle manifest; inspect the build log.");
                string[] names = built.GetAllAssetBundles();
                Array.Sort(names, StringComparer.Ordinal);
                result.bundles = new BundleRecord[names.Length];
                for (int index = 0; index < names.Length; index++)
                {
                    string file = Path.Combine(output, names[index]);
                    if (!BuildPipeline.GetCRCForAssetBundle(file, out uint crc))
                        throw new IOException("Could not obtain bundle CRC: " + file);
                    string[] dependencies = built.GetAllDependencies(names[index]);
                    Array.Sort(dependencies, StringComparer.Ordinal);
                    result.bundles[index] = new BundleRecord
                    {
                        name = names[index], file = names[index], sha256 = Sha256(file), crc = crc,
                        bytes = new FileInfo(file).Length, dependencies = dependencies,
                    };
                }
                string root = Path.Combine(output, new DirectoryInfo(output).Name + ".manifest");
                if (!File.Exists(root)) throw new IOException("Unity did not write the root .manifest for type preservation.");
                result.typePreservationManifest = root;
                string[] paths = AssetDatabase.GetDependencies(result.scenes, true);
                Array.Sort(paths, StringComparer.Ordinal);
                var dependenciesList = new List<SourceRecord>(paths.Length);
                var assemblies = new SortedSet<string>(StringComparer.Ordinal);
                var playerAssemblies = new HashSet<string>(StringComparer.Ordinal);
                foreach (UnityEditor.Compilation.Assembly assembly in CompilationPipeline.GetAssemblies(AssembliesType.Player))
                    playerAssemblies.Add(assembly.name);
                foreach (string path in paths)
                {
                    string physicalPath = ResolveDependency(path);
                    if (!File.Exists(physicalPath)) continue; // Built-in Unity resources have no project file.
                    dependenciesList.Add(new SourceRecord
                    {
                        path = path,
                        physicalPath = path.StartsWith("Packages/", StringComparison.Ordinal) ? physicalPath : null,
                        sha256 = Sha256(physicalPath),
                    });
                    if (!path.EndsWith(".cs", StringComparison.Ordinal)) continue;
                    MonoScript script = AssetDatabase.LoadAssetAtPath<MonoScript>(path);
                    if (script == null) continue;
                    Type type = script.GetClass();
                    // Shader/importer dependencies can include Editor scripts.
                    // Only the actual Player compilation graph defines runtime DLLs.
                    if (type != null && playerAssemblies.Contains(type.Assembly.GetName().Name))
                        assemblies.Add(type.Assembly.GetName().Name);
                }
                result.sourceDependencies = dependenciesList.ToArray();
                result.requiredAssemblies = new string[assemblies.Count];
                assemblies.CopyTo(result.requiredAssemblies);
            }
            finally
            {
                PlayerSettings.SetGraphicsAPIs(BuildTarget.StandaloneLinux64, previousApis);
                PlayerSettings.SetUseDefaultGraphicsAPIs(BuildTarget.StandaloneLinux64, defaultGraphics);
                PlayerSettings.SetScriptingBackend(NamedBuildTarget.Standalone, previousBackend);
            }
        }

        private static string ResolveDependency(string path)
        {
            if (!path.StartsWith("Packages/", StringComparison.Ordinal)) return path;
            PackageInfo package = PackageInfo.FindForAssetPath(path);
            if (package == null || string.IsNullOrWhiteSpace(package.resolvedPath))
                throw new IOException("Could not resolve package dependency: " + path);
            string prefix = "Packages/" + package.name + "/";
            if (!path.StartsWith(prefix, StringComparison.Ordinal))
                throw new IOException("Package dependency does not match its package: " + path);
            string project = Path.GetFullPath(".") + Path.DirectorySeparatorChar;
            string root = Path.GetFullPath(package.resolvedPath);
            string embedded = Path.GetFullPath("Packages/" + package.name);
            string cache = Path.GetFullPath("Library/PackageCache") + Path.DirectorySeparatorChar;
            if (root != embedded && (!root.StartsWith(cache, StringComparison.Ordinal)
                || !new DirectoryInfo(root).Name.StartsWith(package.name + "@", StringComparison.Ordinal)))
                throw new IOException("Package dependencies must be embedded or in this project's PackageCache: " + path);
            string physical = Path.GetFullPath(Path.Combine(root, path.Substring(prefix.Length)));
            if (!physical.StartsWith(root + Path.DirectorySeparatorChar, StringComparison.Ordinal)
                || !physical.StartsWith(project, StringComparison.Ordinal))
                throw new IOException("Package dependency escapes its package/project: " + path);
            return physical.Substring(project.Length).Replace(Path.DirectorySeparatorChar, '/');
        }

        private static string CachePath(string variable)
        {
            string configured = Environment.GetEnvironmentVariable(variable);
            if (string.IsNullOrWhiteSpace(configured) || !Path.IsPathRooted(configured))
                throw new ArgumentException(variable + " must be an absolute path.");
            string path = Path.GetFullPath(configured);
            string cache = Path.GetFullPath("Builds/CLIPlay/.asset-cache") + Path.DirectorySeparatorChar;
            if (!path.StartsWith(cache, StringComparison.Ordinal))
                throw new ArgumentException("Asset export writes only under " + cache);
            return path;
        }

        private static string[] ParseScenes(string json)
        {
            if (string.IsNullOrWhiteSpace(json)) throw new ArgumentException("No content scenes supplied.");
            SceneList list = JsonUtility.FromJson<SceneList>("{\"scenes\":" + json + "}");
            if (list == null || list.scenes == null || list.scenes.Length == 0)
                throw new ArgumentException("At least one content scene is required.");
            string assets = Path.GetFullPath("Assets") + Path.DirectorySeparatorChar;
            var seen = new HashSet<string>(StringComparer.Ordinal);
            foreach (string path in list.scenes)
            {
                if (string.IsNullOrWhiteSpace(path) || Path.IsPathRooted(path) || !path.EndsWith(".unity", StringComparison.Ordinal)
                    || !Path.GetFullPath(path).StartsWith(assets, StringComparison.Ordinal) || !seen.Add(path))
                    throw new ArgumentException("Content scenes must be distinct project Assets/*.unity paths.");
            }
            return list.scenes;
        }

        private static string Sha256(string path)
        {
            using (var algorithm = SHA256.Create())
            using (var stream = File.OpenRead(path))
                return BitConverter.ToString(algorithm.ComputeHash(stream)).Replace("-", string.Empty).ToLowerInvariant();
        }

        [Serializable] private sealed class SceneList { public string[] scenes; }
        [Serializable] private sealed class BundleRecord
        {
            public string name;
            public string file;
            public string sha256;
            public uint crc;
            public long bytes;
            public string[] dependencies;
        }
        [Serializable] private sealed class SourceRecord { public string path; public string physicalPath; public string sha256; }
        [Serializable] private sealed class ContentManifest
        {
            public int schemaVersion = 1;
            public bool success;
            public string engineVersion;
            public string target;
            public string scriptingBackend;
            public bool development;
            public string graphicsApi;
            public string[] scenes;
            public string sceneBundle;
            public BundleRecord[] bundles;
            public SourceRecord[] sourceDependencies;
            public string[] requiredAssemblies;
            public string typePreservationManifest;
            public double elapsedSeconds;
            public string failure = string.Empty;
            public string notes;
        }
    }
}
