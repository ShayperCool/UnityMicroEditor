using System;
using System.Collections.Generic;
using System.IO;
using System.Security.Cryptography;
using UnityEngine;
using UnityEngine.SceneManagement;

namespace Dungeon.CLIPlay
{
    /// <summary>Loads an immutable, verified Linux scene-content revision at startup.</summary>
    public sealed class CliPlayContent : IDisposable
    {
        private readonly AssetBundle[] loaded;
        private bool disposed;

        public string ScenePath { get; }
        public string Revision { get; }
        public string ManifestPath { get; }

        private CliPlayContent(string scenePath, string revision, string manifestPath, AssetBundle[] bundles)
        {
            ScenePath = scenePath;
            Revision = revision;
            ManifestPath = manifestPath;
            loaded = bundles;
        }

        public static CliPlayContent Open(string manifestPath, string sceneSelector)
        {
            if (Application.platform != RuntimePlatform.LinuxPlayer)
                throw new InvalidOperationException("External CLI content supports Linux Players only.");
            if (string.IsNullOrWhiteSpace(manifestPath) || !Path.IsPathRooted(manifestPath))
                throw new ArgumentException("Content manifest must be an absolute file path.");
            string resolved = Path.GetFullPath(manifestPath);
            string json = File.ReadAllText(resolved);
            ContentPointer pointer = JsonUtility.FromJson<ContentPointer>(json);
            if (pointer != null && !string.IsNullOrEmpty(pointer.manifest))
            {
                if (pointer.schemaVersion != 1 || string.IsNullOrEmpty(pointer.revision))
                    throw new InvalidDataException("Unsupported content pointer schema/revision.");
                resolved = ContainedPath(Path.GetDirectoryName(resolved), pointer.manifest);
                VerifyFile(resolved, pointer.manifestSha256, -1);
                json = File.ReadAllText(resolved);
            }
            ContentManifest manifest = JsonUtility.FromJson<ContentManifest>(json);
            if (manifest == null || manifest.schemaVersion != 1 || !manifest.success || !manifest.ready
                || string.IsNullOrEmpty(manifest.revision))
                throw new InvalidDataException("Content revision has not been successfully published.");
            if (pointer != null && !string.IsNullOrEmpty(pointer.manifest) && pointer.revision != manifest.revision)
                throw new InvalidDataException("Content pointer/revision mismatch.");
            if (manifest.target != "StandaloneLinux64" || manifest.engineVersion != Application.unityVersion)
                throw new InvalidDataException("Content must match this Linux Player's exact Unity version.");
            if (manifest.bundles == null || manifest.bundles.Length == 0 || manifest.scenes == null || manifest.scenes.Length == 0)
                throw new InvalidDataException("Content revision contains no bundles/scenes.");
            string scenePath = SelectScene(manifest.scenes, sceneSelector);
            string folder = Path.GetDirectoryName(resolved);
            var records = new Dictionary<string, BundleRecord>(StringComparer.Ordinal);
            foreach (BundleRecord record in manifest.bundles)
            {
                if (record == null || string.IsNullOrWhiteSpace(record.name) || records.ContainsKey(record.name))
                    throw new InvalidDataException("Bundle names must be nonempty and distinct.");
                string file = ContainedPath(folder, record.file);
                VerifyFile(file, record.sha256, record.bytes);
                records.Add(record.name, record);
            }
            if (!records.ContainsKey(manifest.sceneBundle)) throw new InvalidDataException("Scene bundle is missing.");
            var bundles = new Dictionary<string, AssetBundle>(StringComparer.Ordinal);
            var stack = new HashSet<string>(StringComparer.Ordinal);
            var ordered = new List<AssetBundle>(records.Count);
            try
            {
                LoadBundle(manifest.sceneBundle, folder, records, bundles, stack, ordered);
                AssetBundle sceneBundle = bundles[manifest.sceneBundle];
                if (!sceneBundle.isStreamedSceneAssetBundle)
                    throw new InvalidDataException("Published scene bundle contains no streamed scenes.");
                string actualPath = null;
                foreach (string candidate in sceneBundle.GetAllScenePaths())
                {
                    if (string.Equals(candidate, scenePath, StringComparison.OrdinalIgnoreCase)) actualPath = candidate;
                }
                if (actualPath == null) throw new InvalidDataException("Requested scene is absent from the verified bundle: " + scenePath);
                return new CliPlayContent(actualPath, manifest.revision, resolved, ordered.ToArray());
            }
            catch
            {
                for (int index = ordered.Count - 1; index >= 0; index--)
                    if (ordered[index] != null) ordered[index].Unload(true);
                throw;
            }
        }

