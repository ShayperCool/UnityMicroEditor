using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Security.Cryptography;
using Unity.Collections;
using Unity.Content;
using Unity.IO.Archive;
using Unity.Loading;
using UnityEngine;
using Object = UnityEngine.Object;
using Debug = UnityEngine.Debug;

namespace UnityPlay.WorkerProbe
{
    // Loaded into an existing Mono Player by the raw loader's public initializer
    // bridge. This fixture probe deliberately uses only public Unity runtime APIs.
    public static class WorkerRuntimeDeliveryProbe
    {
        private const string ExpectedPCMHash = "ad4d61350a59160ef1b86a1c6cf0dfc6f496872863076d68886e61170ec32f44";
        private static readonly List<string> Messages = new List<string>();
        private static readonly object MessageLock = new object();
        private static int NativeErrors;

        public static void __UnityPlay_RuntimeInitialize_WorkerDelivery()
        {
            string output = RequiredAbsoluteEnvironmentPath("UNITY_PLAY_WORKER_DELIVERY_REPORT");
            string input = RequiredAbsoluteEnvironmentPath("UNITY_PLAY_WORKER_DELIVERY_FILE");
            string kind = Environment.GetEnvironmentVariable("UNITY_PLAY_WORKER_DELIVERY_KIND");
            if (kind != "mesh" && kind != "audio") throw new InvalidDataException("Delivery probe kind must be mesh or audio.");
            var report = new Report { schemaVersion = 1, unityVersion = Application.unityVersion, input = input, kind = kind, stage = "starting" };
            var watch = Stopwatch.StartNew();
            ContentFile content = default(ContentFile);
            bool defaultNamespace = Environment.GetEnvironmentVariable("UNITY_PLAY_WORKER_DELIVERY_DEFAULT_NAMESPACE") == "1";
            ContentNamespace space = defaultNamespace ? ContentNamespace.Default : ContentNamespace.GetOrCreateNamespace("WorkerProbe");
            ArchiveHandle archive = default(ArchiveHandle);
            bool archiveMounted = false;
            Messages.Clear();
            NativeErrors = 0;
            Application.logMessageReceivedThreaded += CaptureMessage;
            try
            {
                Write(output, report);
                string archivePath = Environment.GetEnvironmentVariable("UNITY_PLAY_WORKER_DELIVERY_ARCHIVE");
                if (!string.IsNullOrEmpty(archivePath))
                {
                    if (!Path.IsPathRooted(archivePath)) throw new InvalidDataException("Archive path must be absolute.");
                    report.archive = Path.GetFullPath(archivePath);
                    report.stage = "mounting";
                    Write(output, report);
                    archive = ArchiveFileInterface.MountAsync(space, report.archive, string.Empty);
                    archiveMounted = true;
                    archive.JobHandle.Complete();
                    if (archive.Status != ArchiveStatus.Complete) throw new InvalidDataException("Archive mount failed: " + archive.Status);
                    report.mountPath = archive.GetMountPath();
                    input = report.mountPath + "VirtualArtifacts/Primary/"
                        + (kind == "mesh" ? "b1d15452ab70b6a669e9b8e3c0b91251" : "71705265666978617564696f70726f62");
                    report.input = input;
                }
                var dependencies = new NativeArray<ContentFile>(kind == "mesh" ? 1 : 0, Allocator.Persistent);
                try
                {
                    if (kind == "mesh") dependencies[0] = ContentFile.GlobalTableDependency;
                    report.stage = "loading";
                    Write(output, report);
                    content = ContentLoadInterface.LoadContentFileAsync(space, input, dependencies);
                    report.validHandle = content.IsValid;
                    report.completedWithinTimeout = content.WaitForCompletion(10000);
                    report.status = content.LoadingStatus.ToString();
                    report.loadSeconds = watch.Elapsed.TotalSeconds;
                    Write(output, report);
                    if (!report.completedWithinTimeout || content.LoadingStatus != LoadingStatus.Completed)
                        throw new InvalidDataException("Native ContentFile did not complete successfully: " + report.status);
                }
                finally { dependencies.Dispose(); }
                report.stage = "loaded";
                Object[] objects = content.GetObjects();
                report.objectCount = objects.Length;
                var types = new List<string>(objects.Length);
                foreach (Object value in objects) types.Add(value == null ? "null" : value.GetType().FullName + ":" + value.name);
                report.objects = types.ToArray();
                if (kind == "mesh") ValidateMesh(content, report);
                else ValidateAudio(content, report, output);
                report.runtimeObjectsValidated = true;
                report.success = true;
            }
            catch (Exception error)
            {
                report.error = error.ToString();
                Debug.LogError("[unity-worker-delivery] " + report.error);
            }
            finally
            {
                try
                {
                    if (content.IsValid)
                    {
                        report.stage = "unloading";
                        Write(output, report);
                        report.unloaded = content.UnloadAsync().WaitForCompletion(10000);
                        if (!report.unloaded) { report.success = false; report.error += "\nContentFile unload timed out."; }
                    }
                    if (archiveMounted) archive.Unmount().Complete();
                    if (!defaultNamespace) space.Delete();
                }
                catch (Exception error) { report.success = false; report.error += "\nCleanup: " + error; }
                Application.logMessageReceivedThreaded -= CaptureMessage;
                lock (MessageLock) { report.messages = Messages.ToArray(); report.nativeErrors = NativeErrors; }
                if (report.nativeErrors > 0)
                {
                    report.success = false;
                    if (string.IsNullOrEmpty(report.error)) report.error = "Native errors occurred while delivering the fixture; inspect messages.";
                }
                report.totalSeconds = watch.Elapsed.TotalSeconds;
                report.stage = report.success ? "validated" : "failed";
                Write(output, report);
                Debug.Log("[unity-worker-delivery] " + JsonUtility.ToJson(report));
            }
        }

