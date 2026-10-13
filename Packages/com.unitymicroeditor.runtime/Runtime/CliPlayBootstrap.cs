using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using UnityEngine;
using UnityEngine.LowLevel;
using UnityEngine.SceneManagement;

namespace UnityMicroEditor
{
    public static class CliPlayBootstrap
    {
        [RuntimeInitializeOnLoadMethod(RuntimeInitializeLoadType.BeforeSceneLoad)]
        private static void Initialize()
        {
            if (Application.isEditor) return;
            long bootstrapUtc = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
            double bootstrapRealtime = Time.realtimeSinceStartupAsDouble;
            string[] arguments = Environment.GetCommandLineArgs();
            CliPlayArguments options;
            try
            {
                if (!CliPlayArguments.TryParse(arguments, out options)) return;
            }
            catch (Exception exception)
            {
                Debug.LogError("[unity-cli-play] argument-error " + exception.Message);
                try
                {
                    string path = CliPlayArguments.FindReportPathForFailure(arguments);
                    if (!string.IsNullOrEmpty(path))
                    {
                        CliPlayReport report = CliPlayReport.Create();
                        report.bootstrapUtcUnixMilliseconds = bootstrapUtc;
                        report.bootstrapRealtimeSeconds = bootstrapRealtime;
                        report.exitCode = 2;
                        report.fatalError = exception.Message;
                        report.Write(path);
                    }
                }
                catch (Exception writeError)
                {
                    Debug.LogError("[unity-cli-play] report-write-error " + writeError.Message);
                }
                Application.Quit(2);
                return;
            }
            new Session(options, bootstrapUtc, bootstrapRealtime).Start();
        }

        private sealed class Session
        {
            private sealed class FrameMarker { }
            private sealed class RawLoadMarker { }

            private readonly CliPlayArguments options;
            private readonly object logLock = new object();
            private readonly List<LogReport> logs = new List<LogReport>(32);
            private readonly double startedAt;
            private readonly long bootstrapUtc;
            private readonly int previousTargetFrameRate;
            private readonly int previousVsync;
            private int frames;
            private int errors;
            private int exceptions;
            private int sceneIndex = -1;
            private AsyncOperation sceneLoad;
            private CliPlayContent content;
            private bool loadRequested;
            private bool rawLoaded;
            private UnityMicroEditor.Raw.RawSceneLoader.LiveSession rawSession;
            private AgentControl agentControl;
            private CodeReload codeReload;
            private DateTime reloadFileWriteTime;
            private long reloadFileLength = -1;
            private double nextReloadPoll;
            private CliPlayRawReloadRequest pendingReload;
            private double reloadStartedAt;
            private int reloadErrors;
            private int reloadExceptions;
            private string lastReloadRequestId;
            private bool finished;
            private long readyUtc;
            private double readyAt;
            private long completedUtc;
            private double completedAt;
            private bool screenshotWritten;
            private int screenshotWidth;
            private int screenshotHeight;
            private double screenshotCaptureSeconds;

            internal Session(CliPlayArguments options, long bootstrapUtc, double bootstrapRealtime)
            {
                this.options = options;
                this.bootstrapUtc = bootstrapUtc;
                startedAt = bootstrapRealtime;
                previousTargetFrameRate = Application.targetFrameRate;
                previousVsync = QualitySettings.vSyncCount;
            }

