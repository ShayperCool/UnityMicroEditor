using System;
using System.IO;
using System.Reflection;
using UnityEngine;
using UnityEngine.Rendering;
using Object = UnityEngine.Object;

namespace UnityMicroEditor.Raw
{
    public static partial class RawSceneLoader
    {
        // The phases after the RuntimeInitializeLoadType ones belong to Play mode
        // code: [InitializeOnLoad] classes (5), [InitializeOnLoadMethod] (6) and
        // [InitializeOnEnterPlayMode] (7) methods, which the Editor runs as it
        // enters Play mode, before any runtime initializer.
        private const int LastRuntimePhase = 4, LastEditorPhase = 7;

        // The game's [RuntimeInitializeOnLoadMethod] methods, which the
        // lifecycle compiler exposes as public bridges, run at the points a
        // built Player runs them.
        private static void InvokeRuntimeInitializers(RawSceneManifest manifest, int phase)
        {
            if (manifest.runtimeInitializers == null) return;
            foreach (RawRuntimeInitializer initializer in manifest.runtimeInitializers)
            {
                if (initializer == null || initializer.phase < 0 || initializer.phase > LastEditorPhase
                    || string.IsNullOrEmpty(initializer.assembly) || string.IsNullOrEmpty(initializer.typeName)
                    || string.IsNullOrEmpty(initializer.methodName)
                    || !initializer.methodName.StartsWith(initializer.phase > LastRuntimePhase ? "__UnityPlay_EditorInitialize_" : "__UnityPlay_RuntimeInitialize_", StringComparison.Ordinal))
                    throw new InvalidDataException("Runtime initialization bridge descriptor is invalid.");
                if (initializer.phase != phase) continue;
                Assembly assembly = null;
                if (manifest.assemblies != null)
                    foreach (string path in manifest.assemblies)
                        if (Path.GetFileNameWithoutExtension(path) == initializer.assembly)
                        {
                            foreach (Assembly candidate in AppDomain.CurrentDomain.GetAssemblies())
                                if (candidate.GetName().Name == initializer.assembly) { assembly = candidate; break; }
                            break;
                        }
                if (assembly == null) throw new InvalidDataException("Runtime initializer assembly is not a supplied source assembly: " + initializer.assembly);
                Type type = assembly.GetType(initializer.typeName, false);
                MethodInfo method = type == null ? null : type.GetMethod(initializer.methodName, BindingFlags.Public | BindingFlags.Static | BindingFlags.DeclaredOnly);
                if (method == null || method.ReturnType != typeof(void) || method.GetParameters().Length != 0 || method.ContainsGenericParameters)
                    throw new InvalidDataException("Runtime initializer is not a public static parameterless compiler bridge: " + initializer.typeName + "." + initializer.methodName);
                var callback = (Action)Delegate.CreateDelegate(typeof(Action), method);
                try { callback(); }
                catch (Exception exception) { Debug.LogException(exception); }
            }
        }

        private sealed partial class ImportContext
        {
            // What a built Player loads before its first scene.
            private void LoadPreloadedAssets()
            {
                if (manifest.preloadedAssets == null) { preloadedAssets = Array.Empty<Object>(); return; }
                preloadedAssets = new Object[manifest.preloadedAssets.Length];
                for (int index = 0; index < preloadedAssets.Length; index++)
                {
                    RawAssetReference reference = manifest.preloadedAssets[index];
                    preloadedAssets[index] = Resolve(reference.guid, reference.fileId);
                }
            }

            // The project's render pipeline; the host Player keeps the global
            // settings its build stripped by the pipeline's own rules.
            private void ConfigurePipeline()
            {
                if (string.IsNullOrEmpty(manifest.renderPipelineGuid)) return;
                var pipeline = Resolve(manifest.renderPipelineGuid, manifest.renderPipelineFileId) as RenderPipelineAsset;
                if (pipeline == null) throw new InvalidDataException("The scene's render pipeline reference is not a RenderPipelineAsset.");
                GraphicsSettings.defaultRenderPipeline = pipeline;
                QualitySettings.renderPipeline = pipeline;
            }
        }
    }
}
