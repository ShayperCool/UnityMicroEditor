using System;
using System.IO;
using System.Text;
using System.Globalization;
using System.Text.RegularExpressions;
using UnityEditor;
using UnityEngine;
using Object = UnityEngine.Object;

namespace Dungeon.CLIPlay.ImportedAssets.Editor
{
    /// <summary>Read-only observation of serialized curve enums versus the public API.</summary>
    public static class CurveWrapProbe
    {
        private sealed class Carrier : ScriptableObject
        {
            public AnimationCurve curve = AnimationCurve.Linear(0f, 0f, 1f, 1f);
        }

        [Serializable] private sealed class Observation
        {
            public string requestedName;
            public int requestedPublicCode;
            public int actualPublicPreWrapMode;
            public int actualPublicPostWrapMode;
            public int serializedPreInfinity;
            public int serializedPostInfinity;
            public string serializedJson;
        }

        [Serializable] private sealed class Report
        {
            public int schemaVersion = 1;
            public bool success;
            public string engineVersion = Application.unityVersion;
            public Observation[] curveWrapModes;
            public string failure = "";
        }

        public static void RunFromCommandLine()
        {
            var report = new Report();
            string path = Environment.GetEnvironmentVariable("UNITY_PLAY_CURVE_PROBE_REPORT");
            Carrier carrier = null;
            try
            {
                if (!Application.isBatchMode || string.IsNullOrWhiteSpace(path) || !Path.IsPathRooted(path))
                    throw new ArgumentException("Run the probe in batchmode with absolute UNITY_PLAY_CURVE_PROBE_REPORT.");
                carrier = ScriptableObject.CreateInstance<Carrier>();
                carrier.hideFlags = HideFlags.HideAndDontSave;
                WrapMode[] requested = { WrapMode.Default, WrapMode.Once, WrapMode.Loop, WrapMode.PingPong, WrapMode.ClampForever };
                report.curveWrapModes = new Observation[requested.Length];
                for (int index = 0; index < requested.Length; index++)
                {
                    carrier.curve.preWrapMode = requested[index];
                    carrier.curve.postWrapMode = requested[index];
                    string serialized = EditorJsonUtility.ToJson(carrier);
                    report.curveWrapModes[index] = new Observation
                    {
                        requestedName = requested[index].ToString(), requestedPublicCode = (int)requested[index],
                        actualPublicPreWrapMode = (int)carrier.curve.preWrapMode, actualPublicPostWrapMode = (int)carrier.curve.postWrapMode,
                        serializedPreInfinity = Infinity(serialized, "m_PreInfinity"), serializedPostInfinity = Infinity(serialized, "m_PostInfinity"), serializedJson = serialized,
                    };
                }
                report.success = true;
            }
            catch (Exception error) { report.failure = error.ToString(); Debug.LogError(report.failure); }
            finally { if (carrier != null) Object.DestroyImmediate(carrier); }
            if (!string.IsNullOrWhiteSpace(path) && Path.IsPathRooted(path))
            {
                Directory.CreateDirectory(Path.GetDirectoryName(path));
                File.WriteAllText(path, JsonUtility.ToJson(report, true) + "\n", new UTF8Encoding(false));
            }
            EditorApplication.Exit(report.success ? 0 : 1);
        }

        private static int Infinity(string json, string field)
        {
            MatchCollection matches = Regex.Matches(json, "\\\"" + Regex.Escape(field) + "\\\"\\s*:\\s*(-?[0-9]+)");
            if (matches.Count != 1 || !int.TryParse(matches[0].Groups[1].Value, NumberStyles.Integer, CultureInfo.InvariantCulture, out int value))
                throw new InvalidOperationException("Native curve JSON has no unique " + field + ": " + json);
            return value;
        }
    }
}