            internal void Start()
            {
                Application.logMessageReceivedThreaded += OnLog;
                Application.quitting += OnQuitting;
                try
                {
                    if (!string.IsNullOrEmpty(options.ControlDirectory))
                        agentControl = new AgentControl(options.ControlDirectory, () => rawSession);
                    if (!string.IsNullOrEmpty(options.CodeReloadRequestPath)) codeReload = new CodeReload(options.CodeReloadRequestPath);
                    if (options.TargetFrameRate.HasValue) Application.targetFrameRate = options.TargetFrameRate.Value;
                    if (options.NoVsync) QualitySettings.vSyncCount = 0;
                    if (string.IsNullOrEmpty(options.RawManifestPath) && !string.IsNullOrEmpty(options.ContentManifestPath))
                        content = CliPlayContent.Open(options.ContentManifestPath, options.Scene);
                    else if (string.IsNullOrEmpty(options.RawManifestPath) && !string.IsNullOrEmpty(options.Scene))
                        sceneIndex = ResolveScene(options.Scene);
                    PlayerLoopSystem loop = PlayerLoop.GetCurrentPlayerLoop();
                    FrameLoop.Remove(ref loop, typeof(FrameMarker));
                    FrameLoop.Remove(ref loop, typeof(RawLoadMarker));
                    if (!string.IsNullOrEmpty(options.RawManifestPath)
                        && !FrameLoop.AppendToStage(ref loop, typeof(UnityEngine.PlayerLoop.EarlyUpdate), typeof(RawLoadMarker), LoadRaw))
                        throw new InvalidOperationException("The current PlayerLoop has no EarlyUpdate stage.");
                    if (!FrameLoop.AppendEndOfFrame(ref loop, typeof(FrameMarker), Tick))
                        throw new InvalidOperationException("The current PlayerLoop has no PostLateUpdate stage.");
                    PlayerLoop.SetPlayerLoop(loop);
                    Debug.Log("[unity-cli-play] bootstrap utcUnixMilliseconds=" + bootstrapUtc + " frames=" + options.Frames + " report=" + options.ReportPath);
                }
                catch (Exception exception)
                {
                    Finish(exception.Message, true);
                }
            }

            private void LoadRaw()
            {
                if (finished) return;
                try
                {
                    if (rawLoaded)
                    {
                        // One clock for both request files, and code first: a scene
                        // request of the same edit is then taken in the same frame and
                        // meets the new code. A pending scene reload counts every error
                        // logged until its ack, so callbacks of a code patch wait for it.
                        double now = Time.realtimeSinceStartupAsDouble;
                        if (now < nextReloadPoll) return;
                        nextReloadPoll = now + .025;
                        if (codeReload != null && pendingReload == null) codeReload.Poll();
                        PollRawReload();
                        return;
                    }
                    // Bootstrap scene is loaded, but the first render has not
                    // constructed and cached the host's URP global settings.
                    rawLoaded = true;
                    rawSession = UnityMicroEditor.Raw.RawSceneLoader.OpenSession(options.RawManifestPath, options.RawContentRoutesPath);
                }
                catch (Exception exception)
                {
                    Debug.LogException(exception);
                    Finish(exception.Message, true);
                }
            }

            private void PollRawReload()
            {
                if (pendingReload != null || rawSession == null || string.IsNullOrEmpty(options.RawReloadRequestPath)) return;
                double now = Time.realtimeSinceStartupAsDouble;
                CliPlayRawReloadRequest request = null;
                try
                {
                    var file = new FileInfo(options.RawReloadRequestPath);
                    if (!file.Exists || (file.LastWriteTimeUtc == reloadFileWriteTime && file.Length == reloadFileLength)) return;
                    reloadFileWriteTime = file.LastWriteTimeUtc; reloadFileLength = file.Length;
                    if (file.Length > 65536) throw new InvalidDataException("Raw reload request exceeds the size limit.");
                    request = JsonUtility.FromJson<CliPlayRawReloadRequest>(File.ReadAllText(file.FullName));
                    if (request == null || request.schemaVersion != 1 || string.IsNullOrEmpty(request.requestId)
                        || request.requestId.Length > 128 || string.IsNullOrEmpty(request.manifest) || !Path.IsPathRooted(request.manifest)
                        || string.IsNullOrEmpty(request.manifestSha256) || request.manifestSha256.Length != 64
                        || string.IsNullOrEmpty(request.revision) || request.revision != new DirectoryInfo(Path.GetDirectoryName(Path.GetFullPath(request.manifest))).Name)
                        throw new InvalidDataException("Raw reload request identity or schema is invalid.");
                    if (request.requestId == lastReloadRequestId) return;
                    lastReloadRequestId = request.requestId;
                    pendingReload = request;
                    reloadStartedAt = Time.realtimeSinceStartupAsDouble;
                    lock (logLock) { reloadErrors = errors; reloadExceptions = exceptions; }
                    if (!string.IsNullOrEmpty(request.patch))
                    {
                        if (!Path.IsPathRooted(request.patch) || request.patchSha256 == null || request.patchSha256.Length != 64)
                            throw new InvalidDataException("Raw patch path or hash is invalid.");
                        rawSession.ApplyPatch(Path.GetFullPath(request.patch), request.patchSha256, request.revision);
                    }
                    else rawSession.Reload(Path.GetFullPath(request.manifest), request.manifestSha256);
                }
                catch (Exception exception)
                {
                    if (pendingReload == null)
                    {
                        pendingReload = request ?? new CliPlayRawReloadRequest { requestId = string.Empty, manifest = string.Empty };
                        reloadStartedAt = now;
                        lock (logLock) { reloadErrors = errors; reloadExceptions = exceptions; }
                    }
                    // A rejected revision is reported over its own ack channel;
                    // validation/creation rejection preserves the old graph;
                    // an activation failure explicitly requests a restart.
                    Debug.LogWarning("[unity-cli-play] raw-reload-rejected " + exception.Message);
                    CompleteRawReload(exception.Message);
                }
            }

