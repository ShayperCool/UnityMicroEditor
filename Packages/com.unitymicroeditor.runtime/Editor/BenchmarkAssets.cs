using System;
using System.IO;
using UnityEditor;
using UnityEditor.SceneManagement;
using UnityEngine;

namespace Dungeon.CLIPlay.Editor
{
    /// <summary>Creates missing benchmark assets once. Existing content is never reset.</summary>
    public static class BenchmarkAssets
    {
        public const string Folder = "Assets/Dungeon/CLIPlay/Benchmarks";
        public const string ShipScene = Folder + "/ShipGenBenchmark.unity";
        public const string AssetScene = Folder + "/AssetIterationBenchmark.unity";

        public static void CreateFromCommandLine()
        {
            try
            {
                CreateMissing();
                EditorApplication.Exit(0);
            }
            catch (Exception exception)
            {
                Debug.LogException(exception);
                EditorApplication.Exit(1);
            }
        }

        // Invoked only by an explicit benchmark environment variable. Normal exports
        // never alter assets. This intentionally edits and saves the test assets in Editor.
        public static void ApplyRevisionFromEnvironment()
        {
            string value = Environment.GetEnvironmentVariable("UNITY_CLI_CONTENT_BENCHMARK_REVISION");
            if (string.IsNullOrEmpty(value)) return;
            if (!int.TryParse(value, out int revision) || revision < 1)
                throw new ArgumentException("Benchmark revision must be positive.");
            long editedAt = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
            var material = AssetDatabase.LoadAssetAtPath<Material>(Folder + "/Probe.mat");
            var data = AssetDatabase.LoadAssetAtPath<AssetIterationData>(Folder + "/Probe.asset");
            if (material == null || data == null || !File.Exists(AssetScene))
                throw new InvalidOperationException("Create benchmark assets before running the asset benchmark.");
            Color color = revision == 1 ? new Color(0.1f, 0.6f, 1f, 1f) :
                revision % 2 == 0 ? new Color(0.9f, 0.15f, 0.1f, 1f) : new Color(0.2f, 0.9f, 0.3f, 1f);
            material.SetColor("_BaseColor", color);
            EditorUtility.SetDirty(material);
            data.revision = revision;
            EditorUtility.SetDirty(data);
            Color pixel = revision == 1 ? Color.white : revision % 2 == 0 ?
                new Color(1f, 0.75f, 0.5f, 1f) : new Color(0.5f, 0.25f, 1f, 1f);
            var generated = new Texture2D(2, 2, TextureFormat.RGBA32, false);
            generated.SetPixels(new[] {pixel, pixel, pixel, pixel});
            generated.Apply();
            File.WriteAllBytes(Folder + "/Probe.png", generated.EncodeToPNG());
            UnityEngine.Object.DestroyImmediate(generated);
            AssetDatabase.ImportAsset(Folder + "/Probe.png", ImportAssetOptions.ForceUpdate);
            string path = Folder + "/Probe.prefab";
            var prefab = PrefabUtility.LoadPrefabContents(path);
            try
            {
                prefab.transform.localScale = Vector3.one * (revision == 1 ? 1f : 1f + revision * 0.05f);
                PrefabUtility.SaveAsPrefabAsset(prefab, path);
            }
            finally { PrefabUtility.UnloadPrefabContents(prefab); }
            AssetDatabase.SaveAssets();
            var scene = EditorSceneManager.OpenScene(AssetScene, OpenSceneMode.Single);
            // Scene switches unload unused native assets; stack references do not
            // retain an unreferenced ScriptableObject. Resolve it after the switch.
            data = AssetDatabase.LoadAssetAtPath<AssetIterationData>(Folder + "/Probe.asset");
            material = AssetDatabase.LoadAssetAtPath<Material>(Folder + "/Probe.mat");
            Camera sceneCamera = null;
            foreach (var root in scene.GetRootGameObjects())
            foreach (var camera in root.GetComponentsInChildren<Camera>(true))
            {
                camera.backgroundColor = revision == 1 ? new Color(0.03f, 0.04f, 0.06f) : revision % 2 == 0 ?
                    new Color(0.08f, 0.04f, 0.03f) : new Color(0.03f, 0.08f, 0.04f);
                sceneCamera = camera;
            }
            foreach (var root in scene.GetRootGameObjects())
            foreach (var probe in root.GetComponentsInChildren<AssetIterationProbe>(true))
            {
                var serialized = new SerializedObject(probe);
                serialized.FindProperty("sceneCamera").objectReferenceValue = sceneCamera;
                serialized.FindProperty("material").objectReferenceValue = material;
                serialized.FindProperty("texture").objectReferenceValue = AssetDatabase.LoadAssetAtPath<Texture2D>(Folder + "/Probe.png");
                serialized.FindProperty("prefab").objectReferenceValue = AssetDatabase.LoadAssetAtPath<GameObject>(Folder + "/Probe.prefab");
                serialized.FindProperty("data").objectReferenceValue = data;
                serialized.ApplyModifiedPropertiesWithoutUndo();
            }
            if (!EditorSceneManager.SaveScene(scene, AssetScene)) throw new IOException("Could not save the benchmark revision.");
            AssetDatabase.SaveAssets();
            Debug.Log("[unity-cli-play] asset-edit-ready {\"revision\":" + revision +
                ",\"editUtcUnixMilliseconds\":" + editedAt + "}");
        }

