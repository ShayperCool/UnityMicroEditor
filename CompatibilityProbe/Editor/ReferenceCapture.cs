using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using UnityEditor;
using UnityEditor.SceneManagement;
using UnityEngine;
using UnityEngine.SceneManagement;
using Object = UnityEngine.Object;

namespace UnityPlay.Compatibility
{
    /// <summary>Captures an authored scene and its real cold Editor Play Mode state.</summary>
    [InitializeOnLoad]
    public static class ReferenceCapture
    {
        private const string Prefix = "UnityPlay.Compatibility.Reference.";
        private static readonly List<ErrorRecord> errors = new List<ErrorRecord>();

        static ReferenceCapture()
        {
            RestoreErrors();
            EditorApplication.update += Update;
            EditorApplication.playModeStateChanged += PlayModeChanged;
            Application.logMessageReceived += LogMessage;
        }

        public static void RunFromCommandLine()
        {
            string reportPath = Environment.GetEnvironmentVariable("UNITY_PLAY_REFERENCE_REPORT");
            string scenePath = Environment.GetEnvironmentVariable("UNITY_PLAY_REFERENCE_SCENE");
            try
            {
                if (string.IsNullOrEmpty(reportPath) || !Path.IsPathRooted(reportPath))
                    throw new ArgumentException("UNITY_PLAY_REFERENCE_REPORT must be an absolute file path.");
                if (string.IsNullOrEmpty(scenePath) || !scenePath.StartsWith("Assets/", StringComparison.Ordinal)
                    || !scenePath.EndsWith(".unity", StringComparison.OrdinalIgnoreCase))
                    throw new ArgumentException("UNITY_PLAY_REFERENCE_SCENE must identify an authored Assets/...unity scene.");
                if (EditorApplication.isPlayingOrWillChangePlaymode) throw new InvalidOperationException("Reference capture requires an idle Editor.");
                SessionState.SetString(Prefix + "path", Path.GetFullPath(reportPath));
                SessionState.SetString(Prefix + "scene", scenePath);
                SessionState.SetBool(Prefix + "originalOptionsEnabled", EditorSettings.enterPlayModeOptionsEnabled);
                SessionState.SetInt(Prefix + "originalOptions", (int)EditorSettings.enterPlayModeOptions);
                SessionState.SetBool(Prefix + "optionsCaptured", true);
                SessionState.SetString(Prefix + "deadline", DateTime.UtcNow.AddSeconds(60).Ticks.ToString(CultureInfo.InvariantCulture));
                SessionState.SetInt(Prefix + "exitCode", 0);
                SessionState.SetString(Prefix + "phase", "authoring");
                errors.Clear();
                PersistErrors();
                Scene scene = EditorSceneManager.OpenScene(scenePath, OpenSceneMode.Single);
                var report = new Report
                {
                    schemaVersion = 1, engineVersion = Application.unityVersion, scenePath = scenePath,
                    profile = "cold-play-mode-domain-and-scene-reload",
                    enterPlayModeOptionsEnabled = false, effectiveDomainReload = true, effectiveSceneReload = true,
                    originalEnterPlayModeOptionsEnabled = EditorSettings.enterPlayModeOptionsEnabled,
                    originalEnterPlayModeOptions = (int)EditorSettings.enterPlayModeOptions,
                    batchMode = Application.isBatchMode, authoringScene = SceneSnapshot.Capture(scene),
                    assets = CaptureAssets(), startedUtc = DateTime.UtcNow.ToString("O"),
                };
                Save(report);
                // Disabling the options switch enables both normal reloads, regardless
                // of which optimization bits the source project previously selected.
                EditorSettings.enterPlayModeOptionsEnabled = false;
                CompatibilityLifecycleProbe.ResetEvents();
                // executeMethod can run before QuickSearch's startup delayCall.
                // Give normal Edit Mode startup two delayCall ticks to create
                // its default database before scene/domain reload begins.
                SessionState.SetString(Prefix + "phase", "waiting-for-editor");
                EditorApplication.delayCall += QueuePlayMode;
            }
            catch (Exception exception)
            {
                if (!string.IsNullOrEmpty(reportPath) && Path.IsPathRooted(reportPath))
                    SessionState.SetString(Prefix + "path", reportPath);
                Fail(exception);
            }
        }

        private static void QueuePlayMode()
        {
            EditorApplication.delayCall += EnterReferencePlayMode;
        }

        private static void EnterReferencePlayMode()
        {
            if (SessionState.GetString(Prefix + "phase", "") != "waiting-for-editor") return;
            SessionState.SetString(Prefix + "phase", "entering");
            EditorApplication.EnterPlaymode();
        }

