using System;
using System.Globalization;
using System.IO;

namespace UnityMicroEditor
{
    /// <summary>Arguments owned by the CLI observer. Unity's other player arguments are left alone.</summary>
    public sealed class CliPlayArguments
    {
        public string ReportPath { get; }
        public string ReadyPath { get; }
        public string ScreenshotPath { get; }
        public string ContentManifestPath { get; }
        public string RawManifestPath { get; }
        public string RawContentRoutesPath { get; }
        public string RawReloadRequestPath { get; }
        public string CodeReloadRequestPath { get; }
        public string ControlDirectory { get; }
        public int Frames { get; }
        public string Scene { get; }
        public int? TargetFrameRate { get; }
        public bool NoVsync { get; }
        public bool Interactive { get; }

        private CliPlayArguments(string reportPath, string readyPath, string screenshotPath, string contentManifestPath, string rawManifestPath, string rawReloadRequestPath, string rawContentRoutesPath,
            int frames, string scene, int? targetFrameRate, bool noVsync, bool interactive, string controlDirectory, string codeReloadRequestPath)
        {
            ReportPath = reportPath;
            ReadyPath = readyPath;
            ScreenshotPath = screenshotPath;
            ContentManifestPath = contentManifestPath;
            RawManifestPath = rawManifestPath;
            RawReloadRequestPath = rawReloadRequestPath;
            CodeReloadRequestPath = codeReloadRequestPath;
            ControlDirectory = controlDirectory;
            RawContentRoutesPath = rawContentRoutesPath;
            Frames = frames;
            Scene = scene;
            TargetFrameRate = targetFrameRate;
            NoVsync = noVsync;
            Interactive = interactive;
        }