        public static void CreateMissing()
        {
            Directory.CreateDirectory(Folder);
            AssetDatabase.Refresh();
            if (!File.Exists(ShipScene))
            {
                if (!AssetDatabase.CopyAsset("Assets/Dungeon/ShipGen/Scenes/ShipGenDemo.unity", ShipScene))
                    throw new IOException("Could not copy the benchmark ship scene.");
                var ship = EditorSceneManager.OpenScene(ShipScene, OpenSceneMode.Single);
                bool seeded = false;
                foreach (var root in ship.GetRootGameObjects())
                foreach (var behaviour in root.GetComponentsInChildren<MonoBehaviour>(true))
                {
                    if (behaviour == null || behaviour.GetType().FullName != "Dungeon.ShipGen.ShipBootstrap") continue;
                    var serialized = new SerializedObject(behaviour);
                    serialized.FindProperty("shipSeed").intValue = 4242;
                    serialized.FindProperty("scenarioSeed").intValue = 1717;
                    serialized.FindProperty("archetype").intValue = 0;
                    serialized.ApplyModifiedPropertiesWithoutUndo();
                    seeded = true;
                }
                if (!seeded) throw new InvalidOperationException("ShipBootstrap was not found in the copied scene.");
                if (!EditorSceneManager.SaveScene(ship, ShipScene)) throw new IOException("Could not save the seeded scene.");
            }

            string materialPath = Folder + "/Probe.mat";
            string texturePath = Folder + "/Probe.png";
            string prefabPath = Folder + "/Probe.prefab";
            string dataPath = Folder + "/Probe.asset";
            var material = AssetDatabase.LoadAssetAtPath<Material>(materialPath);
            bool materialCreated = false;
            if (material == null)
            {
                if (File.Exists(materialPath)) throw new IOException("Existing material could not be loaded.");
                Shader shader = Shader.Find("Universal Render Pipeline/Unlit");
                if (shader == null) throw new InvalidOperationException("URP Unlit shader is missing.");
                material = new Material(shader);
                material.SetColor("_BaseColor", new Color(0.1f, 0.6f, 1f, 1f));
                AssetDatabase.CreateAsset(material, materialPath);
                materialCreated = true;
            }
            if (!File.Exists(texturePath))
            {
                var generated = new Texture2D(2, 2, TextureFormat.RGBA32, false);
                generated.SetPixels(new[] {Color.white, Color.white, Color.white, Color.white});
                generated.Apply();
                File.WriteAllBytes(texturePath, generated.EncodeToPNG());
                UnityEngine.Object.DestroyImmediate(generated);
                AssetDatabase.ImportAsset(texturePath);
                var importer = (TextureImporter)AssetImporter.GetAtPath(texturePath);
                importer.isReadable = true;
                importer.textureCompression = TextureImporterCompression.Uncompressed;
                importer.SaveAndReimport();
            }
            var texture = AssetDatabase.LoadAssetAtPath<Texture2D>(texturePath);
            if (texture == null) throw new IOException("Existing benchmark texture could not be loaded.");
            if (!material.HasProperty("_BaseMap")) throw new InvalidOperationException("Benchmark material requires _BaseMap.");
            // Assign only at creation; hand-edited material settings remain authoritative.
            if (materialCreated)
            {
                material.SetTexture("_BaseMap", texture);
                EditorUtility.SetDirty(material);
            }
            var prefab = AssetDatabase.LoadAssetAtPath<GameObject>(prefabPath);
            if (prefab == null)
            {
                if (File.Exists(prefabPath)) throw new IOException("Existing prefab could not be loaded.");
                var cube = GameObject.CreatePrimitive(PrimitiveType.Cube);
                cube.name = "Asset iteration cube";
                cube.GetComponent<Renderer>().sharedMaterial = material;
                prefab = PrefabUtility.SaveAsPrefabAsset(cube, prefabPath);
                UnityEngine.Object.DestroyImmediate(cube);
            }
            var data = AssetDatabase.LoadAssetAtPath<AssetIterationData>(dataPath);
            if (data == null)
            {
                if (File.Exists(dataPath)) throw new IOException("Existing benchmark data could not be loaded.");
                data = ScriptableObject.CreateInstance<AssetIterationData>();
                AssetDatabase.CreateAsset(data, dataPath);
            }
            if (!File.Exists(AssetScene))
            {
                AssetDatabase.SaveAssets();
                var scene = EditorSceneManager.NewScene(NewSceneSetup.EmptyScene, NewSceneMode.Single);
                data = AssetDatabase.LoadAssetAtPath<AssetIterationData>(dataPath);
                material = AssetDatabase.LoadAssetAtPath<Material>(materialPath);
                texture = AssetDatabase.LoadAssetAtPath<Texture2D>(texturePath);
                prefab = AssetDatabase.LoadAssetAtPath<GameObject>(prefabPath);
                var cameraObject = new GameObject("Benchmark camera", typeof(Camera));
                var camera = cameraObject.GetComponent<Camera>();
                camera.transform.position = new Vector3(0f, 0f, -4f);
                camera.clearFlags = CameraClearFlags.SolidColor;
                camera.backgroundColor = new Color(0.03f, 0.04f, 0.06f);
                cameraObject.tag = "MainCamera";
                PrefabUtility.InstantiatePrefab(prefab, scene);
                var probeObject = new GameObject("Serialized asset proof", typeof(AssetIterationProbe));
                var probe = new SerializedObject(probeObject.GetComponent<AssetIterationProbe>());
                probe.FindProperty("material").objectReferenceValue = material;
                probe.FindProperty("texture").objectReferenceValue = texture;
                probe.FindProperty("prefab").objectReferenceValue = prefab;
                probe.FindProperty("data").objectReferenceValue = data;
                probe.FindProperty("sceneCamera").objectReferenceValue = camera;
                probe.ApplyModifiedPropertiesWithoutUndo();
                if (!EditorSceneManager.SaveScene(scene, AssetScene)) throw new IOException("Could not save the asset benchmark scene.");
            }
            AssetDatabase.SaveAssets();
            Debug.Log("[unity-cli-play] benchmark-assets-ready");
        }
    }
}