            private void CompleteRawReload(string failure)
            {
                if (string.IsNullOrEmpty(failure)) rawSession.ReleaseRetiredNativeFiles();
                CliPlayRawReloadRequest request = pendingReload;
                if (request == null) return;
                pendingReload = null;
                int newErrors, newExceptions;
                lock (logLock) { newErrors = errors - reloadErrors; newExceptions = exceptions - reloadExceptions; }
                var report = new CliPlayRawReloadReport
                {
                    requestId = request.requestId ?? string.Empty,
                    manifest = request.manifest ?? string.Empty,
                    revision = string.IsNullOrEmpty(request.revision) ? rawSession.Revision : request.revision,
                    loadedRevision = rawSession.Revision,
                    restartRequired = rawSession.LastReloadRestartRequired || newErrors > 0 || newExceptions > 0,
                    pid = System.Diagnostics.Process.GetCurrentProcess().Id,
                    success = string.IsNullOrEmpty(failure) && newErrors == 0 && newExceptions == 0,
                    elapsedSeconds = Time.realtimeSinceStartupAsDouble - reloadStartedAt,
                    completedUtcUnixMilliseconds = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds(),
                    requestedUtcUnixMilliseconds = request.requestedUtcUnixMilliseconds,
                    sourceChangedUtcUnixMilliseconds = request.sourceChangedUtcUnixMilliseconds,
                    errors = newErrors, exceptions = newExceptions,
                    fatalError = failure ?? string.Empty,
                    scene = SceneReport.Capture(SceneManager.GetActiveScene()),
                    reloadCount = rawSession.ReloadCount,
                    nativeLoadCount = rawSession.NativeLoadCount,
                    nativeObjectCount = rawSession.NativeObjectCount,
                    nativeIdentityDigest = rawSession.NativeIdentityDigest,
                };
                report.Write(options.RawReloadRequestPath + ".ack.json");
                Debug.Log("[unity-cli-play] raw-reload-ack request=" + report.requestId + " success=" + report.success + " seconds=" + report.elapsedSeconds);
            }

