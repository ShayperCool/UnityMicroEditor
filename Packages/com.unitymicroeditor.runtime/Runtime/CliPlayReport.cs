using System;
using System.Collections.Generic;
using System.IO;
using System.Text;
using UnityEngine;
using UnityEngine.LowLevel;
using UnityEngine.SceneManagement;

namespace UnityMicroEditor
{
    [Serializable]
    public sealed class CliPlayReport
    {
        public int schemaVersion = 1;
        public string observerVersion = "baseline";
        public bool success;
        public int exitCode;
        public string engineVersion;
        public SceneReport scene;
        public string requestedScene;
        public int frames;
        public int requestedFrames;
        public double elapsedSeconds;
        public double realtimeSeconds;
        public long bootstrapUtcUnixMilliseconds;
        public double bootstrapRealtimeSeconds;
        public long readyUtcUnixMilliseconds;
        public double readyRealtimeSeconds;
        public double elapsedToReadySeconds;
        public long completedFramesUtcUnixMilliseconds;
        public double completedFramesRealtimeSeconds;
        public double afterReadyElapsedSeconds;
        public double observedFramesPerSecond;
        public string readyReportPath;
        public string contentManifestPath;
        public string contentRevisionManifestPath;
        public string contentScenePath;
        public string contentRevision;
        public string rawReloadRequestPath;
        public string controlDirectory;
        public int pid;
        public int rawReloadCount;
        public int nativeLoadCount;
        public int nativeObjectCount;
        public string nativeIdentityDigest;
        public int targetFrameRate;
        public int vSyncCount;
        public bool noVsyncRequested;
        public string screenshotPath;
        public bool screenshotWritten;
        public int screenshotWidth;
        public int screenshotHeight;
        public double screenshotCaptureSeconds;
        public bool isBatchMode;
        public string graphicsDeviceType;
        public string graphicsDeviceName;
        public string graphicsDeviceVendor;
        public int graphicsDeviceID;
        public int graphicsDeviceVendorID;
        public string graphicsDeviceVersion;
        public int graphicsMemorySizeMB;
        public bool graphicsMultiThreaded;
        public int errors;
        public int exceptions;
        public string fatalError;
        public LogReport[] capturedLogs;
        public LoopReport playerLoop;
        public PhysicsReport physics;

        internal static CliPlayReport Create()
        {
            Scene active = SceneManager.GetActiveScene();
            return new CliPlayReport
            {
                engineVersion = Application.unityVersion,
                scene = SceneReport.Capture(active),
                requestedScene = string.Empty,
                realtimeSeconds = Time.realtimeSinceStartupAsDouble,
                isBatchMode = Application.isBatchMode,
                graphicsDeviceType = SystemInfo.graphicsDeviceType.ToString(),
                graphicsDeviceName = SystemInfo.graphicsDeviceName,
                graphicsDeviceVendor = SystemInfo.graphicsDeviceVendor,
                graphicsDeviceID = SystemInfo.graphicsDeviceID,
                graphicsDeviceVendorID = SystemInfo.graphicsDeviceVendorID,
                graphicsDeviceVersion = SystemInfo.graphicsDeviceVersion,
                graphicsMemorySizeMB = SystemInfo.graphicsMemorySize,
                graphicsMultiThreaded = SystemInfo.graphicsMultiThreaded,
                targetFrameRate = Application.targetFrameRate,
                vSyncCount = QualitySettings.vSyncCount,
                fatalError = string.Empty,
                capturedLogs = Array.Empty<LogReport>(),
                playerLoop = LoopReport.Capture(PlayerLoop.GetCurrentPlayerLoop()),
                physics = new PhysicsReport
                {
                    simulationMode = Physics.simulationMode.ToString(),
                    autoSyncTransforms = Physics.autoSyncTransforms,
                    fixedDeltaTime = Time.fixedDeltaTime,
                },
            };
        }

        internal void Write(string path)
        {
            CliPlayArtifact.WriteText(path, JsonUtility.ToJson(this, true) + "\n");
        }
    }

    [Serializable]
    public sealed class CliPlayRawReloadRequest
    {
        public int schemaVersion;
        public string requestId;
        public string manifest;
        public string manifestSha256;
        public string patch;
        public string patchSha256;
        public string revision;
        public long requestedUtcUnixMilliseconds;
        public long sourceChangedUtcUnixMilliseconds;
    }

    [Serializable]
    public sealed class CliPlayRawReloadReport
    {
        public int schemaVersion = 1;
        public string requestId;
        public bool success;
        public string manifest;
        public string revision;
        public string loadedRevision;
        public bool restartRequired;
        public int pid;
        public double elapsedSeconds;
        public long completedUtcUnixMilliseconds;
        public long requestedUtcUnixMilliseconds;
        public long sourceChangedUtcUnixMilliseconds;
        public int errors;
        public int exceptions;
        public string fatalError;
        public SceneReport scene;
        public int reloadCount;
        public int nativeLoadCount;
        public int nativeObjectCount;
        public string nativeIdentityDigest;
        internal void Write(string path) { CliPlayArtifact.WriteText(path, JsonUtility.ToJson(this, true) + "\n"); }
    }

