using System;
using UnityEngine;

namespace UnityMicroEditor.Raw
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
        public NativeContentFileDescriptor[] nativeContentFiles;
        public NativeSceneFileDescriptor nativeScene;
        public string renderPipelineGuid;
        public string renderPipelineFileId;
        public RawAssetReference[] preloadedAssets;
        public RawRuntimeInitializer[] runtimeInitializers;
    }

    [Serializable]
    public sealed class RawAssetReference
    {
        public string guid;
        public string fileId;
        public int type;
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
        // Shaders a build compiled no program of: unsupported in a Player too.
        public string[] programlessShaders;
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
    public sealed class RawRuntimeInitializer
    {
        public string assembly;
        public string typeName;
        public string methodName;
        public int phase;
    }
}