        private static void Update()
        {
            string phase = SessionState.GetString(Prefix + "phase", "");
            if (string.IsNullOrEmpty(phase) || phase == "complete") return;
            try
            {
                string deadline = SessionState.GetString(Prefix + "deadline", "");
                if (long.TryParse(deadline, NumberStyles.Integer, CultureInfo.InvariantCulture, out long ticks)
                    && DateTime.UtcNow.Ticks > ticks) throw new TimeoutException("Editor reference Play Mode capture exceeded its 60 second deadline.");
                if ((phase == "entering" || phase == "playing") && EditorApplication.isPlaying)
                {
                    if (!SnapshotDriver.Ready) return;
                    Scene scene = SceneManager.GetActiveScene();
                    if (!scene.IsValid() || !scene.isLoaded) return;
                    var report = Read();
                    report.playScene = SnapshotDriver.CapturedScene;
                    report.lifecycle = SnapshotDriver.CapturedLifecycle;
                    report.runtimeErrors = errors.ToArray();
                    report.success = errors.Count == 0;
                    if (!report.success) report.failure = ErrorSummary();
                    report.finishedUtc = DateTime.UtcNow.ToString("O");
                    report.capturedEditorIsPlaying = EditorApplication.isPlaying;
                    report.capturedEnterPlayModeOptionsEnabled = EditorSettings.enterPlayModeOptionsEnabled;
                    Save(report);
                    SessionState.SetInt(Prefix + "exitCode", report.success ? 0 : 1);
                    SessionState.SetString(Prefix + "phase", "stopping");
                    EditorApplication.ExitPlaymode();
                }
                else if (phase == "stopping" && !EditorApplication.isPlayingOrWillChangePlaymode)
                    Finish();
            }
            catch (Exception exception) { Fail(exception); }
        }

        private static void PlayModeChanged(PlayModeStateChange state)
        {
            string phase = SessionState.GetString(Prefix + "phase", "");
            if (string.IsNullOrEmpty(phase) || phase == "complete") return;
            if (state == PlayModeStateChange.EnteredPlayMode && phase == "entering")
                SessionState.SetString(Prefix + "phase", "playing");
            if (state == PlayModeStateChange.EnteredEditMode && phase == "stopping")
                Finish();
        }

        private static void LogMessage(string message, string stack, LogType type)
        {
            string phase = SessionState.GetString(Prefix + "phase", "");
            if (phase != "authoring" && phase != "waiting-for-editor" && phase != "playing" && phase != "entering" && phase != "stopping") return;
            if (type != LogType.Error && type != LogType.Exception && type != LogType.Assert) return;
            if (errors.Count < 20)
            {
                errors.Add(new ErrorRecord { type = type.ToString(), message = message, stack = stack });
                PersistErrors();
            }
        }

        private static void RestoreErrors()
        {
            string json = SessionState.GetString(Prefix + "errors", "");
            if (string.IsNullOrEmpty(json)) return;
            var buffer = JsonUtility.FromJson<ErrorBuffer>(json);
            if (buffer != null && buffer.items != null) errors.AddRange(buffer.items);
        }

        private static void PersistErrors()
        { SessionState.SetString(Prefix + "errors", JsonUtility.ToJson(new ErrorBuffer { items = errors.ToArray() })); }

        private static string ErrorSummary()
        {
            if (errors.Count == 0) return "";
            return "Unity reported " + errors.Count + " error(s): " + errors[0].type + ": " + errors[0].message;
        }

        private static void Fail(Exception exception)
        {
            try
            {
                Report report = Read();
                report.success = false; report.failure = exception.ToString(); report.finishedUtc = DateTime.UtcNow.ToString("O");
                if (errors.Count != 0) report.runtimeErrors = errors.ToArray();
                Save(report);
            }
            catch (Exception writeFailure) { Debug.LogError("Reference report write failed: " + writeFailure.Message); }
            SessionState.SetInt(Prefix + "exitCode", 1);
            SessionState.SetString(Prefix + "phase", "stopping");
            if (EditorApplication.isPlayingOrWillChangePlaymode) EditorApplication.ExitPlaymode();
            else Finish();
        }

        private static void Finish()
        {
            if (SessionState.GetString(Prefix + "phase", "") == "complete") return;
            int exitCode = SessionState.GetInt(Prefix + "exitCode", 1);
            try
            {
                if (errors.Count != 0)
                {
                    exitCode = 1;
                    var report = Read();
                    report.success = false; report.runtimeErrors = errors.ToArray();
                    if (string.IsNullOrEmpty(report.failure)) report.failure = ErrorSummary();
                    Save(report);
                }
            }
            catch (Exception exception)
            { exitCode = 1; Debug.LogError("Reference capture finalization failed: " + exception); }
            finally
            {
                try
                {
                    if (SessionState.GetBool(Prefix + "optionsCaptured", false))
                    {
                        EditorSettings.enterPlayModeOptionsEnabled = SessionState.GetBool(Prefix + "originalOptionsEnabled", false);
                        EditorSettings.enterPlayModeOptions = (EnterPlayModeOptions)SessionState.GetInt(Prefix + "originalOptions", 0);
                    }
                }
                catch (Exception exception)
                { exitCode = 1; Debug.LogError("Reference Editor settings restoration failed: " + exception); }
                SessionState.SetString(Prefix + "phase", "complete");
                EditorApplication.Exit(exitCode);
            }
        }

