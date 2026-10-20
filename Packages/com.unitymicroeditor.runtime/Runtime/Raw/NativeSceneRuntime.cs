using System;
using System.IO;
using Unity.Collections;
using Unity.Content;
using Unity.Loading;
using UnityEngine;
using UnityEngine.SceneManagement;

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

            internal bool IsNativeScene { get { return HasNativeScene(manifest); } }

            // A scene the import worker wrote in the Player format: the engine
            // loads it as a built Player loads its scenes. Its objects wake as
            // the scene integrates at the end of the frame; then it becomes the
            // active scene, which gives it its lighting settings.
            private Scene LoadNative(bool runInitializers, Action commit, Scene target)
            {
                if (target.IsValid()) throw new InvalidOperationException("A scene in the Player format is not built into another scene.");
                NativeSceneFileDescriptor descriptor = manifest.nativeScene;
                if (!ValidNativeHash(descriptor.sha256)) throw new InvalidDataException("Native scene descriptor is incomplete.");
                string path = AssetPath(manifest, descriptor.path);
                LoadNativeContentFiles();
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
                scene = nativeScene.Scene;
                bool initializers = runInitializers;
                UnityEngine.Events.UnityAction<Scene, LoadSceneMode> integrated = null;
                integrated = (loaded, mode) =>
                {
                    if (loaded != scene) return;
                    SceneManager.sceneLoaded -= integrated;
                    SceneManager.SetActiveScene(scene);
                    if (initializers) InvokeRuntimeInitializers(manifest, 0);
                };
                SceneManager.sceneLoaded += integrated;
                nativeScene.IntegrateAtEndOfFrame();
                return scene;
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
    }
}