    [Serializable]
    public sealed class SceneReport
    {
        public string name;
        public string path;
        public int buildIndex;
        public bool isLoaded;

        internal static SceneReport Capture(Scene scene)
        {
            return new SceneReport { name = scene.name, path = scene.path, buildIndex = scene.buildIndex, isLoaded = scene.isLoaded };
        }
    }

    [Serializable]
    public sealed class CliPlayReadyReport
    {
        public int schemaVersion = 1;
        public string @event = "first-active-scene-end-frame";
        public long bootstrapUtcUnixMilliseconds;
        public double bootstrapRealtimeSeconds;
        public long readyUtcUnixMilliseconds;
        public double readyRealtimeSeconds;
        public double elapsedToReadySeconds;
        public string engineVersion;
        public SceneReport scene;
        public string contentManifestPath;
        public string contentRevisionManifestPath;
        public string contentScenePath;
        public string contentRevision;
        public string rawReloadRequestPath;
        public string controlDirectory;
        public int pid;
        public int rawReloadCount;
        public int nativeLoadCount;
        public int nativeObjectCount;
        public string nativeIdentityDigest;
        public int targetFrameRate;
        public int vSyncCount;
        public string graphicsDeviceType;
        public string graphicsDeviceName;
        public string graphicsDeviceVendor;
        public int graphicsDeviceID;
        public int graphicsDeviceVendorID;
        public string graphicsDeviceVersion;
        public int graphicsMemorySizeMB;
        public int errors;
        public int exceptions;

        internal void Write(string path)
        {
            CliPlayArtifact.WriteText(path, JsonUtility.ToJson(this, true) + "\n");
        }
    }

    internal static class CliPlayArtifact
    {
        internal static void WriteText(string path, string text)
        {
            string temporary = PrepareTemporary(path);
            try
            {
                File.WriteAllText(temporary, text, new UTF8Encoding(false));
                Commit(temporary, path);
            }
            finally
            {
                if (File.Exists(temporary)) File.Delete(temporary);
            }
        }

        internal static void WriteBytes(string path, byte[] bytes)
        {
            string temporary = PrepareTemporary(path);
            try
            {
                File.WriteAllBytes(temporary, bytes);
                Commit(temporary, path);
            }
            finally
            {
                if (File.Exists(temporary)) File.Delete(temporary);
            }
        }

        private static string PrepareTemporary(string path)
        {
            string directory = Path.GetDirectoryName(path);
            if (!string.IsNullOrEmpty(directory)) Directory.CreateDirectory(directory);
            return path + "." + Guid.NewGuid().ToString("N") + ".tmp";
        }

        private static void Commit(string temporary, string path)
        {
            if (File.Exists(path)) File.Replace(temporary, path, null);
            else File.Move(temporary, path);
        }
    }

    [Serializable]
    public sealed class LogReport
    {
        public string type;
        public string message;
        public string stackTrace;
    }

    [Serializable]
    public sealed class LoopReport
    {
        public string type;
        [NonSerialized] public LoopReport[] children;
        public LoopEntry[] systems;

        internal static LoopReport Capture(PlayerLoopSystem system)
        {
            LoopReport root = CaptureTree(system);
            var entries = new List<LoopEntry>();
            Flatten(root, -1, 0, entries);
            root.systems = entries.ToArray();
            return root;
        }

        private static LoopReport CaptureTree(PlayerLoopSystem system)
        {
            int count = system.subSystemList == null ? 0 : system.subSystemList.Length;
            var result = new LoopReport
            {
                type = system.type == null ? "PlayerLoop" : system.type.FullName,
                children = new LoopReport[count],
            };
            for (int index = 0; index < count; index++) result.children[index] = CaptureTree(system.subSystemList[index]);
            return result;
        }

        private static void Flatten(LoopReport node, int parent, int depth, List<LoopEntry> entries)
        {
            int index = entries.Count;
            entries.Add(new LoopEntry { type = node.type, parentIndex = parent, depth = depth });
            foreach (LoopReport child in node.children) Flatten(child, index, depth + 1, entries);
        }
    }

    [Serializable]
    public sealed class LoopEntry
    {
        public string type;
        public int parentIndex;
        public int depth;
    }

    [Serializable]
    public sealed class PhysicsReport
    {
        public string simulationMode;
        public bool autoSyncTransforms;
        public float fixedDeltaTime;
    }
}