        private static void ValidateMesh(ContentFile content, Report report)
        {
            // Exact 64-bit local identifier from this fixture's native artifact,
            // preserved across the Editor and independent worker imports.
            var mesh = content.GetObject(0x6e96ce2c1bf45a14UL) as Mesh;
            if (mesh == null) throw new InvalidDataException("The fixture's exact local Mesh ID was not restored.");
            report.meshVertices = mesh.vertexCount;
            report.meshSubMeshes = mesh.subMeshCount;
            report.meshReadable = mesh.isReadable;
            report.meshIndices = mesh.subMeshCount == 1 ? checked((int)mesh.GetIndexCount(0)) : 0;
            report.meshBounds = mesh.bounds.ToString();
            if (report.meshVertices != 24 || report.meshSubMeshes != 1 || report.meshIndices != 36)
                throw new InvalidDataException("The native Mesh topology differs from the fixture import.");
            var root = content.GetObject(0x0cc169d1e40d3d51UL) as GameObject;
            var child = content.GetObject(0x6bd876a88717979cUL) as GameObject;
            if (root == null || child == null || !child.TryGetComponent<MeshFilter>(out var filter))
                throw new InvalidDataException("The native model GameObject hierarchy or MeshFilter was not restored.");
            report.modelHierarchyRestored = root.transform.childCount == 1 && child.transform.parent == root.transform;
            report.modelMeshReferenceRestored = filter.sharedMesh == mesh;
            report.modelRootActiveSelf = root.activeSelf;
            report.modelRootActiveInHierarchy = root.activeInHierarchy;
            report.modelRootSceneValid = root.scene.IsValid();
            if (!report.modelHierarchyRestored || !report.modelMeshReferenceRestored)
                throw new InvalidDataException("Native model graph references differ from the fixture import.");
        }