            private void Tick()
            {
                if (finished) return;
                try
                {
                    if (!string.IsNullOrEmpty(options.RawManifestPath) && !rawLoaded) return;
                    Scene active = SceneManager.GetActiveScene();
                    if (content != null)
                    {
                        if (!loadRequested)
                        {
                            loadRequested = true;
                            sceneLoad = content.LoadScene();
                            if (sceneLoad == null) throw new InvalidOperationException("Unity did not start the external content scene load.");
                            return;
                        }
                        if (!sceneLoad.isDone) return;
                        if (!content.Matches(active))
                            throw new InvalidOperationException("The external content scene finished loading but is not active.");
                    }
                    else if (sceneIndex >= 0 && active.buildIndex != sceneIndex)
                    {
                        if (!loadRequested)
                        {
                            loadRequested = true;
                            sceneLoad = SceneManager.LoadSceneAsync(sceneIndex, LoadSceneMode.Single);
                            if (sceneLoad == null) throw new InvalidOperationException("Unity did not start the requested scene load.");
                        }
                        else if (sceneLoad.isDone)
                        {
                            throw new InvalidOperationException("The requested scene finished loading but is not active.");
                        }
                        return;
                    }
                    if (!active.IsValid() || !active.isLoaded || (sceneLoad != null && !sceneLoad.isDone)) return;
                    if (pendingReload != null && (rawSession.PreviousSceneUnload == null || rawSession.PreviousSceneUnload.isDone))
                        CompleteRawReload(null);
                    frames++;
                    if (frames == 1) WriteReady(active);
                    if (agentControl != null) agentControl.Poll();
                    if (!options.Interactive && frames >= options.Frames)
                    {
                        completedAt = Time.realtimeSinceStartupAsDouble;
                        completedUtc = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
                        Finish(null, true);
                    }
                }
                catch (Exception exception)
                {
                    Finish(exception.Message, true);
                }
            }

            private void OnLog(string message, string stackTrace, LogType type)
            {
                if (type != LogType.Error && type != LogType.Assert && type != LogType.Exception) return;
                lock (logLock)
                {
                    if (finished) return;
                    if (type == LogType.Exception) exceptions++;
                    else errors++;
                    if (logs.Count < 32)
                        logs.Add(new LogReport { type = type.ToString(), message = message, stackTrace = stackTrace });
                }
            }

            private void WriteReady(Scene active)
            {
                readyAt = Time.realtimeSinceStartupAsDouble;
                readyUtc = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
                var ready = new CliPlayReadyReport
                {
                    bootstrapUtcUnixMilliseconds = bootstrapUtc,
                    bootstrapRealtimeSeconds = startedAt,
                    readyUtcUnixMilliseconds = readyUtc,
                    readyRealtimeSeconds = readyAt,
                    elapsedToReadySeconds = readyAt - startedAt,
                    engineVersion = Application.unityVersion,
                    scene = SceneReport.Capture(active),
                    contentManifestPath = options.ContentManifestPath ?? string.Empty,
                    contentRevisionManifestPath = content == null ? string.Empty : content.ManifestPath,
                    contentScenePath = content == null ? string.Empty : content.ScenePath,
                    contentRevision = content == null ? string.Empty : content.Revision,
                    targetFrameRate = Application.targetFrameRate,
                    vSyncCount = QualitySettings.vSyncCount,
                    graphicsDeviceType = SystemInfo.graphicsDeviceType.ToString(),
                    graphicsDeviceName = SystemInfo.graphicsDeviceName,
                    graphicsDeviceVendor = SystemInfo.graphicsDeviceVendor,
                    graphicsDeviceID = SystemInfo.graphicsDeviceID,
                    graphicsDeviceVendorID = SystemInfo.graphicsDeviceVendorID,
                    graphicsDeviceVersion = SystemInfo.graphicsDeviceVersion,
                    graphicsMemorySizeMB = SystemInfo.graphicsMemorySize,
                    rawReloadRequestPath = options.RawReloadRequestPath ?? string.Empty,
                    controlDirectory = options.ControlDirectory ?? string.Empty,
                    pid = System.Diagnostics.Process.GetCurrentProcess().Id,
                    rawReloadCount = rawSession == null ? 0 : rawSession.ReloadCount,
                    nativeLoadCount = rawSession == null ? 0 : rawSession.NativeLoadCount,
                    nativeObjectCount = rawSession == null ? 0 : rawSession.NativeObjectCount,
                    nativeIdentityDigest = rawSession == null ? string.Empty : rawSession.NativeIdentityDigest,
                };
                lock (logLock)
                {
                    ready.errors = errors;
                    ready.exceptions = exceptions;
                }
                ready.Write(options.ReadyPath);
                Debug.Log("[unity-cli-play] ready utcUnixMilliseconds=" + readyUtc + " scene=" + active.path + " report=" + options.ReadyPath);
            }

