// Reference capture, run by the tool's developers only, with a licensed Editor:
// writes an object of every engine component and of the engine's asset classes
// the Editor can make, with their default values, in the Player format with
// their type trees, as one file. The tool reads each class's tree and sample
// off it (`player-file engine-pack <release> <output> <file>`); a user's
// preparation never runs this.
//
//   Unity -batchmode -nographics -projectPath <empty project of the release> \
//     -executeMethod UnityMicroEditor.ReferenceCapture.ClassCapture.Run \
//     -captureOutput <directory> -quit
using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using UnityEditor;
using UnityEditor.Build.Content;
using UnityEngine;

namespace UnityMicroEditor.ReferenceCapture
{
    public static class ClassCapture
    {
        const BuildTarget Target = BuildTarget.StandaloneLinux64;
        const string Folder = "Assets/ClassCapture";

        public static void Run()
        {
            string[] args = Environment.GetCommandLineArgs();
            int at = Array.IndexOf(args, "-captureOutput");
            if (at < 0 || at + 1 >= args.Length) throw new ArgumentException("-captureOutput <directory> is required");
            string output = Path.GetFullPath(args[at + 1]);
            Directory.CreateDirectory(output);
            var log = new System.Text.StringBuilder();
            if (AssetDatabase.IsValidFolder(Folder)) AssetDatabase.DeleteAsset(Folder);
            AssetDatabase.CreateFolder("Assets", "ClassCapture");

            // One game object per component class, each in a prefab of its own.
            var components = TypeCache.GetTypesDerivedFrom<Component>()
                .Where(t => !t.IsAbstract && !t.IsGenericType && t.Namespace != null
                    && t.Namespace.StartsWith("UnityEngine") && !typeof(MonoBehaviour).IsAssignableFrom(t)
                    && t.Assembly.GetName().Name.StartsWith("UnityEngine"))
                .OrderBy(t => t.FullName, StringComparer.Ordinal)
                .ToList();
            var assets = new List<string>();
            foreach (var type in components)
            {
                try
                {
                    var go = new GameObject(type.Name);
                    if (type != typeof(Transform) && go.GetComponent(type) == null && go.AddComponent(type) == null)
                    {
                        UnityEngine.Object.DestroyImmediate(go);
                        log.Append("component refused ").Append(type.FullName).Append('\n');
                        continue;
                    }
                    string path = Folder + "/" + type.FullName + ".prefab";
                    PrefabUtility.SaveAsPrefabAsset(go, path);
                    UnityEngine.Object.DestroyImmediate(go);
                    assets.Add(path);
                }
                catch (Exception error) { log.Append("component failed ").Append(type.FullName).Append(' ').Append(error.Message).Append('\n'); }
            }

            // The engine's asset classes made with their defaults.
            var made = new List<(string, Func<UnityEngine.Object>)>
            {
                ("PhysicsMaterial2D.physicsMaterial2D", () => new PhysicsMaterial2D()),
                ("PhysicsMaterial.physicMaterial", () => new PhysicsMaterial()),
                ("Material.mat", () => new Material(Shader.Find("Hidden/InternalErrorShader"))),
                ("Mesh.asset", () => new Mesh()),
                ("Cubemap.asset", () => new Cubemap(4, TextureFormat.RGBA32, false)),
                ("Texture2DArray.asset", () => new Texture2DArray(4, 4, 2, TextureFormat.RGBA32, false)),
                ("Texture3D.asset", () => new Texture3D(4, 4, 4, TextureFormat.RGBA32, false)),
                ("CubemapArray.asset", () => new CubemapArray(4, 1, TextureFormat.RGBA32, false)),
                ("RenderTexture.renderTexture", () => new RenderTexture(4, 4, 16)),
                ("CustomRenderTexture.asset", () => new CustomRenderTexture(4, 4)),
                ("Flare.flare", () => new Flare()),
                ("LightingSettings.lighting", () => new LightingSettings()),
                ("AvatarMask.mask", () => new AvatarMask()),
                ("AnimationClip.anim", () => new AnimationClip()),
                ("ShaderVariantCollection.shadervariants", () => new ShaderVariantCollection()),
                ("TerrainData.asset", () => new TerrainData()),
                ("TerrainLayer.terrainlayer", () => new TerrainLayer()),
                ("LightingDataAsset.asset", () => null),
            };
            foreach (var (name, make) in made)
            {
                try
                {
                    var asset = make();
                    if (asset == null) continue;
                    string path = Folder + "/" + name;
                    AssetDatabase.CreateAsset(asset, path);
                    assets.Add(path);
                }
                catch (Exception error) { log.Append("asset failed ").Append(name).Append(' ').Append(error.Message).Append('\n'); }
            }
            AssetDatabase.SaveAssets();
            AssetDatabase.Refresh();

            var ids = new List<ObjectIdentifier>();
            foreach (string path in assets)
            {
                GUID guid = new GUID(AssetDatabase.AssetPathToGUID(path));
                ObjectIdentifier[] found;
                try { found = ContentBuildInterface.GetPlayerObjectIdentifiersInAsset(guid, Target); }
                catch (Exception error) { log.Append("refused ").Append(path).Append(' ').Append(error.Message).Append('\n'); continue; }
                foreach (var id in found)
                {
                    var type = ContentBuildInterface.GetTypeForObject(id);
                    log.Append("object ").Append(path).Append(' ').Append(id.localIdentifierInFile).Append(' ').Append(type == null ? "?" : type.FullName).Append('\n');
                    if (type == null || typeof(MonoBehaviour).IsAssignableFrom(type)) continue;
                    ids.Add(id);
                }
            }
            const string InternalName = "CAB-classcapture";
            var command = new WriteCommand { fileName = "classes.player", internalName = InternalName, serializeObjects = new List<SerializationInfo>() };
            var map = new BuildReferenceMap();
            long index = 1;
            foreach (var id in ids)
            {
                command.serializeObjects.Add(new SerializationInfo { serializationObject = id, serializationIndex = index });
                map.AddMapping(InternalName, index, id);
                index++;
            }
            var global = ContentBuildInterface.GetGlobalUsageFromGraphicsSettings();
            var usage = new BuildUsageTagSet();
            var all = ids.ToArray();
            ContentBuildInterface.CalculateBuildUsageTags(all, all, global, usage);
            ContentBuildInterface.WriteSerializedFile(output, new WriteParameters
            {
                writeCommand = command,
                settings = new BuildSettings { target = Target, group = BuildTargetGroup.Standalone, buildFlags = ContentBuildFlags.None },
                globalUsage = global,
                usageSet = usage,
                referenceMap = map,
            });
            log.Append("file classes.player ").Append(ids.Count).Append(" objects\n");
            log.Append("unity ").Append(Application.unityVersion).Append('\n');
            File.WriteAllText(Path.Combine(output, "capture.txt"), log.ToString());
        }
    }
}
