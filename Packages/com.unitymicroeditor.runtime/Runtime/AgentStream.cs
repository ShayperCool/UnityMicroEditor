using System;
using System.Collections;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Net;
using System.Net.Sockets;
using System.Reflection;
using System.Text;
using System.Threading;
using Unity.Collections;
using UnityEngine;
using UnityEngine.Rendering;
using UnityMicroEditor.Raw;

namespace UnityMicroEditor
{
    /// <summary>
    /// Sends the viewport as raw frames to the tool's remote session server over a loopback
    /// connection the Player opens, and applies the input that comes back through
    /// <see cref="AgentInput"/>. The server encodes the video. Capture and readback use
    /// public APIs on the main thread; socket I/O runs on the link's own threads.
    /// </summary>
    internal sealed class AgentStream : IDisposable
    {
        private const int MaximumQueuedLines = 1024, MaximumSide = 4096;
        private readonly AgentInput input;
        private readonly Func<AgentCaptureRunner> runner;
        private readonly int pid = System.Diagnostics.Process.GetCurrentProcess().Id;
        private Link link;
        private Coroutine loop;
        private AgentCaptureRunner loopOwner;
        private RenderTexture screenTarget, scaledTarget;
        private Texture2D readbackTexture;
        private byte[] pixels;
        private int fps = 60, maximumWidth = 1920;
        private int frameWidth, frameHeight;
        private double nextCapture;
        private bool readbackPending, pendingRelease;
        private long captureErrors, inputEvents, inputErrors;
        private string lastError, lastInputError, sentState;

        internal AgentStream(AgentInput input, Func<AgentCaptureRunner> runner) { this.input = input; this.runner = runner; }

        internal static bool Supported()
        { return SystemInfo.graphicsDeviceType != GraphicsDeviceType.Null && CaptureMethod() != null; }

        private static MethodInfo CaptureMethod()
        {
            Type type = Type.GetType("UnityEngine.ScreenCapture, UnityEngine.ScreenCaptureModule", false);
            return type == null ? null : type.GetMethod("CaptureScreenshotIntoRenderTexture", BindingFlags.Public | BindingFlags.Static,
                null, new[] { typeof(RenderTexture) }, null);
        }

        internal object Execute(Dictionary<string, object> arguments)
        {
            string action = Text(arguments, "action");
            if (action == "stop") { Stop(); return Status(); }
            if (action != "start") throw new InvalidDataException("Stream action must be start or stop.");
            if (!Supported()) throw new InvalidOperationException("Streaming requires an active graphics device and the public ScreenCapture module.");
            long port = Integer(arguments, "port", 0); string token = Text(arguments, "token");
            if (port < 1 || port > 65535 || token.Length < 16 || token.Length > 128) throw new InvalidDataException("Stream start requires the server's loopback port and token.");
            foreach (char c in token) if (!char.IsLetterOrDigit(c) || c > 127) throw new InvalidDataException("Stream token must be alphanumeric.");
            long newFps = Integer(arguments, "fps", 60), newWidth = Integer(arguments, "maxWidth", 1920);
            if (newFps < 1 || newFps > 60 || newWidth < 64 || newWidth > MaximumSide)
                throw new InvalidDataException("Stream settings exceed supported bounds (fps 1-60, maxWidth 64-4096).");
            Stop();
            fps = (int)newFps; maximumWidth = (int)newWidth;
            lastError = null; sentState = null; nextCapture = 0;
            link = new Link((int)port, token, pid);
            loopOwner = runner(); loop = loopOwner.StartCoroutine(CaptureLoop(link));
            return Status();
        }