            private void OnQuitting()
            {
                if (finished) return;
                if (options.Interactive)
                {
                    completedAt = Time.realtimeSinceStartupAsDouble;
                    completedUtc = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
                    Finish(null, false);
                }
                else
                {
                    Finish("Player exited before the requested frames completed.", false);
                }
            }

            private void Finish(string failure, bool quit)
            {
                lock (logLock)
                {
                    if (finished) return;
                    finished = true;
                }

                if (string.IsNullOrEmpty(failure) && !string.IsNullOrEmpty(options.ScreenshotPath))
                {
                    try
                    {
                        CaptureScreenshot();
                    }
                    catch (Exception exception)
                    {
                        failure = "Screenshot capture failed: " + exception.Message;
                        Debug.LogError("[unity-cli-play] screenshot-error " + exception.Message);
                    }
                }
                int exitCode = !string.IsNullOrEmpty(failure) ? 2 : errors + exceptions > 0 ? 1 : 0;
                try
                {
                    CliPlayReport report = CliPlayReport.Create();
                    report.exitCode = exitCode;
                    report.success = exitCode == 0;
                    report.requestedScene = options.Scene ?? string.Empty;
                    report.controlDirectory = options.ControlDirectory ?? string.Empty;
                    report.frames = frames;
                    report.requestedFrames = options.Frames;
                    report.elapsedSeconds = (completedAt > 0 ? completedAt : Time.realtimeSinceStartupAsDouble) - startedAt;
                    report.bootstrapUtcUnixMilliseconds = bootstrapUtc;
                    report.bootstrapRealtimeSeconds = startedAt;
                    report.readyUtcUnixMilliseconds = readyUtc;
                    report.readyRealtimeSeconds = readyAt;
                    report.elapsedToReadySeconds = readyUtc == 0 ? 0 : readyAt - startedAt;
                    report.completedFramesUtcUnixMilliseconds = completedUtc;
                    report.completedFramesRealtimeSeconds = completedAt;
                    report.afterReadyElapsedSeconds = completedAt > 0 && readyUtc != 0 ? completedAt - readyAt : 0;
                    report.observedFramesPerSecond = frames > 1 && report.afterReadyElapsedSeconds > 0 ? (frames - 1) / report.afterReadyElapsedSeconds : 0;
                    report.readyReportPath = options.ReadyPath;
                    report.contentManifestPath = options.ContentManifestPath ?? string.Empty;
                    report.contentRevisionManifestPath = content == null ? string.Empty : content.ManifestPath;
                    report.contentScenePath = content == null ? string.Empty : content.ScenePath;
                    report.contentRevision = content == null ? string.Empty : content.Revision;
                    report.rawReloadRequestPath = options.RawReloadRequestPath ?? string.Empty;
                    report.pid = System.Diagnostics.Process.GetCurrentProcess().Id;
                    report.rawReloadCount = rawSession == null ? 0 : rawSession.ReloadCount;
                    report.nativeLoadCount = rawSession == null ? 0 : rawSession.NativeLoadCount;
                    report.nativeObjectCount = rawSession == null ? 0 : rawSession.NativeObjectCount;
                    report.nativeIdentityDigest = rawSession == null ? string.Empty : rawSession.NativeIdentityDigest;
                    report.noVsyncRequested = options.NoVsync;
                    report.screenshotPath = options.ScreenshotPath ?? string.Empty;
                    report.screenshotWritten = screenshotWritten;
                    report.screenshotWidth = screenshotWidth;
                    report.screenshotHeight = screenshotHeight;
                    report.screenshotCaptureSeconds = screenshotCaptureSeconds;
                    report.errors = errors;
                    report.exceptions = exceptions;
                    report.fatalError = failure ?? string.Empty;
                    lock (logLock) report.capturedLogs = logs.ToArray();
                    report.Write(options.ReportPath);
                    Debug.Log("[unity-cli-play] report-written frames=" + frames + " errors=" + errors + " exceptions=" + exceptions + " exit=" + exitCode);
                }
                catch (Exception exception)
                {
                    exitCode = 2;
                    Debug.LogError("[unity-cli-play] report-write-error " + exception.Message);
                }
                finally
                {
                    Application.logMessageReceivedThreaded -= OnLog;
                    Application.quitting -= OnQuitting;
                    try
                    {
                        try
                        {
                            if (agentControl != null) agentControl.Dispose();
                            if (content != null) content.Dispose();
                            if (rawSession != null) rawSession.Dispose();
                        }
                        finally
                        {
                            try
                            {
                                Application.targetFrameRate = previousTargetFrameRate;
                                QualitySettings.vSyncCount = previousVsync;
                            }
                            finally
                            {
                                PlayerLoopSystem loop = PlayerLoop.GetCurrentPlayerLoop();
                                bool changed = FrameLoop.Remove(ref loop, typeof(FrameMarker));
                                changed = FrameLoop.Remove(ref loop, typeof(RawLoadMarker)) || changed;
                                if (changed) PlayerLoop.SetPlayerLoop(loop);
                            }
                        }
                    }
                    catch (Exception exception)
                    {
                        exitCode = 2;
                        Debug.LogError("[unity-cli-play] loop-cleanup-error " + exception.Message);
                    }
                }
                if (quit) Application.Quit(exitCode);
            }