        private static void ValidateAudio(ContentFile content, Report report, string output)
        {
            var clip = content.GetObject(8300000UL) as AudioClip;
            if (clip == null) throw new InvalidDataException("The fixture's exact local AudioClip ID was not restored.");
            report.audioSamples = clip.samples;
            report.audioChannels = clip.channels;
            report.audioFrequency = clip.frequency;
            report.audioLoadState = clip.loadState.ToString();
            Write(output, report);
            if (report.audioSamples != 4096 || report.audioChannels != 2 || report.audioFrequency != 22050)
                throw new InvalidDataException("Native AudioClip metadata differs from the fixture import.");
            // Unity 6000.6.3f1 can dereference a null FMOD sound from GetData after
            // a resource-open failure. The probe must report the failed load
            // before calling that API, not turn a missing stream into a crash.
            if (clip.loadState != AudioDataLoadState.Loaded)
                throw new InvalidDataException("Native AudioClip resource did not load: " + clip.loadState);
            lock (MessageLock)
                if (NativeErrors != 0) throw new InvalidDataException("Native errors occurred before AudioClip.GetData; inspect messages.");
            var samples = new float[checked(clip.samples * clip.channels)];
            report.audioGetData = clip.GetData(samples, 0);
            if (!report.audioGetData) throw new InvalidDataException("Native AudioClip.GetData failed; check its streamed resource path.");
            using (var stream = new MemoryStream())
            using (var writer = new BinaryWriter(stream))
            using (var hash = SHA256.Create())
            {
                writer.Write(samples.Length);
                foreach (float sample in samples) writer.Write(sample);
                writer.Flush();
                report.audioPCMHash = BitConverter.ToString(hash.ComputeHash(stream.ToArray())).Replace("-", "").ToLowerInvariant();
            }
            if (report.audioPCMHash != ExpectedPCMHash) throw new InvalidDataException("Native AudioClip.GetData differs from the Editor PCM fingerprint.");
        }

        private static string RequiredAbsoluteEnvironmentPath(string name)
        {
            string value = Environment.GetEnvironmentVariable(name);
            if (string.IsNullOrEmpty(value) || !Path.IsPathRooted(value)) throw new InvalidDataException(name + " must contain an absolute path.");
            return Path.GetFullPath(value);
        }

        private static void CaptureMessage(string message, string stackTrace, LogType type)
        {
            if (type != LogType.Error && type != LogType.Exception && type != LogType.Assert && type != LogType.Warning) return;
            lock (MessageLock)
            {
                if (type == LogType.Error || type == LogType.Exception || type == LogType.Assert) NativeErrors++;
                if (Messages.Count < 64) Messages.Add(type + ": " + message);
            }
        }

        private static void Write(string output, Report report)
        {
            Directory.CreateDirectory(Path.GetDirectoryName(output));
            File.WriteAllText(output, JsonUtility.ToJson(report, true) + "\n");
        }

        [Serializable] private sealed class Report
        {
            public int schemaVersion;
            public bool success;
            public string unityVersion;
            public string input;
            public string archive;
            public string mountPath;
            public string kind;
            public string stage;
            public bool validHandle;
            public bool runtimeObjectsValidated;
            public int nativeErrors;
            public bool completedWithinTimeout;
            public string status;
            public double loadSeconds;
            public double totalSeconds;
            public int objectCount;
            public string[] objects;
            public int meshVertices;
            public int meshSubMeshes;
            public int meshIndices;
            public bool meshReadable;
            public string meshBounds;
            public bool modelHierarchyRestored;
            public bool modelMeshReferenceRestored;
            public bool modelRootActiveSelf;
            public bool modelRootActiveInHierarchy;
            public bool modelRootSceneValid;
            public int audioSamples;
            public int audioChannels;
            public int audioFrequency;
            public string audioLoadState;
            public bool audioGetData;
            public string audioPCMHash;
            public bool unloaded;
            public string error;
            public string[] messages;
        }
    }
}
