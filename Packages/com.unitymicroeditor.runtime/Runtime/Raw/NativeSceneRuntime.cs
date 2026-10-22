using System;
using System.Collections.Generic;
using System.IO;
using Unity.Collections;
using Unity.Content;
using Unity.Loading;
using UnityEngine;
using UnityEngine.SceneManagement;
using Object = UnityEngine.Object;

namespace UnityMicroEditor.Raw
{
    public static partial class RawSceneLoader
    {
        private static bool HasNativeScene(RawSceneManifest manifest)
        { return manifest.nativeScene != null && !string.IsNullOrEmpty(manifest.nativeScene.path); }

        private sealed partial class ImportContext
        {
            private ContentSceneFile nativeScene;
            private bool nativeSceneLoaded;

            // A scene the import worker wrote in the Player format: the engine
            // loads it as a built Player loads its scenes. Its objects wake as
            // the scene integrates at the end of the frame; then it becomes the
            // active scene, which gives it its lighting settings.
            // A content scene is loaded beside the session's scene; one built
            // into a scene another owner loaded (an Addressables request)
            // gives its roots to that scene once they woke.
            private Scene LoadNative(bool runInitializers, Action commit, bool main, Scene target)
            {
                NativeSceneFileDescriptor descriptor = manifest.nativeScene;
                if (!ValidNativeHash(descriptor.sha256)) throw new InvalidDataException("Native scene descriptor is incomplete.");
                string path = AssetPath(manifest, descriptor.path);
                LoadNativeContentFiles();
                // The objects the manifest names by their sources, as a
                // content file of the scene holds them.
                foreach (NativeSceneAliasDescriptor alias in descriptor.aliases ?? Array.Empty<NativeSceneAliasDescriptor>())
                {
                    if (alias != null && destroyedByGame.Contains(AssetKey(alias.file, alias.localId)))
                    {
                        destroyedByGame.Add(AssetKey(alias.guid, alias.fileId));
                        continue;
                    }
                    if (alias == null || !assets.TryGetValue(AssetKey(alias.file, alias.localId), out Object held))
                        throw new InvalidDataException("Native scene alias names no published object: " + (alias == null ? "" : alias.guid + "/" + alias.fileId));
                    assets[AssetKey(alias.guid, alias.fileId)] = held;
                }
                // What a built Player loads before its first scene: the
                // preloaded assets (which wake as they load) and the render
                // pipeline with its global settings.
                if (main)
                {
                    LoadPreloadedAssets();
                    ConfigurePipeline();
                }
                RequireNativeContentHash(path, descriptor.sha256);
                NativeContentDependencyDescriptor[] declared = descriptor.dependencies ?? Array.Empty<NativeContentDependencyDescriptor>();
                var dependencies = new NativeArray<ContentFile>(declared.Length, Allocator.Persistent);
                try
                {
                    for (int index = 0; index < declared.Length; index++)
                    {
                        NativeContentDependencyDescriptor dependency = declared[index];
                        if (dependency == null || !ValidNativeGUID(dependency.guid)) throw new InvalidDataException("Native scene dependency GUID is invalid.");
                        if (dependency.global)
                        {
                            if (dependency.guid != BuiltinResourceGuid && dependency.guid != BuiltinMeshGuid)
                                throw new InvalidDataException("Global native dependencies must identify Unity built-in resources.");
                            dependencies[index] = ContentFile.GlobalTableDependency;
                            continue;
                        }
                        if (!nativeCache.ContentFiles.TryGetValue(dependency.guid, out NativeContentHandle handle))
                            throw new InvalidDataException("Native scene dependency has no published descriptor: " + dependency.guid);
                        dependencies[index] = handle.File;
                    }
                    if (runInitializers) InvokeRuntimeInitializers(manifest, 1);
                    // The previous scene's objects are retired before the new ones wake.
                    if (commit != null) commit();
                    nativeScene = ContentLoadInterface.LoadSceneAsync(ContentNamespace.Default, path, manifest.sceneName,
                        new ContentSceneParameters { loadSceneMode = LoadSceneMode.Additive, localPhysicsMode = LocalPhysicsMode.None, autoIntegrate = false },
                        dependencies);
                    nativeSceneLoaded = true;
                    nativeScene.WaitForLoadCompletion(30000);
                }
                finally { dependencies.Dispose(); }
                if (nativeScene.Status != SceneLoadingStatus.WaitingForIntegrate && nativeScene.Status != SceneLoadingStatus.Complete)
                    throw new InvalidDataException("Native scene failed to load (" + nativeScene.Status + "): " + path);
                Scene loadedScene = nativeScene.Scene;
                scene = target.IsValid() ? target : loadedScene;
                bool initializers = runInitializers;
                UnityEngine.Events.UnityAction<Scene, LoadSceneMode> integrated = null;
                integrated = (loaded, mode) =>
                {
                    if (loaded != loadedScene) return;
                    SceneManager.sceneLoaded -= integrated;
                    if (target.IsValid())
                        foreach (GameObject root in loadedScene.GetRootGameObjects()) SceneManager.MoveGameObjectToScene(root, target);
                    if (main) SceneManager.SetActiveScene(loadedScene);
                    if (initializers)
                    {
                        InvokeRuntimeInitializers(manifest, 0);
                        UnityMicroEditor.PlayModeEditor.EditorLoop.EnteredPlayMode();
                    }
                };
                SceneManager.sceneLoaded += integrated;
                nativeScene.IntegrateAtEndOfFrame();
                return scene;
            }

