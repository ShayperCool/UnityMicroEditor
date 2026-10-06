using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Security.Cryptography;
using UnityEditor;
using UnityEditor.Experimental;
using UnityEngine;

// Runs only in a disposable reference project. No importer settings are changed.
// This is the measurement oracle, not the standalone worker implementation.
public static class WorkerImportReference
{
    private const string ModelPath = "Assets/Probe/R_Floor.fbx";
    private const string AudioPath = "Assets/Probe/Probe.wav";

    public static void Capture()
    {
        string reportPath = Argument("--worker-reference-report");
        var report = new Report { unityVersion = Application.unityVersion };
        try
        {
            AssetDatabase.ActiveRefreshImportMode = AssetDatabase.RefreshImportMode.InProcess;
            AssetDatabase.ImportAsset(ModelPath, ImportAssetOptions.ForceUpdate | ImportAssetOptions.ForceSynchronousImport);
            AssetDatabase.ImportAsset(AudioPath, ImportAssetOptions.ForceUpdate | ImportAssetOptions.ForceSynchronousImport);
            report.refreshImportMode = AssetDatabase.ActiveRefreshImportMode.ToString();
            report.model = Model();
            report.audio = Audio();
            report.artifacts = new[] { Artifact(ModelPath), Artifact(AudioPath) };
            report.artifactFilesMaterialized = report.artifacts.All(value => value.files.All(file => file.materialized));
            report.success = true;
        }
        catch (Exception error)
        {
            report.error = error.ToString();
            Debug.LogException(error);
        }
        Directory.CreateDirectory(Path.GetDirectoryName(Path.GetFullPath(reportPath)));
        File.WriteAllText(reportPath, JsonUtility.ToJson(report, true));
        EditorApplication.Exit(report.success ? 0 : 1);
    }

    private static ModelRecord Model()
    {
        GameObject root = AssetDatabase.LoadAssetAtPath<GameObject>(ModelPath);
        if (root == null) throw new InvalidDataException("FBX root was not imported");
        var objects = new List<ObjectRecord>();
        foreach (UnityEngine.Object value in AssetDatabase.LoadAllAssetsAtPath(ModelPath))
            if (value != null) objects.Add(Object(value, ""));
        var hierarchy = new List<HierarchyRecord>();
        Hierarchy(root.transform, "", hierarchy, objects);
        MeshRecord[] meshes = AssetDatabase.LoadAllAssetsAtPath(ModelPath).OfType<Mesh>()
            .Select(Mesh).OrderBy(value => value.localFileId).ToArray();
        return new ModelRecord { guid = AssetDatabase.AssetPathToGUID(ModelPath),
            objects = objects.GroupBy(value => value.guid + "/" + value.localFileId).Select(group => group.First())
                .OrderBy(value => value.localFileId).ToArray(), hierarchy = hierarchy.ToArray(), meshes = meshes };
    }

    private static void Hierarchy(Transform transform, string parent, List<HierarchyRecord> hierarchy, List<ObjectRecord> objects)
    {
        string path = parent + "/" + transform.name;
        ObjectRecord gameObject = Object(transform.gameObject, path);
        objects.Add(gameObject);
        objects.Add(Object(transform, path));
        var componentIds = new List<long>();
        foreach (Component component in transform.gameObject.GetComponents<Component>())
        {
            if (component == null) continue;
            ObjectRecord record = Object(component, path);
            objects.Add(record);
            componentIds.Add(record.localFileId);
        }
        hierarchy.Add(new HierarchyRecord { path = path, localFileId = gameObject.localFileId,
            componentLocalFileIds = componentIds.ToArray(), localPosition = transform.localPosition,
            localRotation = transform.localRotation, localScale = transform.localScale,
            activeSelf = transform.gameObject.activeSelf });
        for (int index = 0; index < transform.childCount; index++)
            Hierarchy(transform.GetChild(index), path, hierarchy, objects);
    }

    private static ObjectRecord Object(UnityEngine.Object value, string path)
    {
        if (!AssetDatabase.TryGetGUIDAndLocalFileIdentifier(value, out string guid, out long fileId))
            throw new InvalidDataException("Imported object has no stable GUID/local file ID: " + value.name);
        return new ObjectRecord { name = value.name, type = value.GetType().FullName,
            guid = guid, localFileId = fileId, hierarchyPath = path };
    }

    private static MeshRecord Mesh(Mesh mesh)
    {
        ObjectRecord identity = Object(mesh, "");
        var submeshes = new List<SubmeshRecord>();
        for (int index = 0; index < mesh.subMeshCount; index++)
        {
            int[] indices = mesh.GetIndices(index);
            submeshes.Add(new SubmeshRecord { topology = mesh.GetTopology(index).ToString(),
                indexCount = indices.Length, indicesSha256 = Hash(writer => Ints(writer, indices)) });
        }
        return new MeshRecord { name = mesh.name, localFileId = identity.localFileId,
            vertexCount = mesh.vertexCount, bounds = mesh.bounds, submeshes = submeshes.ToArray(),
            verticesSha256 = Hash(writer => Vectors(writer, mesh.vertices)),
            normalsSha256 = Hash(writer => Vectors(writer, mesh.normals)),
            tangentsSha256 = Hash(writer => { Vector4[] values = mesh.tangents; writer.Write(values.Length);
                foreach (Vector4 value in values) { writer.Write(value.x); writer.Write(value.y); writer.Write(value.z); writer.Write(value.w); } }),
            uvSha256 = Hash(writer => { Vector2[] values = mesh.uv; writer.Write(values.Length);
                foreach (Vector2 value in values) { writer.Write(value.x); writer.Write(value.y); } }),
            bindposesSha256 = Hash(writer => { Matrix4x4[] values = mesh.bindposes; writer.Write(values.Length);
                foreach (Matrix4x4 value in values) for (int index = 0; index < 16; index++) writer.Write(value[index]); }) };
    }