        internal Dictionary<string, object> Status()
        {
            Link current = link;
            return new Dictionary<string, object> { ["supported"] = Supported(), ["active"] = current != null,
                ["connected"] = current != null && current.Connected, ["viewers"] = current == null ? 0 : current.Viewers,
                ["framesSent"] = current == null ? 0 : current.FramesSent, ["bytesSent"] = current == null ? 0 : current.BytesSent,
                ["captureErrors"] = captureErrors, ["inputEvents"] = inputEvents, ["inputErrors"] = inputErrors,
                ["lastError"] = lastError ?? (current == null ? null : current.Error), ["lastInputError"] = lastInputError,
                ["fps"] = fps, ["maxWidth"] = maximumWidth,
                ["frameSize"] = new Dictionary<string, object> { ["width"] = frameWidth, ["height"] = frameHeight },
                ["asyncReadback"] = SystemInfo.supportsAsyncGPUReadback,
                ["semantics"] = "end_of_frame_viewport_rgba_over_loopback" };
        }

        // Called on the main thread once per rendered frame, gated simulation or not.
        internal void Poll()
        {
            Link current = link; if (current == null) return;
            if (current.Closed)
            {
                // The server went away: drop what a viewer was holding down.
                lastError = current.Error; Stop(); return;
            }
            for (int handled = 0; handled < 256 && current.Lines.TryDequeue(out string line); handled++) Apply(current, line);
            if (pendingRelease) { pendingRelease = false; Release(); }
            if (!current.Connected) return;
            string state = "{\"cursorLocked\":" + (Cursor.lockState == CursorLockMode.Locked ? "true" : "false")
                + ",\"width\":" + Screen.width.ToString(CultureInfo.InvariantCulture) + ",\"height\":" + Screen.height.ToString(CultureInfo.InvariantCulture) + "}";
            if (state != sentState) { sentState = state; current.SendState(state); }
        }

        private void Apply(Link current, string line)
        {
            Dictionary<string, object> message;
            try { message = RawJson.Parse(line) as Dictionary<string, object>; }
            catch (Exception error) { inputErrors++; lastInputError = error.Message; return; }
            if (message == null) return;
            if (message.TryGetValue("viewers", out object viewers) && viewers is long count)
            {
                if (count <= 0 && current.Viewers > 0) pendingRelease = true;
                current.Viewers = (int)Math.Max(0, Math.Min(count, 1024));
                // A viewer that just arrived gets a frame without waiting for the interval.
                nextCapture = 0;
            }
            if (message.TryGetValue("release", out object release) && release is bool && (bool)release) pendingRelease = true;
            if (!(message.TryGetValue("input", out object raw) && raw is List<object> events)) return;
            foreach (object item in events)
            {
                if (!(item is Dictionary<string, object> entry)) continue;
                try { input.Execute(Resolve(entry), null); inputEvents++; }
                catch (Exception error) { inputErrors++; lastInputError = error.Message; }
            }
        }

        // Viewers know only the picture they see: positions arrive normalized with a
        // top-left origin and become viewport pixels with Unity's bottom-left origin here.
        private Dictionary<string, object> Resolve(Dictionary<string, object> entry)
        {
            if (Text(entry, "action") != "mouse") return entry;
            var resolved = new Dictionary<string, object>(entry);
            if (entry.TryGetValue("normalized", out object rawPoint) && rawPoint is Dictionary<string, object> point)
                resolved["position"] = Point(Number(point, "x") * Screen.width, (1 - Number(point, "y")) * Screen.height);
            else if (entry.TryGetValue("delta", out object rawDelta) && rawDelta is Dictionary<string, object> delta)
            {
                Vector2 from = input.MousePosition;
                resolved["position"] = Point(from.x + Number(delta, "x") * Screen.width, from.y - Number(delta, "y") * Screen.height);
            }
            resolved.Remove("normalized"); resolved.Remove("delta");
            return resolved;
        }

        private static Dictionary<string, object> Point(double x, double y)
        { return new Dictionary<string, object> { ["x"] = Math.Max(-1000000, Math.Min(1000000, x)), ["y"] = Math.Max(-1000000, Math.Min(1000000, y)) }; }