            private void CaptureScreenshot()
            {
                if (SystemInfo.graphicsDeviceType == UnityEngine.Rendering.GraphicsDeviceType.Null)
                    throw new InvalidOperationException("A screenshot requires an active graphics device; omit -nographics.");
                double captureStartedAt = Time.realtimeSinceStartupAsDouble;
                Texture2D texture = null;
                try
                {
                    texture = ScreenCapture.CaptureScreenshotAsTexture(1);
                    if (texture == null || texture.width <= 0 || texture.height <= 0)
                        throw new InvalidOperationException("Unity returned no rendered screenshot texture.");
                    byte[] png = ImageConversion.EncodeToPNG(texture);
                    if (png == null || png.Length == 0) throw new IOException("Unity could not encode the screenshot as PNG.");
                    CliPlayArtifact.WriteBytes(options.ScreenshotPath, png);
                    screenshotWidth = texture.width;
                    screenshotHeight = texture.height;
                    screenshotWritten = true;
                    Debug.Log("[unity-cli-play] screenshot-written " + options.ScreenshotPath);
                }
                finally
                {
                    if (texture != null) UnityEngine.Object.Destroy(texture);
                    screenshotCaptureSeconds = Time.realtimeSinceStartupAsDouble - captureStartedAt;
                }
            }

            private static int ResolveScene(string selector)
            {
                int count = SceneManager.sceneCountInBuildSettings;
                if (int.TryParse(selector, NumberStyles.None, CultureInfo.InvariantCulture, out int index))
                {
                    if (index >= count) throw new ArgumentException("Scene index is outside this player's build scenes: " + selector);
                    return index;
                }

                int match = -1;
                for (int candidate = 0; candidate < count; candidate++)
                {
                    string path = SceneUtility.GetScenePathByBuildIndex(candidate);
                    if (!string.Equals(path, selector, StringComparison.Ordinal) &&
                        !string.Equals(Path.GetFileNameWithoutExtension(path), selector, StringComparison.Ordinal)) continue;
                    if (match >= 0) throw new ArgumentException("Scene name is ambiguous. Use its build index or asset path: " + selector);
                    match = candidate;
                }
                if (match < 0) throw new ArgumentException("Scene is not included in this player: " + selector);
                return match;
            }
        }
    }
}