            // An asset the game asks for by address: the content files of the
            // scene hold its objects under the source their aliases name, the
            // main object first (a prefab's root, a texture before its sprites).
            private bool TryGetNativeAsset(string guid, Type type, out Object asset)
            {
                asset = null;
                foreach (NativeSceneAliasDescriptor alias in manifest.nativeScene.aliases ?? Array.Empty<NativeSceneAliasDescriptor>())
                    if (alias != null && alias.guid == guid && assets.TryGetValue(AssetKey(alias.guid, alias.fileId), out Object value)
                        && value != null && (type == null || type.IsInstanceOfType(value)))
                    { asset = value; return true; }
                foreach (NativeSceneAliasDescriptor alias in sourceAliases)
                    if (alias.guid == guid && assets.TryGetValue(AssetKey(alias.guid, alias.fileId), out Object value)
                        && value != null && (type == null || type.IsInstanceOfType(value)))
                    { asset = value; return true; }
                return false;
            }

            // Assets the session wrote while the Player ran, which the game's Editor code asked
            // the asset database for (Play mode code), named by their sources.
            private readonly List<NativeSceneAliasDescriptor> sourceAliases = new List<NativeSceneAliasDescriptor>();

            internal void AddSourceAssets(NativeContentFileDescriptor[] files, NativeSceneAliasDescriptor[] aliases)
            {
                LoadNativeContentFiles(files);
                foreach (NativeSceneAliasDescriptor alias in aliases ?? Array.Empty<NativeSceneAliasDescriptor>())
                {
                    if (alias == null || !assets.TryGetValue(AssetKey(alias.file, alias.localId), out Object held))
                        throw new InvalidDataException("Written assets name no published object: " + (alias == null ? "" : alias.guid + "/" + alias.fileId));
                    assets[AssetKey(alias.guid, alias.fileId)] = held;
                    sourceAliases.Add(alias);
                }
            }

            private void UnloadNativeScene()
            {
                if (!nativeSceneLoaded) return;
                nativeSceneLoaded = false;
                if (nativeScene.IsValid) nativeScene.UnloadAtEndOfFrame();
            }
        }
    }

    [Serializable]
    public sealed class NativeSceneFileDescriptor
    {
        public string path;
        public string sha256;
        public NativeContentDependencyDescriptor[] dependencies;
        public NativeSceneAliasDescriptor[] aliases;
    }

    [Serializable]
    public sealed class NativeSceneAliasDescriptor
    {
        public string guid;
        public string fileId;
        public string file;
        public string localId;
    }
}