        private static Report Read()
        {
            string json = SessionState.GetString(Prefix + "report", "");
            return string.IsNullOrEmpty(json) ? new Report { schemaVersion = 1, engineVersion = Application.unityVersion }
                : JsonUtility.FromJson<Report>(json);
        }

        private static void Save(Report report)
        {
            string json = SceneSnapshot.ToJson(report, true);
            // This internal journal is read by native JsonUtility after the Editor
            // domain reload. Preserve its Rect/Bounds field representation there;
            // the published artifact uses the shared observer writer on both sides.
            SessionState.SetString(Prefix + "report", JsonUtility.ToJson(report));
            string path = SessionState.GetString(Prefix + "path", "");
            if (string.IsNullOrEmpty(path)) throw new InvalidOperationException("Reference report path was not configured.");
            Directory.CreateDirectory(Path.GetDirectoryName(path));
            string temporary = path + ".tmp";
            File.WriteAllText(temporary, json + "\n");
            if (File.Exists(path)) File.Delete(path);
            File.Move(temporary, path);
        }

        private static AssetRecord[] CaptureAssets()
        {
            var paths = new List<string>();
            foreach (string guid in AssetDatabase.FindAssets("", new[] { "Assets" }))
            {
                string path = AssetDatabase.GUIDToAssetPath(guid);
                if (!AssetDatabase.IsValidFolder(path)) paths.Add(path);
            }
            paths.Sort(StringComparer.Ordinal);
            var result = new List<AssetRecord>();
            foreach (string path in paths)
            {
                Object main = AssetDatabase.LoadMainAssetAtPath(path);
                var record = new AssetRecord
                {
                    path = path, guid = AssetDatabase.AssetPathToGUID(path),
                    mainType = main != null ? main.GetType().FullName : "", name = main != null ? main.name : "",
                };
                if (main != null && AssetDatabase.TryGetGUIDAndLocalFileIdentifier(main, out string guid, out long localFileId))
                { record.guid = guid; record.localFileId = localFileId.ToString(CultureInfo.InvariantCulture); }
                AssetImporter importer = AssetImporter.GetAtPath(path);
                record.importerType = importer != null ? importer.GetType().FullName : "";
                if (importer is TextureImporter texture)
                    record.textureImporter = new TextureImporterRecord
                    {
                        textureType = (int)texture.textureType, textureShape = (int)texture.textureShape,
                        sRGBTexture = texture.sRGBTexture, mipmapEnabled = texture.mipmapEnabled, isReadable = texture.isReadable,
                        filterMode = (int)texture.filterMode, aniso = texture.anisoLevel, wrapU = (int)texture.wrapModeU, wrapV = (int)texture.wrapModeV,
                        alphaSource = (int)texture.alphaSource, sourceHasAlpha = texture.DoesSourceTextureHaveAlpha(),
                    };
                if (main is Material material)
                    record.material = new MaterialRecord { shaderName = material.shader != null ? material.shader.name : "", color = material.color, renderQueue = material.renderQueue };
                result.Add(record);
            }
            return result.ToArray();
        }

        [Serializable] public sealed class Report
        {
            public int schemaVersion;
            public string engineVersion;
            public string scenePath;
            public string profile;
            public bool enterPlayModeOptionsEnabled;
            public bool effectiveDomainReload;
            public bool effectiveSceneReload;
            public bool originalEnterPlayModeOptionsEnabled;
            public int originalEnterPlayModeOptions;
            public bool batchMode;
            public bool success;
            public bool capturedEditorIsPlaying;
            public bool capturedEnterPlayModeOptionsEnabled;
            public string failure;
            public string startedUtc;
            public string finishedUtc;
            public SceneSnapshot.Snapshot authoringScene;
            public SceneSnapshot.Snapshot playScene;
            public CompatibilityLifecycleProbe.Event[] lifecycle;
            public AssetRecord[] assets;
            public ErrorRecord[] runtimeErrors;
        }
        [Serializable] public sealed class ErrorRecord { public string type; public string message; public string stack; }
        [Serializable] private sealed class ErrorBuffer { public ErrorRecord[] items; }
        [Serializable] public sealed class AssetRecord
        {
            public string path;
            public string guid;
            public string localFileId;
            public string mainType;
            public string name;
            public string importerType;
            public TextureImporterRecord textureImporter;
            public MaterialRecord material;
        }
        [Serializable] public sealed class TextureImporterRecord
        {
            public int textureType;
            public int textureShape;
            public bool sRGBTexture;
            public bool mipmapEnabled;
            public bool isReadable;
            public int filterMode;
            public int aniso;
            public int wrapU;
            public int wrapV;
            public int alphaSource;
            public bool sourceHasAlpha;
        }
        [Serializable] public sealed class MaterialRecord { public string shaderName; public Color color; public int renderQueue; }
    }
}