        public AsyncOperation LoadScene()
        {
            if (disposed) throw new ObjectDisposedException(nameof(CliPlayContent));
            AsyncOperation operation = SceneManager.LoadSceneAsync(ScenePath, LoadSceneMode.Single);
            if (operation == null) throw new InvalidOperationException("Unity did not start the external scene load.");
            return operation;
        }

        public bool Matches(Scene scene)
        {
            return scene.IsValid() && scene.isLoaded && string.Equals(scene.path, ScenePath, StringComparison.OrdinalIgnoreCase);
        }

        public void Dispose()
        {
            if (disposed) return;
            disposed = true;
            // The active scene owns its instantiated objects until it unloads.
            // Unload(false) releases file handles without destroying those objects.
            for (int index = loaded.Length - 1; index >= 0; index--)
                if (loaded[index] != null) loaded[index].Unload(false);
        }

        private static void LoadBundle(string name, string folder, Dictionary<string, BundleRecord> records,
            Dictionary<string, AssetBundle> bundles, HashSet<string> stack, List<AssetBundle> ordered)
        {
            if (bundles.ContainsKey(name)) return;
            if (!records.TryGetValue(name, out BundleRecord record)) throw new InvalidDataException("Bundle dependency is absent: " + name);
            if (!stack.Add(name)) throw new InvalidDataException("Cyclic bundle dependency: " + name);
            if (record.dependencies != null)
                foreach (string dependency in record.dependencies) LoadBundle(dependency, folder, records, bundles, stack, ordered);
            AssetBundle bundle = AssetBundle.LoadFromFile(ContainedPath(folder, record.file), record.crc);
            if (bundle == null) throw new InvalidDataException("Unity could not load the verified bundle: " + name);
            bundles.Add(name, bundle);
            ordered.Add(bundle);
            stack.Remove(name);
        }

        private static string SelectScene(string[] scenes, string selector)
        {
            if (string.IsNullOrEmpty(selector))
            {
                if (scenes.Length != 1) throw new ArgumentException("Select a scene when the content revision contains several scenes.");
                return scenes[0];
            }
            string match = null;
            foreach (string scene in scenes)
            {
                if (!string.Equals(scene, selector, StringComparison.Ordinal)
                    && !string.Equals(Path.GetFileNameWithoutExtension(scene), selector, StringComparison.Ordinal)) continue;
                if (match != null) throw new ArgumentException("External scene name is ambiguous; use its asset path.");
                match = scene;
            }
            if (match == null) throw new ArgumentException("Scene is not in the content revision: " + selector);
            return match;
        }

        private static string ContainedPath(string folder, string relative)
        {
            if (string.IsNullOrWhiteSpace(relative) || Path.IsPathRooted(relative))
                throw new InvalidDataException("Content files must use relative paths.");
            string root = Path.GetFullPath(folder) + Path.DirectorySeparatorChar;
            string full = Path.GetFullPath(Path.Combine(folder, relative));
            if (!full.StartsWith(root, StringComparison.Ordinal)) throw new InvalidDataException("Content path escapes its revision directory.");
            return full;
        }

        private static void VerifyFile(string path, string expected, long bytes)
        {
            if (string.IsNullOrEmpty(expected) || expected.Length != 64 || !File.Exists(path)
                || bytes >= 0 && new FileInfo(path).Length != bytes)
                throw new InvalidDataException("Content file is missing or has the wrong size: " + path);
            using (var algorithm = SHA256.Create())
            using (var stream = File.OpenRead(path))
            {
                string actual = BitConverter.ToString(algorithm.ComputeHash(stream)).Replace("-", string.Empty).ToLowerInvariant();
                if (!string.Equals(actual, expected, StringComparison.Ordinal))
                    throw new InvalidDataException("Content SHA256 mismatch: " + path);
            }
        }

        [Serializable] private sealed class ContentPointer
        {
            public int schemaVersion;
            public string revision;
            public string manifest;
            public string manifestSha256;
        }
        [Serializable] private sealed class ContentManifest
        {
            public int schemaVersion;
            public bool success;
            public bool ready;
            public string revision;
            public string engineVersion;
            public string target;
            public string[] scenes;
            public string sceneBundle;
            public BundleRecord[] bundles;
        }
        [Serializable] private sealed class BundleRecord
        {
            public string name;
            public string file;
            public string sha256;
            public uint crc;
            public long bytes;
            public string[] dependencies;
        }
    }
}