        private void Release()
        {
            try { input.ReleaseAll(); }
            catch (Exception error) { inputErrors++; lastInputError = error.Message; }
        }

        private IEnumerator CaptureLoop(Link owner)
        {
            var endOfFrame = new WaitForEndOfFrame();
            while (link == owner && !owner.Closed)
            {
                yield return endOfFrame;
                if (link != owner) yield break;
                try { Capture(owner); }
                catch (Exception error)
                {
                    readbackPending = false; captureErrors++;
                    lastError = "Capture failed: " + ((error as TargetInvocationException)?.InnerException ?? error).Message;
                    // A failing renderer is not retried every frame.
                    nextCapture = Time.realtimeSinceStartupAsDouble + 1;
                }
            }
        }

        private void Capture(Link owner)
        {
            // One frame is in flight at a time: a slow viewer or encoder lowers the
            // frame rate instead of queueing stale pictures.
            if (!owner.Connected || owner.Viewers < 1 || readbackPending || owner.Busy) return;
            double now = Time.realtimeSinceStartupAsDouble;
            if (now < nextCapture) return;
            nextCapture = Math.Max(nextCapture + 1d / fps, now + .5d / fps);
            int screenWidth = Screen.width, screenHeight = Screen.height;
            if (screenWidth < 2 || screenHeight < 2 || screenWidth > 8192 || screenHeight > 8192) return;
            Ensure(ref screenTarget, screenWidth, screenHeight);
            CaptureMethod().Invoke(null, new object[] { screenTarget });
            double scale = Math.Min(1d, Math.Min((double)maximumWidth / screenWidth, (double)MaximumSide / screenHeight));
            // Video encoders take even sizes.
            int width = Math.Max(2, (int)Math.Round(screenWidth * scale) & ~1), height = Math.Max(2, (int)Math.Round(screenHeight * scale) & ~1);
            // The server expects the top row first, and a readback starts at the bottom row
            // unless the graphics API already captured the screen upside down.
            bool flip = !SystemInfo.graphicsUVStartsAtTop;
            RenderTexture source = screenTarget;
            if (flip || width != screenWidth || height != screenHeight)
            {
                Ensure(ref scaledTarget, width, height);
                if (flip) Graphics.Blit(screenTarget, scaledTarget, new Vector2(1, -1), new Vector2(0, 1));
                else Graphics.Blit(screenTarget, scaledTarget);
                source = scaledTarget;
            }
            frameWidth = width; frameHeight = height;
            if (SystemInfo.supportsAsyncGPUReadback)
            {
                readbackPending = true;
                AsyncGPUReadback.Request(source, 0, TextureFormat.RGBA32, request =>
                {
                    readbackPending = false;
                    if (link != owner || owner.Closed) return;
                    if (request.hasError) { captureErrors++; lastError = "GPU readback failed."; return; }
                    Submit(owner, request.GetData<byte>(), request.width, request.height, screenWidth, screenHeight);
                });
                return;
            }
            if (readbackTexture == null || readbackTexture.width != width || readbackTexture.height != height)
            {
                if (readbackTexture != null) UnityEngine.Object.Destroy(readbackTexture);
                readbackTexture = new Texture2D(width, height, TextureFormat.RGBA32, false) { hideFlags = HideFlags.HideAndDontSave };
            }
            RenderTexture previous = RenderTexture.active;
            try { RenderTexture.active = source; readbackTexture.ReadPixels(new Rect(0, 0, width, height), 0, 0, false); }
            finally { RenderTexture.active = previous; }
            Submit(owner, readbackTexture.GetRawTextureData<byte>(), width, height, screenWidth, screenHeight);
        }

