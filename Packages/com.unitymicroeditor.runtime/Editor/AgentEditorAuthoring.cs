using System;
using System.Collections.Generic;
using System.IO;
using UnityEditor;
using UnityEditor.SceneManagement;
using UnityEngine;
using UnityEngine.SceneManagement;

namespace Dungeon.CLIPlay.Editor
{
    /// <summary>Capture actual Unity defaults for a supported native component.</summary>
    public static class AgentEditorAuthoring
    {
        private static readonly Dictionary<string, Type> types = new Dictionary<string, Type>(StringComparer.Ordinal)
        {
            ["UnityEngine.Camera"] = typeof(Camera), ["UnityEngine.Light"] = typeof(Light),
            ["UnityEngine.MeshRenderer"] = typeof(MeshRenderer), ["UnityEngine.MeshFilter"] = typeof(MeshFilter),
            ["UnityEngine.Rigidbody"] = typeof(Rigidbody), ["UnityEngine.BoxCollider"] = typeof(BoxCollider),
            ["UnityEngine.SphereCollider"] = typeof(SphereCollider), ["UnityEngine.CapsuleCollider"] = typeof(CapsuleCollider),
            ["UnityEngine.MeshCollider"] = typeof(MeshCollider), ["UnityEngine.AudioListener"] = typeof(AudioListener),
            ["UnityEngine.AudioSource"] = typeof(AudioSource), ["UnityEngine.Animator"] = typeof(Animator),
            ["UnityEngine.LineRenderer"] = typeof(LineRenderer), ["UnityEngine.SkinnedMeshRenderer"] = typeof(SkinnedMeshRenderer),
            ["UnityEngine.CharacterController"] = typeof(CharacterController), ["UnityEngine.ParticleSystem"] = typeof(ParticleSystem),
            ["UnityEngine.ParticleSystemRenderer"] = typeof(ParticleSystemRenderer), ["UnityEngine.LODGroup"] = typeof(LODGroup),
            ["UnityEngine.ReflectionProbe"] = typeof(ReflectionProbe), ["UnityEngine.CanvasRenderer"] = typeof(CanvasRenderer),
            ["UnityEngine.Canvas"] = typeof(Canvas), ["UnityEngine.CanvasGroup"] = typeof(CanvasGroup)
        };

        internal static Type ResolveBuiltin(string name)
        {
            if (name != null && types.TryGetValue(name.StartsWith("UnityEngine.", StringComparison.Ordinal) ? name : "UnityEngine." + name, out Type type)) return type;
            throw new ArgumentException("Component type is outside the supported native profile.");
        }

        public static Dictionary<string, object> Execute(Dictionary<string, object> arguments, string mailbox, string requestId)
        {
            if (EditorApplication.isPlayingOrWillChangePlaymode || EditorApplication.isCompiling)
                throw new InvalidOperationException("Capture component defaults in idle Edit Mode.");
            if (!arguments.TryGetValue("componentType", out object value) || !(value is string name))
                throw new ArgumentException("Only supported named native component types can be captured.");
            Type type = ResolveBuiltin(name);
            for (int index = 0; index < SceneManager.sceneCount; index++)
            {
                Scene existing = SceneManager.GetSceneAt(index);
                if (existing.isDirty || (existing.path.Length == 0 && existing.rootCount != 0)) throw new InvalidOperationException("Component capture refuses dirty or unsaved loaded scenes.");
            }
            string folder = "Assets/UnityMicroEditorGenerated";
            if (AssetDatabase.IsValidFolder(folder) || Directory.Exists(folder) || File.Exists(folder + ".meta"))
                throw new IOException("Owned schema scratch directory already exists; refusing to use authored content.");
            Scene active = SceneManager.GetActiveScene();
            bool emptyStartup = SceneManager.sceneCount == 1 && active.IsValid() && active.path.Length == 0 && active.rootCount == 0;
            Scene preview = EditorSceneManager.NewScene(NewSceneSetup.EmptyScene, emptyStartup ? NewSceneMode.Single : NewSceneMode.Additive);
            try
            {
                var owner = new GameObject("ComponentTemplate");
                owner.SetActive(false);
                SceneManager.MoveGameObjectToScene(owner, preview);
                // The renderer is owned by ParticleSystem and cannot be added
                // independently on every pinned Editor profile. Capture the
                // public required-component closure instead.
                Component component;
                if (type == typeof(ParticleSystemRenderer))
                {
                    owner.AddComponent<ParticleSystem>();
                    component = owner.GetComponent<ParticleSystemRenderer>();
                }
                else component = owner.AddComponent(type);
                if (component == null) throw new InvalidOperationException("Component creation failed.");
                if (AssetDatabase.CreateFolder("Assets", "UnityMicroEditorGenerated").Length == 0) throw new IOException("Cannot create exclusive schema scratch.");
                string source = folder + "/" + requestId + ".unity";
                if (!EditorSceneManager.SaveScene(preview, source)) throw new IOException("Could not capture component template source.");
                string output = Path.Combine(mailbox, "schema-" + requestId + ".unity");
                if (File.Exists(output)) throw new IOException("Schema result already exists.");
                File.WriteAllBytes(output, File.ReadAllBytes(source));
                return new Dictionary<string, object> { ["backend"] = "unity_editor", ["componentType"] = name,
                    ["templatePath"] = output, ["unityVersion"] = Application.unityVersion,
                    ["serializedJson"] = EditorJsonUtility.ToJson(component) };
            }
            finally
            {
                EditorSceneManager.CloseScene(preview, true);
                if (emptyStartup) EditorSceneManager.NewScene(NewSceneSetup.EmptyScene, NewSceneMode.Single);
                else if (active.IsValid() && active.isLoaded) SceneManager.SetActiveScene(active);
                if (AssetDatabase.IsValidFolder(folder) && !AssetDatabase.DeleteAsset(folder)) throw new IOException("Cannot remove owned schema scratch.");
                AssetDatabase.Refresh(ImportAssetOptions.ForceSynchronousImport);
            }
        }
    }
}
