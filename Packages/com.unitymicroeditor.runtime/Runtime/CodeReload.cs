using System;
using System.Collections.Generic;
using System.IO;
using System.Reflection;
using System.Security.Cryptography;
using System.Text;
using UnityMicroEditor.Raw;
using UnityEngine;

namespace UnityMicroEditor
{
    // The bridge is pinned to its host build, so it carries only the transport of
    // a code patch: it loads the applier assembly a request names and hands the
    // request file to it. Descriptor, redirects and callbacks belong to the applier.
    internal sealed class CodeReload
    {
        private readonly string requestPath;
        private DateTime fileWriteTime;
        private long fileLength = -1;
        private string lastRequestId;
        private string applierSha256;
        private string applierError;
        private Func<string, string> handle;

        /// <summary>The code generation this Player runs; zero is the code it loaded.</summary>
        internal static long Generation { get; private set; }

        internal CodeReload(string requestPath) { this.requestPath = requestPath; Generation = 0; }

        /// <summary>
        /// Main thread, between two frames: the applier rewrites compiled game code. The caller
        /// owns the clock, which it shares with the poll of the scene reload request.
        /// </summary>
        internal void Poll()
        {
            string requestId = string.Empty, ack; long generation = 0;
            Func<string, string> entry;
            try
            {
                var file = new FileInfo(requestPath);
                if (!file.Exists || (file.LastWriteTimeUtc == fileWriteTime && file.Length == fileLength)) return;
                fileWriteTime = file.LastWriteTimeUtc; fileLength = file.Length;
                if (file.Length > 65536) throw new InvalidDataException("Code reload request exceeds the size limit.");
                var request = RawJson.Parse(File.ReadAllText(file.FullName)) as Dictionary<string, object>;
                if (request == null || !(Member(request, "requestId") is string id) || id.Length == 0 || id.Length > 128)
                    throw new InvalidDataException("Code reload request identity is invalid.");
                if (id == lastRequestId) return;
                lastRequestId = requestId = id;
                if (Member(request, "generation") is long number) generation = number;
                if (!(Member(request, "schemaVersion") is long schema) || schema != 1 || !(Member(request, "applier") is string applier)
                    || !Path.IsPathRooted(applier) || !(Member(request, "applierSha256") is string hash) || hash.Length != 64)
                    throw new InvalidDataException("Code reload request schema or applier is invalid.");
                if (applierSha256 != null && applierSha256 != hash)
                {
                    // A second applier would start without the redirects of the first,
                    // and patches bind to the first assembly loaded under its name.
                    // Only another Player process takes another applier.
                    Publish(requestId, Rejection(requestId, generation, "This Player runs code reload applier " + applierSha256 + " and cannot take applier " + hash
                        + ": one Player process loads one applier. Restart the Player to patch code with the new one.", true, "applier"));
                    return;
                }
                entry = Bind(Path.GetFullPath(applier), hash);
            }
            catch (Exception exception)
            {
                // Nothing ran: the Player keeps the generation it had.
                Publish(requestId, Rejection(requestId, generation, exception.Message, false));
                return;
            }
            try
            {
                ack = entry(requestPath);
                Dictionary<string, object> reply = null;
                try { reply = RawJson.Parse(ack ?? string.Empty) as Dictionary<string, object>; }
                catch (InvalidDataException) { }
                if (reply == null || !(Member(reply, "appliedGeneration") is long applied)) throw new InvalidDataException("The code reload applier returned no ack.");
                Generation = applied;
            }
            catch (Exception exception)
            {
                // The applier answers its own failures. One that escapes it may
                // have left game code half redirected.
                ack = Rejection(requestId, generation, exception.Message, true);
            }
            Publish(requestId, ack);
        }

        private Func<string, string> Bind(string path, string sha256)
        {
            if (applierSha256 != null)
            {
                if (handle == null) throw new InvalidOperationException(applierError);
                return handle;
            }
            var file = new FileInfo(path);
            if (!file.Exists || file.Length > 16 * 1024 * 1024) throw new InvalidDataException("Code reload applier is missing or exceeds the size limit.");
            byte[] bytes = File.ReadAllBytes(path);
            var text = new StringBuilder(64);
            using (SHA256 hash = SHA256.Create())
                foreach (byte value in hash.ComputeHash(bytes)) text.Append(value.ToString("x2"));
            if (text.ToString() != sha256) throw new InvalidDataException("Code reload applier differs from its request hash.");
            Assembly assembly = Assembly.Load(bytes);
            applierSha256 = sha256;
            try
            {
                MethodInfo method = assembly.GetType("UnityMicroEditor.HotReload.Entry", true).GetMethod("Handle", BindingFlags.Public | BindingFlags.Static, null, new[] { typeof(string) }, null);
                if (method == null || method.ReturnType != typeof(string)) throw new MissingMethodException("UnityMicroEditor.HotReload.Entry", "Handle");
                handle = (Func<string, string>)Delegate.CreateDelegate(typeof(Func<string, string>), method);
            }
            catch (Exception exception)
            {
                applierError = "Code reload applier has no usable entry point: " + exception.Message;
                throw new InvalidOperationException(applierError);
            }
            return handle;
        }

        private void Publish(string requestId, string ack)
        {
            try
            {
                CliPlayArtifact.WriteText(requestPath + ".ack.json", ack);
                Debug.Log("[unity-cli-play] code-reload-ack request=" + requestId + " generation=" + Generation);
            }
            catch (Exception exception)
            {
                Debug.LogWarning("[unity-cli-play] code-reload-ack-error " + exception.Message);
            }
        }

        // A stage names a rejection the runner acts on by its kind, in the form of the applier's failures.
        private static string Rejection(string requestId, long generation, string message, bool restartRequired, string stage = null)
        {
            Debug.LogWarning("[unity-cli-play] code-reload-rejected " + message);
            var failures = new List<object>();
            if (stage != null) failures.Add(new Dictionary<string, object> { ["assembly"] = string.Empty, ["member"] = string.Empty, ["stage"] = stage, ["message"] = message });
            return RawJson.Write(new Dictionary<string, object>
            {
                ["schemaVersion"] = 1, ["requestId"] = requestId, ["pid"] = System.Diagnostics.Process.GetCurrentProcess().Id,
                ["success"] = false, ["generation"] = generation, ["appliedGeneration"] = Generation,
                ["restartRequired"] = restartRequired, ["error"] = message, ["failures"] = failures,
                ["detoursApplied"] = 0, ["detoursRestored"] = 0, ["callbacksInvoked"] = 0, ["callbackErrors"] = 0, ["inliningDisabled"] = false,
                ["elapsedSeconds"] = 0, ["completedUtcUnixMilliseconds"] = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds(),
            }) + "\n";
        }

        private static object Member(Dictionary<string, object> map, string name)
        {
            return map.TryGetValue(name, out object value) ? value : null;
        }
    }
}