        private void Submit(Link owner, NativeArray<byte> data, int width, int height, int screenWidth, int screenHeight)
        {
            if (data.Length != width * height * 4) { captureErrors++; lastError = "Readback returned an unexpected pixel layout."; return; }
            // The link has sent the previous frame: Busy was false when this one was captured.
            if (pixels == null || pixels.Length != data.Length) pixels = new byte[data.Length];
            data.CopyTo(pixels);
            owner.SendFrame(pixels, width, height, screenWidth, screenHeight);
        }

        private static void Ensure(ref RenderTexture target, int width, int height)
        {
            if (target != null && target.width == width && target.height == height && target.IsCreated()) return;
            if (target != null) { target.Release(); UnityEngine.Object.Destroy(target); }
            target = new RenderTexture(width, height, 0, RenderTextureFormat.ARGB32) { hideFlags = HideFlags.HideAndDontSave, filterMode = FilterMode.Bilinear };
            target.Create();
        }

        private void Stop()
        {
            if (loop != null && loopOwner != null) loopOwner.StopCoroutine(loop);
            loop = null; loopOwner = null;
            if (link != null) { link.Dispose(); link = null; Release(); }
            readbackPending = false; pendingRelease = false;
        }

        public void Dispose()
        {
            Stop();
            if (screenTarget != null) { screenTarget.Release(); UnityEngine.Object.Destroy(screenTarget); screenTarget = null; }
            if (scaledTarget != null) { scaledTarget.Release(); UnityEngine.Object.Destroy(scaledTarget); scaledTarget = null; }
            if (readbackTexture != null) { UnityEngine.Object.Destroy(readbackTexture); readbackTexture = null; }
        }

        private static string Text(Dictionary<string, object> value, string key) { return value.TryGetValue(key, out object item) ? item as string ?? string.Empty : string.Empty; }
        private static long Integer(Dictionary<string, object> value, string key, long fallback)
        { if (!value.TryGetValue(key, out object item)) return fallback; if (!(item is long)) throw new InvalidDataException("Expected an integer: " + key); return (long)item; }
        private static double Number(Dictionary<string, object> value, string key)
        {
            if (!value.TryGetValue(key, out object item) || (!(item is long) && !(item is double))) throw new InvalidDataException("Expected a number: " + key);
            double number = Convert.ToDouble(item, CultureInfo.InvariantCulture);
            if (double.IsNaN(number) || double.IsInfinity(number)) throw new InvalidDataException("Expected a finite number: " + key);
            return number;
        }

        /// <summary>
        /// One connection to the server. The Player writes typed, length-prefixed messages
        /// ('F' frame: viewport and picture size, then R,G,B,A rows; 'S' state) and reads
        /// newline-delimited JSON.
        /// </summary>
        private sealed class Link : IDisposable
        {
            internal readonly ConcurrentQueue<string> Lines = new ConcurrentQueue<string>();
            private readonly object gate = new object();
            private readonly int port, pid;
            private readonly string token;
            private TcpClient client;
            private NetworkStream stream;
            private byte[] framePixels;
            private int frameWidth, frameHeight, frameScreenWidth, frameScreenHeight;
            private string state;
            private volatile bool closed, connected, busy;
            private long framesSent, bytesSent;
            private volatile string error;
            internal volatile int Viewers;
            internal bool Closed => closed;
            internal bool Connected => connected;
            internal bool Busy => busy;
            internal string Error => error;
            internal long FramesSent => Interlocked.Read(ref framesSent);
            internal long BytesSent => Interlocked.Read(ref bytesSent);

            internal Link(int port, string token, int pid)
            {
                this.port = port; this.token = token; this.pid = pid;
                new Thread(Write) { IsBackground = true, Name = "UnityMicroEditor.Stream.Write" }.Start();
            }

            internal void SendFrame(byte[] pixels, int width, int height, int screenWidth, int screenHeight)
            {
                lock (gate)
                {
                    if (closed) return;
                    framePixels = pixels; frameWidth = width; frameHeight = height; frameScreenWidth = screenWidth; frameScreenHeight = screenHeight;
                    busy = true; Monitor.PulseAll(gate);
                }
            }