    private static AudioRecord Audio()
    {
        AudioClip clip = AssetDatabase.LoadAssetAtPath<AudioClip>(AudioPath);
        if (clip == null) throw new InvalidDataException("WAV was not imported");
        clip.LoadAudioData();
        float[] samples = new float[checked(clip.samples * clip.channels)];
        if (!clip.GetData(samples, 0)) throw new InvalidDataException("Imported AudioClip.GetData failed");
        ObjectRecord identity = Object(clip, "");
        return new AudioRecord { name = clip.name, guid = identity.guid, localFileId = identity.localFileId,
            samples = clip.samples, channels = clip.channels, frequency = clip.frequency, length = clip.length,
            loadType = clip.loadType.ToString(), loadState = clip.loadState.ToString(),
            pcmFloatSha256 = Hash(writer => { writer.Write(samples.Length); foreach (float sample in samples) writer.Write(sample); }),
            firstSamples = samples.Take(16).ToArray() };
    }

    private static ArtifactRecord Artifact(string asset)
    {
        string guid = AssetDatabase.AssetPathToGUID(asset);
        ImportResultID artifact = AssetDatabaseExperimental.LookupArtifact(new ArtifactKey(new GUID(guid)));
        if (!artifact.isValid) throw new InvalidDataException("No imported artifact for " + asset);
        AssetDatabaseExperimental.GetArtifactPaths(artifact, out string[] paths);
        return new ArtifactRecord { asset = asset, guid = guid, artifactId = artifact.ToString(),
            files = paths.OrderBy(value => value, StringComparer.Ordinal).Select(ArtifactFileInfo).ToArray() };
    }

    private static ArtifactFile ArtifactFileInfo(string path)
    {
        // Unity 6.6 returns VirtualArtifacts/Primary/... paths, which its
        // filesystem resolves into uds:/ content. System.IO cannot access
        // those mounts. Preserve the exact API value for the independent
        // DataStore reader; Model/Audio above establish content correctness.
        if (path.StartsWith("uds:", StringComparison.Ordinal) || !File.Exists(path))
            return new ArtifactFile { path = path, bytes = -1, materialized = false, sha256 = "" };
        return new ArtifactFile { path = Path.GetFullPath(path), bytes = new FileInfo(path).Length,
            materialized = true, sha256 = BytesHash(File.ReadAllBytes(path)) };
    }

    private static string Hash(Action<BinaryWriter> write)
    {
        using (var stream = new MemoryStream())
        {
            using (var writer = new BinaryWriter(stream, System.Text.Encoding.UTF8, true)) write(writer);
            return BytesHash(stream.ToArray());
        }
    }

    private static string BytesHash(byte[] bytes)
    {
        using (var hash = SHA256.Create())
            return BitConverter.ToString(hash.ComputeHash(bytes)).Replace("-", "").ToLowerInvariant();
    }

    private static void Ints(BinaryWriter writer, int[] values)
    {
        writer.Write(values.Length);
        foreach (int value in values) writer.Write(value);
    }

    private static void Vectors(BinaryWriter writer, Vector3[] values)
    {
        writer.Write(values.Length);
        foreach (Vector3 value in values) { writer.Write(value.x); writer.Write(value.y); writer.Write(value.z); }
    }

    private static string Argument(string name)
    {
        string[] arguments = Environment.GetCommandLineArgs();
        for (int index = 0; index + 1 < arguments.Length; index++)
            if (arguments[index] == name) return arguments[index + 1];
        throw new ArgumentException("Missing " + name);
    }

    [Serializable] private sealed class Report
    {
        public int schemaVersion = 1;
        public bool success;
        public bool artifactFilesMaterialized;
        public string unityVersion, refreshImportMode, error;
        public ModelRecord model;
        public AudioRecord audio;
        public ArtifactRecord[] artifacts;
    }
    [Serializable] private sealed class ModelRecord { public string guid; public ObjectRecord[] objects; public HierarchyRecord[] hierarchy; public MeshRecord[] meshes; }
    [Serializable] private sealed class ObjectRecord { public string name, type, guid, hierarchyPath; public long localFileId; }
    [Serializable] private sealed class HierarchyRecord { public string path; public long localFileId; public long[] componentLocalFileIds; public Vector3 localPosition, localScale; public Quaternion localRotation; public bool activeSelf; }
    [Serializable] private sealed class MeshRecord { public string name, verticesSha256, normalsSha256, tangentsSha256, uvSha256, bindposesSha256; public long localFileId; public int vertexCount; public Bounds bounds; public SubmeshRecord[] submeshes; }
    [Serializable] private sealed class SubmeshRecord { public string topology, indicesSha256; public int indexCount; }
    [Serializable] private sealed class AudioRecord { public string name, guid, loadType, loadState, pcmFloatSha256; public long localFileId; public int samples, channels, frequency; public float length; public float[] firstSamples; }
    [Serializable] private sealed class ArtifactRecord { public string asset, guid, artifactId; public ArtifactFile[] files; }
    [Serializable] private sealed class ArtifactFile { public string path, sha256; public long bytes; public bool materialized; }
}
