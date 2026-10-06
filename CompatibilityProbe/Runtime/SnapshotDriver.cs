using System;
using System.Collections;
using System.IO;
using UnityEngine;

namespace UnityPlay.Compatibility
{
    [DefaultExecutionOrder(32000)]
    public sealed class SnapshotDriver : MonoBehaviour
    {
        public static bool Ready { get; private set; }
        public static SceneSnapshot.Snapshot CapturedScene { get; private set; }
        public static CompatibilityLifecycleProbe.Event[] CapturedLifecycle { get; private set; }

        private IEnumerator Start()
        {
            Ready = false;
            // Components activated by another Start can receive their own Start
            // on the following frame. Both observers use this same capture gate.
            yield return null;
            CapturedScene = SceneSnapshot.Capture(gameObject.scene);
            CapturedLifecycle = CompatibilityLifecycleProbe.CaptureEvents();
            string output = Environment.GetEnvironmentVariable("UNITY_PLAY_RAW_SNAPSHOT");
            if (!string.IsNullOrEmpty(output))
            {
                if (!Path.IsPathRooted(output)) throw new InvalidOperationException("Snapshot output must be absolute.");
                var report = new Report
                {
                    schemaVersion = 1, mode = "raw-player", scene = CapturedScene,
                    lifecycle = CapturedLifecycle,
                };
                Directory.CreateDirectory(Path.GetDirectoryName(output));
                File.WriteAllText(output, SceneSnapshot.ToJson(report) + "\n");
            }
            Ready = true;
        }

        [Serializable] private sealed class Report
        {
            public int schemaVersion;
            public string mode;
            public SceneSnapshot.Snapshot scene;
            public CompatibilityLifecycleProbe.Event[] lifecycle;
        }
    }
}