            internal void SendState(string value) { lock (gate) { if (closed) return; state = value; Monitor.PulseAll(gate); } }

            private void Write()
            {
                try
                {
                    var socket = new TcpClient { NoDelay = true, SendTimeout = 10000 };
                    lock (gate) { if (closed) { socket.Close(); return; } client = socket; }
                    socket.Connect(IPAddress.Loopback, port);
                    NetworkStream output = socket.GetStream();
                    lock (gate) { if (closed) return; stream = output; }
                    byte[] hello = Encoding.ASCII.GetBytes("UME-STREAM 1 " + token + " " + pid.ToString(CultureInfo.InvariantCulture) + "\n");
                    output.Write(hello, 0, hello.Length);
                    new Thread(() => Read(output)) { IsBackground = true, Name = "UnityMicroEditor.Stream.Read" }.Start();
                    connected = true;
                    var header = new byte[13];
                    while (true)
                    {
                        byte[] raw; string text; int width, height, screenWidth, screenHeight;
                        lock (gate)
                        {
                            while (!closed && framePixels == null && state == null) Monitor.Wait(gate);
                            if (closed) return;
                            raw = framePixels; framePixels = null; text = state; state = null;
                            width = frameWidth; height = frameHeight; screenWidth = frameScreenWidth; screenHeight = frameScreenHeight;
                        }
                        if (text != null)
                        {
                            byte[] payload = Encoding.UTF8.GetBytes(text);
                            header[0] = (byte)'S'; Length(header, payload.Length);
                            output.Write(header, 0, 5); output.Write(payload, 0, payload.Length);
                        }
                        if (raw == null) continue;
                        try
                        {
                            header[0] = (byte)'F'; Length(header, raw.Length + 8);
                            Short(header, 5, screenWidth); Short(header, 7, screenHeight); Short(header, 9, width); Short(header, 11, height);
                            output.Write(header, 0, header.Length); output.Write(raw, 0, raw.Length);
                            Interlocked.Increment(ref framesSent); Interlocked.Add(ref bytesSent, raw.Length);
                        }
                        finally { busy = false; }
                    }
                }
                catch (Exception failure) { if (!closed) error = failure.Message; }
                finally { Close(); }
            }

            private void Read(NetworkStream source)
            {
                try
                {
                    var buffer = new byte[16384]; var line = new MemoryStream();
                    while (!closed)
                    {
                        int count = source.Read(buffer, 0, buffer.Length);
                        if (count <= 0) break;
                        for (int index = 0; index < count; index++)
                        {
                            if (buffer[index] != (byte)'\n')
                            {
                                if (line.Length >= 65536) throw new InvalidDataException("The server sent an oversized message.");
                                line.WriteByte(buffer[index]); continue;
                            }
                            // A stalled main thread must not let a viewer grow the queue.
                            if (Lines.Count < MaximumQueuedLines) Lines.Enqueue(new UTF8Encoding(false, true).GetString(line.GetBuffer(), 0, (int)line.Length));
                            line.SetLength(0);
                        }
                    }
                }
                catch (Exception failure) { if (!closed) error = failure.Message; }
                finally { Close(); }
            }

            private static void Length(byte[] header, int value)
            { header[1] = (byte)value; header[2] = (byte)(value >> 8); header[3] = (byte)(value >> 16); header[4] = (byte)(value >> 24); }
            private static void Short(byte[] header, int offset, int value)
            { header[offset] = (byte)value; header[offset + 1] = (byte)(value >> 8); }

            private void Close()
            {
                TcpClient socket;
                lock (gate) { if (closed) return; closed = true; connected = false; socket = client; client = null; Monitor.PulseAll(gate); }
                try { if (socket != null) socket.Close(); } catch (Exception) { }
            }

            public void Dispose() { Close(); }
        }
    }
}
