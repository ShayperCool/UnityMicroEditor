using System;
using UnityEngine;

namespace Dungeon.CLIPlay.Raw
{
    [Serializable]
    public sealed class RawSceneManifest
    {
        public int schemaVersion;
        public string unityVersion;
        public string sceneName;
        public string scenePath;
        public string projectRoot;
        public bool supported;
        public string[] assemblies;
        public int lifecycleBarrierVersion;
        public string[] lifecycleBarrierAssemblies;
        public string importedAssetCatalog;
        public ImportedAssetPack[] importedAssetPacks;
        public NativeContentFileDescriptor[] nativeContentFiles;
        public string renderPipelineGuid;
        public string renderPipelineFileId;
        public RawAssetReference[] preloadedAssets;
        public RawRenderPipelineGlobalSettings[] renderPipelineGlobalSettings;
        public RawRuntimeInitializer[] runtimeInitializers;
        public ImportedAssetBinding[] importedAssetBindings;
        public int scriptableObjectBarrierVersion;
        public bool emptyBuiltinLightingDataVerified;
        public RawSceneObject[] objects;
        public RawSceneAsset[] assets;
        public RawSceneDiagnostic[] diagnostics;
    }

    [Serializable]
    public sealed class RawAssetReference
    {
        public string guid;
        public string fileId;
        public int type;
    }

    [Serializable]
    public sealed class RawRenderPipelineGlobalSettings
    {
        public string pipelineTypeName;
        public string guid;
        public string fileId;
    }

    [Serializable]
    public sealed class ImportedAssetPack
    {
        public string bundlePath;
        public uint crc;
        public string sha256;
        public string registryAsset;
    }

    [Serializable]
    public sealed class NativeContentFileDescriptor
    {
        public string guid;
        public string path;
        public string sha256;
        public NativeContentObjectDescriptor[] objects;
        public NativeContentResourceDescriptor[] resources;
        public NativeContentDependencyDescriptor[] dependencies;
        public NativeMeshCpuRetentionDescriptor[] meshCpuRetention;
        public NativeUIBindingDescriptor[] uiBindings;
    }

    [Serializable]
    public sealed class NativeUIBindingDescriptor
    {
        public string ownerFileId;
        public string propertyPath;
        public string targetGuid;
        public string targetFileId;
        public string managedReferenceId;
    }

    [Serializable]
    public sealed class NativeMeshCpuRetentionDescriptor
    {
        public string fileId;
        public bool originalIsReadable;
    }

    [Serializable]
    public sealed class NativeContentObjectDescriptor
    {
        public string fileId;
        public string typeName;
    }

    [Serializable]
    public sealed class NativeContentResourceDescriptor
    {
        public string path;
        public string virtualPath;
        public string sha256;
    }

    [Serializable]
    public sealed class NativeContentDependencyDescriptor
    {
        public string guid;
        public bool global;
    }

    [Serializable]
    public sealed class ImportedAssetBinding
    {
        public string ownerGuid;
        public string ownerFileId;
        public string propertyPath;
        public string targetGuid;
        public string targetFileId;
    }

    [Serializable]
    public sealed class RawSceneObject
    {
        public string fileId;
        public int classId;
        public string gameObjectId;
        public string sourcePath;
        public string guid;
        public string typeName;
        public string dataJson;
        public string scalarDataJson;
        public RawSceneReference[] references;
        public bool skipMissingScript;
    }

    [Serializable]
    public sealed class RawSceneReference
    {
        public string property;
        public string fileId;
        public string guid;
        public int type;
    }

    [Serializable]
    public sealed class RawSceneAsset
    {
        public string guid;
        public string path;
        public string name;
        public string kind;
        public string fileId;
        public int classId;
        public string typeName;
        public string dataJson;
        public string importerJson;
        public RawSceneReference[] references;
        public string shaderName;
        public Color color = Color.white;
        public string textureGuid;
        public string textureFileId;
        public Vector2 textureScale = Vector2.one;
        public Vector2 textureOffset;
        public int materialMode;
        public string rootFileId;
        public RawSceneObject[] objects;
        public RawPrefabAlias[] objectAliases;
        public int executionOrder;
    }

    [Serializable]
    public sealed class RawRuntimeInitializer
    {
        public string assembly;
        public string typeName;
        public string methodName;
        public int phase;
    }

    [Serializable]
    public sealed class RawPrefabAlias
    {
        public string fileId;
        public string objectId;
    }

    [Serializable]
    public sealed class RawSceneDiagnostic
    {
        public string severity;
        public string code;
        public string path;
        public string fileId;
        public string message;
    }
}