        public static bool TryParse(string[] arguments, out CliPlayArguments result)
        {
            result = null;
            if (arguments == null) throw new ArgumentNullException(nameof(arguments));

            bool enabled = false;
            for (int index = 0; index < arguments.Length; index++)
            {
                if (arguments[index] == "--unity-cli-report") enabled = true;
            }
            if (!enabled) return false;

            string reportPath = null;
            string readyPath = null;
            string screenshotPath = null;
            string contentManifestPath = null;
            string rawManifestPath = null;
            string rawReloadRequestPath = null;
            string codeReloadRequestPath = null;
            string rawContentRoutesPath = null;
            string controlDirectory = null;
            string scene = null;
            int? targetFrameRate = null;
            bool noVsync = false;
            bool interactive = false;
            int frames = 0;
            bool hasScene = false;
            bool hasFrames = false;
            for (int index = 0; index < arguments.Length; index++)
            {
                string argument = arguments[index];
                if (argument == "--unity-cli-report")
                {
                    if (reportPath != null) throw new FormatException("Duplicate --unity-cli-report.");
                    reportPath = ReadAbsolutePath(arguments, ref index);
                }
                else if (argument == "--unity-cli-ready")
                {
                    if (readyPath != null) throw new FormatException("Duplicate --unity-cli-ready.");
                    readyPath = ReadAbsolutePath(arguments, ref index);
                }
                else if (argument == "--unity-cli-screenshot")
                {
                    if (screenshotPath != null) throw new FormatException("Duplicate --unity-cli-screenshot.");
                    screenshotPath = ReadAbsolutePath(arguments, ref index);
                    if (!screenshotPath.EndsWith(".png", StringComparison.OrdinalIgnoreCase))
                        throw new FormatException("--unity-cli-screenshot requires a .png file path.");
                }
                else if (argument == "--unity-cli-content")
                {
                    if (contentManifestPath != null) throw new FormatException("Duplicate --unity-cli-content.");
                    contentManifestPath = ReadAbsolutePath(arguments, ref index);
                }
                else if (argument == "--unity-cli-raw-manifest")
                {
                    if (rawManifestPath != null) throw new FormatException("Duplicate --unity-cli-raw-manifest.");
                    rawManifestPath = ReadAbsolutePath(arguments, ref index);
                }
                else if (argument == "--unity-cli-raw-reload")
                {
                    if (rawReloadRequestPath != null) throw new FormatException("Duplicate --unity-cli-raw-reload.");
                    rawReloadRequestPath = ReadAbsolutePath(arguments, ref index);
                }
                else if (argument == "--unity-cli-code-reload")
                {
                    if (codeReloadRequestPath != null) throw new FormatException("Duplicate --unity-cli-code-reload.");
                    codeReloadRequestPath = ReadAbsolutePath(arguments, ref index);
                }
                else if (argument == "--unity-cli-raw-routes")
                {
                    if (rawContentRoutesPath != null) throw new FormatException("Duplicate --unity-cli-raw-routes.");
                    rawContentRoutesPath = ReadAbsolutePath(arguments, ref index);
                }
                else if (argument == "--unity-cli-control")
                {
                    if (controlDirectory != null) throw new FormatException("Duplicate --unity-cli-control.");
                    controlDirectory = ReadAbsolutePath(arguments, ref index);
                }
                else if (argument == "--unity-cli-fps")
                {
                    if (targetFrameRate.HasValue) throw new FormatException("Duplicate --unity-cli-fps.");
                    string value = ReadValue(arguments, ref index, true);
                    if (!int.TryParse(value, NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture, out int rate) || (rate != -1 && rate <= 0))
                        throw new FormatException("--unity-cli-fps requires -1 for uncapped rendering or a positive integer.");
                    targetFrameRate = rate;
                }
                else if (argument == "--unity-cli-no-vsync")
                {
                    if (noVsync) throw new FormatException("Duplicate --unity-cli-no-vsync.");
                    noVsync = true;
                }
                else if (argument == "--unity-cli-interactive")
                {
                    if (interactive) throw new FormatException("Duplicate --unity-cli-interactive.");
                    interactive = true;
                }
                else if (argument == "--unity-cli-frames")
                {
                    if (hasFrames) throw new FormatException("Duplicate --unity-cli-frames.");
                    hasFrames = true;
                    string value = ReadValue(arguments, ref index);
                    if (!int.TryParse(value, NumberStyles.None, CultureInfo.InvariantCulture, out frames) || frames <= 0)
                        throw new FormatException("--unity-cli-frames requires a positive integer.");
                }
                else if (argument == "--unity-cli-scene")
                {
                    if (hasScene) throw new FormatException("Duplicate --unity-cli-scene.");
                    hasScene = true;
                    scene = ReadValue(arguments, ref index);
                }
                else if (argument != null && argument.StartsWith("--unity-cli-", StringComparison.Ordinal))
                {
                    throw new FormatException("Unknown CLI observer argument: " + argument);
                }
            }

            if (!hasFrames) throw new FormatException("--unity-cli-frames is required with --unity-cli-report.");
            if (rawManifestPath != null && (contentManifestPath != null || hasScene))
                throw new FormatException("A raw manifest owns scene selection; --unity-cli-content and --unity-cli-scene cannot be combined with it.");
            if (readyPath == null) readyPath = reportPath + ".ready.json";
            if (rawReloadRequestPath != null && (rawManifestPath == null || !interactive))
                throw new FormatException("--unity-cli-raw-reload requires an interactive raw manifest run.");
            if (rawReloadRequestPath != null && (rawReloadRequestPath == rawManifestPath || rawReloadRequestPath == reportPath
                || rawReloadRequestPath == readyPath || rawReloadRequestPath == screenshotPath || rawReloadRequestPath == contentManifestPath
                || rawReloadRequestPath + ".ack.json" == reportPath || rawReloadRequestPath + ".ack.json" == readyPath
                || rawReloadRequestPath + ".ack.json" == rawManifestPath || rawReloadRequestPath + ".ack.json" == contentManifestPath))
                throw new FormatException("Raw reload request/ack and source/report destinations must use distinct paths.");
            if (codeReloadRequestPath != null && (rawManifestPath == null || !interactive))
                throw new FormatException("--unity-cli-code-reload requires an interactive raw manifest run.");
            if (codeReloadRequestPath != null)
                foreach (string file in new[] { reportPath, readyPath, contentManifestPath, rawManifestPath, rawContentRoutesPath, rawReloadRequestPath, rawReloadRequestPath == null ? null : rawReloadRequestPath + ".ack.json" })
                    if (file != null && (file == codeReloadRequestPath || file == codeReloadRequestPath + ".ack.json"))
                        throw new FormatException("Code reload request/ack and source/report destinations must use distinct paths.");
            if (interactive && screenshotPath != null)
                throw new FormatException("--unity-cli-screenshot requires a finite run; capture during player shutdown is unavailable.");
            if (reportPath == readyPath || reportPath == screenshotPath || readyPath == screenshotPath ||
                (contentManifestPath != null && (contentManifestPath == reportPath || contentManifestPath == readyPath || contentManifestPath == screenshotPath)))
                throw new FormatException("Report, ready, screenshot and content manifest must use distinct file paths.");
            if (rawManifestPath == reportPath || rawManifestPath == readyPath || rawManifestPath == screenshotPath)
                throw new FormatException("Raw manifest and report destinations must use distinct file paths.");
            if (controlDirectory != null && (!interactive || rawManifestPath == null
                || Path.GetDirectoryName(controlDirectory) != Path.GetDirectoryName(reportPath)))
                throw new FormatException("--unity-cli-control requires an interactive raw run and a directory beside its report.");
            if (controlDirectory != null)
                foreach (string file in new[] { reportPath, readyPath, screenshotPath, contentManifestPath, rawManifestPath, rawReloadRequestPath, rawReloadRequestPath == null ? null : rawReloadRequestPath + ".ack.json", rawContentRoutesPath,
                    codeReloadRequestPath, codeReloadRequestPath == null ? null : codeReloadRequestPath + ".ack.json" })
                    if (file != null && file == controlDirectory)
                        throw new FormatException("Agent control directory and source/report destinations must use distinct paths.");
            result = new CliPlayArguments(reportPath, readyPath, screenshotPath, contentManifestPath, rawManifestPath, rawReloadRequestPath, rawContentRoutesPath, frames, scene, targetFrameRate, noVsync, interactive, controlDirectory, codeReloadRequestPath);
            return true;
        }

        internal static string FindReportPathForFailure(string[] arguments)
        {
            for (int index = 0; index + 1 < arguments.Length; index++)
            {
                if (arguments[index] == "--unity-cli-report" && Path.IsPathRooted(arguments[index + 1]))
                    return Path.GetFullPath(arguments[index + 1]);
            }
            return null;
        }

        private static string ReadAbsolutePath(string[] arguments, ref int index)
        {
            string flag = arguments[index];
            string path = ReadValue(arguments, ref index);
            if (!Path.IsPathRooted(path)) throw new FormatException(flag + " requires an absolute file path.");
            return Path.GetFullPath(path);
        }

        private static string ReadValue(string[] arguments, ref int index, bool allowNegative = false)
        {
            string flag = arguments[index];
            index++;
            if (index >= arguments.Length || string.IsNullOrWhiteSpace(arguments[index]) ||
                (arguments[index].StartsWith("-", StringComparison.Ordinal) && !(allowNegative && arguments[index] == "-1")))
                throw new FormatException("Missing value for " + flag + ".");
            return arguments[index];
        }
    }
}
