using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Security.Cryptography;
using System.Text;
using UnityMicroEditor.Raw;
using UnityEngine;
using UnityEngine.SceneManagement;
using Object = UnityEngine.Object;

namespace UnityMicroEditor
{
    /// <summary>Project commands register explicitly and execute on Unity's main thread.</summary>
    public static class AgentRuntimeCommands
    {
        private static readonly Dictionary<string, Func<string, string>> handlers = new Dictionary<string, Func<string, string>>(StringComparer.Ordinal);
        private static readonly object registryLock = new object();

        public static IDisposable Register(string name, Func<string, string> handler)
        {
            lock (registryLock)
            {
                if (string.IsNullOrEmpty(name) || name.Length > 128 || handler == null || handlers.ContainsKey(name))
                    throw new ArgumentException("A command requires a unique bounded name and a handler.");
                handlers.Add(name, handler);
            }
            return new Registration(name, handler);
        }

        internal static object Execute(string name, object arguments)
        {
            Func<string, string> handler;
            lock (registryLock)
                if (!handlers.TryGetValue(name, out handler)) throw new InvalidOperationException("Unknown project command: " + name);
            string result = handler(RawJson.Write(arguments));
            if (result == null || result.Length > 1024 * 1024) throw new InvalidDataException("Project command result is missing or exceeds the size limit.");
            return RawJson.Parse(result);
        }

        internal static List<object> Names()
        {
            List<string> names; lock (registryLock) names = new List<string>(handlers.Keys); names.Sort(StringComparer.Ordinal);
            var values = new List<object>(); foreach (string name in names) values.Add(name); return values;
        }

        private sealed class Registration : IDisposable
        {
            private readonly string name; private readonly Func<string, string> handler;
            internal Registration(string name, Func<string, string> handler) { this.name = name; this.handler = handler; }
            public void Dispose() { lock (registryLock) if (handlers.TryGetValue(name, out Func<string, string> current) && current == handler) handlers.Remove(name); }
        }
    }

    /// <summary>Public API readbacks shared by Editor comparisons and the Player bridge.</summary>
    public static class AgentRuntimeSnapshot
    {
        public static Dictionary<string, object> Capture(GameObject value, Func<Object, Dictionary<string, object>> identity = null)
        {
            if (value == null) throw new ArgumentNullException(nameof(value));
            Transform transform = value.transform;
            var components = new List<object>();
            foreach (Component component in value.GetComponents<Component>())
            {
                if (component == null) { components.Add(new Dictionary<string, object> { ["type"] = "missing_script" }); continue; }
                var entry = new Dictionary<string, object> { ["type"] = component.GetType().FullName, ["fields"] = Fields(component) };
                if (identity != null) entry["source"] = identity(component);
                components.Add(entry);
            }
            var result = new Dictionary<string, object>
            {
                ["path"] = PathOf(transform), ["name"] = value.name, ["activeSelf"] = value.activeSelf,
                ["activeInHierarchy"] = value.activeInHierarchy, ["tag"] = value.tag, ["layer"] = value.layer,
                ["siblingIndex"] = HierarchyIndex(transform), ["position"] = Vector(transform.position),
                ["localPosition"] = Vector(transform.localPosition), ["rotation"] = QuaternionValue(transform.rotation),
                ["localRotation"] = QuaternionValue(transform.localRotation), ["localScale"] = Vector(transform.localScale),
                ["components"] = components,
            };
            if (identity != null) result["source"] = identity(value);
            return result;
        }

        public static string CaptureJson(GameObject value) { return RawJson.Write(Capture(value)); }

        /// <summary>Live roots, including persistent roots in Unity's unlisted DontDestroyOnLoad scene.</summary>
        public static GameObject[] LiveRoots()
        {
            var roots = new List<GameObject>(); var seen = new HashSet<GameObject>(); var scenes = new HashSet<Scene>();
            for (int index = 0; index < SceneManager.sceneCount; index++)
            {
                Scene scene = SceneManager.GetSceneAt(index); if (!scene.isLoaded) continue;
                scenes.Add(scene);
                foreach (GameObject root in scene.GetRootGameObjects()) AddLiveRoot(root, roots, seen);
            }
            if (Application.isPlaying)
                foreach (GameObject value in Object.FindObjectsByType<GameObject>(FindObjectsInactive.Include))
                {
                    // SceneManager.sceneCount omits the persistent scene. The
                    // public object query also finds its inactive children.
                    if (value == null || !value.scene.IsValid() || !value.scene.isLoaded) continue;
                    if (!scenes.Add(value.scene)) continue;
                    foreach (GameObject root in value.scene.GetRootGameObjects()) AddLiveRoot(root, roots, seen);
                }
            return roots.ToArray();
        }

        private static void AddLiveRoot(GameObject root, List<GameObject> roots, HashSet<GameObject> seen)
        {
            if (root == null || !seen.Add(root)) return;
            if (root.GetComponent<RawSceneResources>() != null || root.GetComponent<AgentCaptureRunner>() != null) return;
            roots.Add(root);
        }

        public static string PathOf(Transform value)
        {
            var parts = new List<string>();
            while (value != null) { parts.Add(value.name.Replace("~", "~0").Replace("/", "~1")); value = value.parent; }
            parts.Reverse(); return string.Join("/", parts);
        }

        private static int HierarchyIndex(Transform value)
        {
            if (value.parent != null) return value.GetSiblingIndex();
            // Unity Player returns zero for native root GetSiblingIndex. The MCP
            // hierarchy reports the index in public Scene root enumeration.
            int index = 0;
            foreach (GameObject root in value.gameObject.scene.GetRootGameObjects())
            {
                if (root.GetComponent<RawSceneResources>() != null || root.GetComponent<AgentCaptureRunner>() != null) continue;
                if (root.transform == value) return index;
                index++;
            }
            throw new InvalidOperationException("Root Transform is absent from its scene.");
        }

        private static Dictionary<string, object> Fields(Component component)
        {
            var values = new Dictionary<string, object>();
            if (component is Behaviour behaviour) values["enabled"] = behaviour.enabled;
            if (component is Transform transform)
            { values["localPosition"] = Vector(transform.localPosition); values["localRotation"] = QuaternionValue(transform.localRotation); values["localScale"] = Vector(transform.localScale); }
            if (component is RectTransform rect)
            { values["anchorMin"] = Vector2Value(rect.anchorMin); values["anchorMax"] = Vector2Value(rect.anchorMax); values["anchoredPosition"] = Vector2Value(rect.anchoredPosition); values["sizeDelta"] = Vector2Value(rect.sizeDelta); values["pivot"] = Vector2Value(rect.pivot); values["offsetMin"] = Vector2Value(rect.offsetMin); values["offsetMax"] = Vector2Value(rect.offsetMax); values["rect"] = new Dictionary<string, object> { ["x"] = rect.rect.x, ["y"] = rect.rect.y, ["width"] = rect.rect.width, ["height"] = rect.rect.height }; }
            if (component is Camera camera)
            { values["fieldOfView"] = camera.fieldOfView; values["nearClipPlane"] = camera.nearClipPlane; values["farClipPlane"] = camera.farClipPlane; values["orthographic"] = camera.orthographic; values["orthographicSize"] = camera.orthographicSize; values["backgroundColor"] = ColorValue(camera.backgroundColor); values["clearFlags"] = (int)camera.clearFlags; values["cullingMask"] = camera.cullingMask; }
            if (component is Light light)
            { values["type"] = (int)light.type; values["color"] = ColorValue(light.color); values["intensity"] = light.intensity; values["range"] = light.range; values["shadows"] = (int)light.shadows; }
            if (component is Rigidbody body)
            { values["mass"] = body.mass; values["useGravity"] = body.useGravity; values["isKinematic"] = body.isKinematic; values["constraints"] = (int)body.constraints; }
            if (component is Collider collider)
            { values["enabled"] = collider.enabled; values["isTrigger"] = collider.isTrigger; }
            if (component is BoxCollider box) { values["center"] = Vector(box.center); values["size"] = Vector(box.size); }
            if (component is SphereCollider sphere) { values["center"] = Vector(sphere.center); values["radius"] = sphere.radius; }
            if (component is CapsuleCollider capsule) { values["center"] = Vector(capsule.center); values["radius"] = capsule.radius; values["height"] = capsule.height; values["direction"] = capsule.direction; }
            if (component is AudioSource audio)
            { values["volume"] = audio.volume; values["pitch"] = audio.pitch; values["mute"] = audio.mute; values["loop"] = audio.loop; values["playOnAwake"] = audio.playOnAwake; values["spatialBlend"] = audio.spatialBlend; values["minDistance"] = audio.minDistance; values["maxDistance"] = audio.maxDistance; values["rolloffMode"] = (int)audio.rolloffMode; values["priority"] = audio.priority; values["isPlaying"] = audio.isPlaying; values["timeSamples"] = audio.clip == null ? 0 : audio.timeSamples; values["clip"] = audio.clip == null ? null : new Dictionary<string, object> { ["name"] = audio.clip.name, ["samples"] = audio.clip.samples, ["frequency"] = audio.clip.frequency, ["channels"] = audio.clip.channels, ["loadState"] = audio.clip.loadState.ToString() }; }
            if (component is Animator animator)
            {
                values["controllerName"] = animator.runtimeAnimatorController == null ? null : animator.runtimeAnimatorController.name;
                values["applyRootMotion"] = animator.applyRootMotion; values["updateMode"] = (int)animator.updateMode; values["cullingMode"] = (int)animator.cullingMode; values["speed"] = animator.speed; values["isInitialized"] = animator.isInitialized;
                var parameters = new List<object>();
                if (animator.runtimeAnimatorController != null && animator.isInitialized)
                    foreach (AnimatorControllerParameter parameter in animator.parameters)
                    {
                        var entry = new Dictionary<string, object> { ["name"] = parameter.name, ["type"] = parameter.type.ToString() };
                        if (parameter.type == AnimatorControllerParameterType.Float) entry["value"] = animator.GetFloat(parameter.nameHash);
                        if (parameter.type == AnimatorControllerParameterType.Int) entry["value"] = animator.GetInteger(parameter.nameHash);
                        if (parameter.type == AnimatorControllerParameterType.Bool) entry["value"] = animator.GetBool(parameter.nameHash);
                        parameters.Add(entry);
                    }
                values["parameters"] = parameters;
            }
            if (component is Canvas canvas)
            { values["renderMode"] = (int)canvas.renderMode; values["sortingOrder"] = canvas.sortingOrder; values["overrideSorting"] = canvas.overrideSorting; values["scaleFactor"] = canvas.scaleFactor; values["referencePixelsPerUnit"] = canvas.referencePixelsPerUnit; values["targetDisplay"] = canvas.targetDisplay; values["pixelPerfect"] = canvas.pixelPerfect; values["worldCameraPath"] = canvas.worldCamera == null ? null : PathOf(canvas.worldCamera.transform); }
            if (component is MeshFilter filter)
            { values["meshName"] = filter.sharedMesh == null ? null : filter.sharedMesh.name; values["vertexCount"] = filter.sharedMesh == null ? 0 : filter.sharedMesh.vertexCount; }
            if (component is Renderer renderer)
            {
                values["enabled"] = renderer.enabled; var materials = new List<object>();
                foreach (Material material in renderer.sharedMaterials)
                    materials.Add(material == null ? null : new Dictionary<string, object>
                    { ["name"] = material.name, ["shader"] = material.shader == null ? null : material.shader.name,
                        ["supported"] = material.shader != null && material.shader.isSupported,
                        ["color"] = material.HasProperty("_Color") ? ColorValue(material.color) : null });
                values["materials"] = materials;
            }
            string assemblyName = component.GetType().Assembly.GetName().Name;
            bool packageComponent = assemblyName == "UnityEngine.UI" || assemblyName.StartsWith("Unity.", StringComparison.Ordinal)
                || assemblyName.StartsWith("UnityEngine.", StringComparison.Ordinal);
            if (packageComponent) CapturePackageUi(component, values);
            if (component is MonoBehaviour && !packageComponent)
            {
                string json = JsonUtility.ToJson(component);
                if (json.Length > 65536) values["serializedTruncated"] = true;
                else values["serialized"] = RawJson.Parse(json);
            }
            return values;
        }

        internal static Dictionary<string, object> Vector(Vector3 value) { return new Dictionary<string, object> { ["x"] = value.x, ["y"] = value.y, ["z"] = value.z }; }
        private static Dictionary<string, object> Vector2Value(Vector2 value) { return new Dictionary<string, object> { ["x"] = value.x, ["y"] = value.y }; }
        private static Dictionary<string, object> QuaternionValue(Quaternion value) { return new Dictionary<string, object> { ["x"] = value.x, ["y"] = value.y, ["z"] = value.z, ["w"] = value.w }; }
        private static Dictionary<string, object> ColorValue(Color value) { return new Dictionary<string, object> { ["r"] = value.r, ["g"] = value.g, ["b"] = value.b, ["a"] = value.a }; }
        private static void CapturePackageUi(Component component, Dictionary<string, object> values)
        {
            // uGUI is optional. Read a bounded list of its documented PUBLIC
            // properties instead of serializing private package fields/IDs.
            string[] properties = { "interactable", "transition", "raycastTarget", "maskable", "color", "text", "fontSize", "alignment", "supportRichText", "fillAmount", "fillMethod", "fillOrigin", "fillClockwise", "isOn", "value", "minValue", "maxValue", "wholeNumbers", "horizontal", "vertical", "normalizedPosition", "sortingOrder", "pointerBehavior", "cursorLockBehavior", "deselectOnBackgroundClick", "moveRepeatDelay", "moveRepeatRate", "scrollDeltaPerTick", "sendNavigationEvents", "pixelDragThreshold" };
            Type type = component.GetType();
            foreach (string name in properties)
            {
                var property = type.GetProperty(name, System.Reflection.BindingFlags.Public | System.Reflection.BindingFlags.Instance);
                if (property == null || property.GetGetMethod(false) == null || property.GetIndexParameters().Length != 0) continue;
                Type kind = property.PropertyType;
                if (kind != typeof(bool) && kind != typeof(string) && kind != typeof(int) && kind != typeof(float) && kind != typeof(Color) && kind != typeof(Vector2) && !kind.IsEnum) continue;
                object value = property.GetValue(component, null);
                if (value is Color color) value = ColorValue(color); else if (value is Vector2 vector) value = Vector2Value(vector); else if (kind.IsEnum) value = Convert.ToInt64(value, CultureInfo.InvariantCulture);
                values[name] = value;
            }
        }
    }

    internal sealed class AgentControl : IDisposable
    {
        private readonly string directory;
        private readonly Func<RawSceneLoader.LiveSession> session;
        private readonly int pid = System.Diagnostics.Process.GetCurrentProcess().Id;
        private readonly object logLock = new object();
        private readonly List<object> logs = new List<object>();
        private readonly Dictionary<string, PendingFrames> pending = new Dictionary<string, PendingFrames>(StringComparer.Ordinal);
        private readonly AgentSimulation simulation = new AgentSimulation();
        private readonly AgentInput input;
        private readonly Dictionary<string, PendingFrames> pendingScreens = new Dictionary<string, PendingFrames>(StringComparer.Ordinal);
        private AgentCaptureRunner captureRunner;
        private PendingFrames pendingStep;
        private long logSequence;
        private double nextPoll;
        private float previousTimeScale;
        private bool paused;
        private volatile bool disposed;

        internal AgentControl(string directory, Func<RawSceneLoader.LiveSession> session)
        {
            this.directory = Path.GetFullPath(directory); this.session = session;
            input = new AgentInput(() => session() == null ? null : session().ProjectRoot);
            AgentRuntimeTools.BindMainThread();
            Directory.CreateDirectory(this.directory);
            if ((File.GetAttributes(this.directory) & FileAttributes.ReparsePoint) != 0) throw new InvalidDataException("Control directory cannot be a symlink.");
            Application.logMessageReceivedThreaded += OnLog;
            simulation.EnsureCounters();
        }

        internal void Poll()
        {
            if (disposed) return;
            bool stepFinished = simulation.CompleteAtEndOfFrame();
            if (pendingStep != null)
            {
                PendingFrames value = pendingStep;
                if (session() == null || value.Revision != session().Revision)
                { Reply(value.Request, value.Hash, null, "stale_revision", "Scene changed while the simulation step was running."); pendingStep = null; }
                else if (stepFinished)
                { Reply(value.Request, value.Hash, Status(), simulation.LastStepError == null ? null : "step_failed", simulation.LastStepError); pendingStep = null; }
                else if (Time.realtimeSinceStartupAsDouble > value.Deadline)
                { Reply(value.Request, value.Hash, null, "outcome_unknown", "The accepted simulation step did not complete before its deadline."); pendingStep = null; }
            }
            foreach (PendingFrames value in new List<PendingFrames>(pending.Values))
            {
                if (session() == null || value.Revision != session().Revision)
                { Reply(value.Request, value.Hash, null, "stale_revision", "Scene changed while waiting for frames."); pending.Remove(value.Id); }
                else if (Time.frameCount >= value.Target)
                { Reply(value.Request, value.Hash, Status(), null, null); pending.Remove(value.Id); }
                else if (Time.realtimeSinceStartupAsDouble > value.Deadline)
                { Reply(value.Request, value.Hash, null, "timeout", "Frame wait expired."); pending.Remove(value.Id); }
            }
            foreach (PendingFrames value in new List<PendingFrames>(pendingScreens.Values))
                if (Time.realtimeSinceStartupAsDouble > value.Deadline)
                { pendingScreens.Remove(value.Id); Reply(value.Request, value.Hash, null, "outcome_unknown", "The accepted screen capture did not complete before its deadline."); }
            double now = Time.realtimeSinceStartupAsDouble;
            if (now < nextPoll) return; nextPoll = now + .025;
            string[] files;
            try { files = Directory.GetFiles(directory, "*.request.json"); }
            catch (Exception error) { nextPoll = now + 1; Warn("Cannot read control inbox", error); return; }
            Array.Sort(files, StringComparer.Ordinal);
            int handled = 0;
            foreach (string file in files)
            {
                string id = Path.GetFileName(file).Replace(".request.json", string.Empty);
                if (!ValidId(id) || pending.ContainsKey(id) || pendingScreens.ContainsKey(id) || (pendingStep != null && pendingStep.Id == id)) continue;
                if (File.Exists(ResponsePath(id)))
                { try { File.Delete(file); } catch (Exception error) { Warn("Cannot retire completed request " + id, error); } continue; }
                if (++handled > 16) break;
                Dictionary<string, object> request = null; string hash = string.Empty; bool customMayHaveExecuted = false;
                try
                {
                    var info = new FileInfo(file);
                    if (info.Length > 65536 || (info.Attributes & FileAttributes.ReparsePoint) != 0) throw new InvalidDataException("Command file exceeds bounds or is a symlink.");
                    byte[] bytes = File.ReadAllBytes(file); hash = Hash(bytes);
                    request = RawJson.Parse(new UTF8Encoding(false, true).GetString(bytes)) as Dictionary<string, object>;
                    if (request == null || Text(request, "requestId") != id || Integer(request, "schemaVersion") != 1)
                        throw new InvalidDataException("Command identity or schema is invalid.");
                    if (Integer(request, "expectedPid") != pid)
                    { Reply(request, hash, null, "stale_process", "Command targets another Player process."); continue; }
                    RawSceneLoader.LiveSession live = session();
                    string operation = Text(request, "operation");
                    if (live == null || live.LastReloadRestartRequired)
                    { Reply(request, hash, null, "busy", "The source scene is not ready."); continue; }
                    if (operation != "status" && Text(request, "expectedRevision") != live.Revision)
                    { Reply(request, hash, null, "stale_revision", "Command targets another scene revision."); continue; }
                    var arguments = request.TryGetValue("arguments", out object args) ? args as Dictionary<string, object> : null;
                    if (arguments == null) throw new InvalidDataException("Command arguments must be a JSON object.");
                    string accepted = Path.Combine(directory, id + ".accepted.json");
                    if (File.Exists(accepted))
                    {
                        Reply(request, hash, null, "outcome_unknown", "This request was already accepted; its response is unavailable. Inspect state before issuing another command.");
                        continue;
                    }
                    // Atomically claim the immutable command before any side
                    // effect. Retain the claim even if response publication fails.
                    // A client retry can recreate the inbox file, but cannot
                    // cause this accepted ID to execute a second time.
                    File.Move(file, accepted);
                    if (operation == "wait_frames")
                    {
                        long frames = Integer(arguments, "frames");
                        if (frames < 1 || frames > 3600 || pending.Count >= 32) throw new InvalidDataException("Frame wait exceeds supported bounds.");
                        pending.Add(id, new PendingFrames { Id = id, Request = request, Hash = hash, Revision = live.Revision,
                            Target = Time.frameCount + frames, Deadline = now + 60 }); continue;
                    }
                    if (operation == "simulation_step")
                    {
                        if (arguments.ContainsKey("frames") && Integer(arguments, "frames") != 1) throw new InvalidDataException("Simulation stepping supports exactly one game/fixed/late iteration per request.");
                        if (pendingStep != null) throw new InvalidOperationException("Another simulation step is still pending.");
                        simulation.Step();
                        pendingStep = new PendingFrames { Id = id, Request = request, Hash = hash, Revision = live.Revision, Deadline = now + 60 }; continue;
                    }
                    if (operation == "screenshot" && (Text(arguments, "mode") == string.Empty || Text(arguments, "mode") == "screen"))
                    { QueueScreen(id, request, hash, arguments, live, now); continue; }
                    customMayHaveExecuted = operation == "custom";
                    Reply(request, hash, Execute(operation, arguments, live), null, null);
                }
                catch (Exception error)
                {
                    if (request == null) request = new Dictionary<string, object> { ["requestId"] = id };
                    // The filename owns the receipt address. A malformed body
                    // cannot redirect an error into another command's response.
                    request["requestId"] = id;
                    Reply(request, hash, null, customMayHaveExecuted || error is AgentToolExecutionException || error is AgentInputExecutionException || error is AgentCaptureExecutionException ? "outcome_unknown" : "command_rejected", error.Message);
                }
            }
        }

        private object Execute(string operation, Dictionary<string, object> arguments, RawSceneLoader.LiveSession live)
        {
            switch (operation)
            {
                case "status": return Status();
                case "hierarchy":
                {
                    List<GameObject> objects = SceneObjects();
                    long offset = Integer(arguments, "offset"), limit = Integer(arguments, "limit");
                    if (limit == 0) limit = 100;
                    if (offset < 0 || limit < 1 || limit > 500) throw new InvalidDataException("Hierarchy page exceeds supported bounds.");
                    var values = new List<object>();
                    for (long index = offset; index < objects.Count && values.Count < limit; index++)
                        values.Add(AgentRuntimeSnapshot.Capture(objects[(int)index], live.DescribeSourceObject));
                    return new Dictionary<string, object> { ["objects"] = values, ["total"] = objects.Count,
                        ["offset"] = offset, ["nextOffset"] = offset + values.Count < objects.Count ? (object)(offset + values.Count) : null };
                }
                case "inspect": return AgentRuntimeSnapshot.Capture(FindObject(arguments, live), live.DescribeSourceObject);
                case "logs":
                {
                    long after = Integer(arguments, "afterSequence"); var values = new List<object>();
                    lock (logLock)
                    {
                        foreach (Dictionary<string, object> value in logs) if ((long)value["sequence"] > after) values.Add(value);
                        return new Dictionary<string, object> { ["entries"] = values, ["lastSequence"] = logSequence,
                            ["firstAvailableSequence"] = logs.Count == 0 ? logSequence + 1 : ((Dictionary<string, object>)logs[0])["sequence"] };
                    }
                }
                case "pause": case "scaled_time_pause":
                    if (simulation.Paused) throw new InvalidOperationException("Scaled-time control cannot change a gated simulation pause.");
                    if (!paused) { previousTimeScale = Time.timeScale; Time.timeScale = 0; paused = true; } return Status();
                case "resume": case "scaled_time_resume":
                    if (simulation.Paused) throw new InvalidOperationException("Use simulation_resume for the gated simulation pause.");
                    if (paused) { Time.timeScale = previousTimeScale; paused = false; } return Status();
                case "simulation_pause": simulation.Pause(); return Status();
                case "simulation_resume": simulation.Resume(); return Status();
                case "tools": return AgentRuntimeTools.Describe(live.DescribeSourceObject);
                case "invoke":
                {
                    GameObject target = null;
                    if (arguments.TryGetValue("target", out object targetValue))
                    { if (!(targetValue is Dictionary<string, object> targetArgs)) throw new InvalidDataException("Tool target must be an object path/source identity."); target = FindObject(targetArgs, live); }
                    return AgentRuntimeTools.Invoke(Text(arguments, "name"), arguments.TryGetValue("parameters", out object toolParameters) ? toolParameters : new Dictionary<string, object>(), target);
                }
                case "input":
                {
                    GameObject target = null;
                    if (Text(arguments, "action") == "ui_click")
                    { if (!(arguments.TryGetValue("target", out object rawTarget) && rawTarget is Dictionary<string, object> targetArgs)) throw new InvalidDataException("UI click requires a source object target."); target = FindObject(targetArgs, live); }
                    return input.Execute(arguments, target);
                }
                case "screenshot":
                {
                    if (Text(arguments, "mode") != "camera") throw new InvalidDataException("Screenshot mode must be screen or camera.");
                    return CameraCapture(arguments, live);
                }
                case "raycast":
                {
                    Vector3 origin = ReadVector(arguments, "origin"), direction = ReadVector(arguments, "direction");
                    float distance = Number(arguments, "distance", 1000);
                    if (distance <= 0 || distance > 100000 || direction.sqrMagnitude < .000001f) throw new InvalidDataException("Raycast parameters exceed supported bounds.");
                    bool hit = Physics.Raycast(origin, direction, out RaycastHit result, distance);
                    return new Dictionary<string, object> { ["hit"] = hit, ["path"] = hit ? AgentRuntimeSnapshot.PathOf(result.collider.transform) : null,
                        ["point"] = hit ? AgentRuntimeSnapshot.Vector(result.point) : null, ["distance"] = hit ? (object)result.distance : null };
                }
                case "custom": return AgentRuntimeCommands.Execute(Text(arguments, "name"), arguments.TryGetValue("parameters", out object parameters) ? parameters : null);
                default: throw new InvalidOperationException("Unsupported runtime operation: " + operation);
            }
        }

        private Dictionary<string, object> Status()
        {
            RawSceneLoader.LiveSession live = session();
            return new Dictionary<string, object> { ["pid"] = pid, ["revision"] = live == null ? string.Empty : live.Revision,
                ["frame"] = Time.frameCount, ["timeScale"] = Time.timeScale, ["pausedScaledTime"] = paused,
                ["pauseSemantics"] = simulation.Paused ? "gated_game_player_loop" : "scaled_time_only", ["graphicsDevice"] = SystemInfo.graphicsDeviceName,
                ["simulation"] = simulation.Status(), ["input"] = input.Capabilities(),
                ["viewport"] = new Dictionary<string, object> { ["width"] = Screen.width, ["height"] = Screen.height },
                ["screenshot"] = new Dictionary<string, object> { ["screenSupported"] = SystemInfo.graphicsDeviceType != UnityEngine.Rendering.GraphicsDeviceType.Null && AgentCaptureRunner.ScreenCaptureMethod() != null,
                    ["cameraSupported"] = SystemInfo.graphicsDeviceType != UnityEngine.Rendering.GraphicsDeviceType.Null,
                    ["defaultMode"] = "screen", ["screenIncludesOverlayUi"] = true, ["screenSemantics"] = "native_viewport_at_end_of_frame", ["maximumDimension"] = 4096 },
                ["rootSiblingIndexSemantics"] = "scene_root_enumeration",
                ["unityVersion"] = Application.unityVersion, ["commands"] = AgentRuntimeCommands.Names(),
                ["operations"] = new List<object> { "status", "hierarchy", "inspect", "logs", "pause", "resume", "scaled_time_pause", "scaled_time_resume", "simulation_pause", "simulation_resume", "simulation_step", "input", "tools", "invoke", "wait_frames", "screenshot", "raycast", "custom" } };
        }

        private static List<GameObject> SceneObjects()
        {
            var values = new List<GameObject>();
            foreach (GameObject root in AgentRuntimeSnapshot.LiveRoots())
                foreach (Transform transform in root.GetComponentsInChildren<Transform>(true))
                    if (transform.gameObject.GetComponent<RawSceneResources>() == null && transform.gameObject.GetComponent<AgentCaptureRunner>() == null)
                        values.Add(transform.gameObject);
            return values;
        }

        private void QueueScreen(string id, Dictionary<string, object> request, string hash, Dictionary<string, object> arguments,
            RawSceneLoader.LiveSession live, double now)
        {
            RequireGraphics();
            if (AgentCaptureRunner.ScreenCaptureMethod() == null) throw new InvalidOperationException("This host profile has no public ScreenCapture API.");
            ValidateCaptureSize(Screen.width, Screen.height);
            long width = Integer(arguments, "width"), height = Integer(arguments, "height");
            if ((width != 0 && width != Screen.width) || (height != 0 && height != Screen.height))
                throw new InvalidDataException("Screen capture uses the actual viewport. Requested dimensions differ from " + Screen.width + "x" + Screen.height + "; use camera mode for an independent size.");
            if (arguments.ContainsKey("target") || !string.IsNullOrEmpty(Text(arguments, "camera")))
                throw new InvalidDataException("Screen mode captures the whole viewport and does not accept a camera target.");
            if (pendingScreens.Count >= 4) throw new InvalidOperationException("Too many screen captures are pending.");
            if (captureRunner == null)
            {
                var owner = new GameObject("UnityMicroEditor.AgentCapture") { hideFlags = HideFlags.HideAndDontSave };
                owner.AddComponent<RawSceneResources>(); Object.DontDestroyOnLoad(owner);
                captureRunner = owner.AddComponent<AgentCaptureRunner>();
            }
            var pendingValue = new PendingFrames { Id = id, Request = request, Hash = hash, Revision = live.Revision,
                Deadline = now + 30, Width = Screen.width, Height = Screen.height };
            pendingScreens.Add(id, pendingValue);
            captureRunner.Capture(texture =>
            {
                if (disposed || !pendingScreens.Remove(id)) return;
                RawSceneLoader.LiveSession current = session();
                if (current == null || current.Revision != pendingValue.Revision || current.LastReloadRestartRequired)
                { Reply(request, hash, null, "stale_revision", "Scene changed before screen capture completed."); return; }
                if (texture.width != pendingValue.Width || texture.height != pendingValue.Height)
                { Reply(request, hash, null, "viewport_changed", "Viewport dimensions changed before screen capture completed."); return; }
                try { Reply(request, hash, SaveCapture(texture, "screen", true), null, null); }
                catch (Exception error) { Reply(request, hash, null, "outcome_unknown", "Screen capture publication failed: " + error.Message); }
            }, error =>
            {
                if (disposed || !pendingScreens.Remove(id)) return;
                Reply(request, hash, null, "capture_failed", "Public end-of-frame screen capture failed: " + error.Message);
            });
        }

        private object CameraCapture(Dictionary<string, object> arguments, RawSceneLoader.LiveSession live)
        {
            RequireGraphics();
            Camera camera;
            if (arguments.TryGetValue("target", out object rawTarget))
            { if (!(rawTarget is Dictionary<string, object> target)) throw new InvalidDataException("Camera target must be a source object reference."); camera = FindObject(target, live).GetComponent<Camera>(); }
            else if (!string.IsNullOrEmpty(Text(arguments, "camera"))) camera = FindObject(new Dictionary<string, object> { ["path"] = Text(arguments, "camera") }, live).GetComponent<Camera>();
            else camera = Camera.main;
            if (camera == null) throw new InvalidOperationException("Camera mode requires an unambiguous live Camera target or MainCamera.");
            long requestedWidth = Integer(arguments, "width"), requestedHeight = Integer(arguments, "height");
            int width = requestedWidth == 0 ? Screen.width : checked((int)requestedWidth), height = requestedHeight == 0 ? Screen.height : checked((int)requestedHeight);
            ValidateCaptureSize(width, height);
            var render = new RenderTexture(width, height, 24, RenderTextureFormat.ARGB32);
            var previousTarget = camera.targetTexture; var previousActive = RenderTexture.active;
            Texture2D texture = null;
            bool renderingBegan = false;
            try
            {
                if (UnityEngine.Rendering.GraphicsSettings.currentRenderPipeline != null)
                {
                    var capture = new UnityEngine.Rendering.RenderPipeline.StandardRequest { destination = render };
                    renderingBegan = true;
                    if (!UnityEngine.Rendering.RenderPipeline.SupportsRenderRequest(camera, capture)) throw new InvalidOperationException("The current render pipeline does not support public standard camera render requests.");
                    UnityEngine.Rendering.RenderPipeline.SubmitRenderRequest(camera, capture);
                }
                else { camera.targetTexture = render; renderingBegan = true; camera.Render(); }
                RenderTexture.active = render;
                texture = new Texture2D(width, height, TextureFormat.RGBA32, false);
                texture.ReadPixels(new Rect(0, 0, width, height), 0, 0); texture.Apply();
                var result = SaveCapture(texture, "camera", false); result["cameraPath"] = AgentRuntimeSnapshot.PathOf(camera.transform);
                return result;
            }
            catch (Exception error)
            { if (renderingBegan) throw new AgentCaptureExecutionException("Camera capture failed after public render execution began: " + error.Message, error); throw; }
            finally { camera.targetTexture = previousTarget; RenderTexture.active = previousActive; if (texture != null) Object.Destroy(texture); render.Release(); Object.Destroy(render); }
        }

        private Dictionary<string, object> SaveCapture(Texture2D texture, string mode, bool includesOverlay)
        {
            ValidateCaptureSize(texture.width, texture.height);
            byte[] png = texture.EncodeToPNG(); if (png == null || png.Length > 64 * 1024 * 1024) throw new InvalidDataException("PNG output exceeds capture bounds.");
            string path = Path.Combine(directory, "capture-" + Guid.NewGuid().ToString("N") + ".png");
            CliPlayArtifact.WriteBytes(path, png);
            return new Dictionary<string, object> { ["path"] = path, ["sha256"] = Hash(png), ["width"] = texture.width, ["height"] = texture.height,
                ["frame"] = Time.frameCount, ["mode"] = mode, ["includesOverlayUi"] = includesOverlay,
                ["viewport"] = new Dictionary<string, object> { ["width"] = Screen.width, ["height"] = Screen.height } };
        }

        private static void ValidateCaptureSize(int width, int height)
        { if (width < 1 || height < 1 || width > 4096 || height > 4096) throw new InvalidDataException("Capture dimensions must be between 1 and 4096."); }
        private static void RequireGraphics()
        { if (SystemInfo.graphicsDeviceType == UnityEngine.Rendering.GraphicsDeviceType.Null) throw new InvalidOperationException("Screenshot requires an active graphics device."); }

        private static GameObject FindObject(Dictionary<string, object> args, RawSceneLoader.LiveSession live)
        {
            string path = Text(args, "path"), fileId = Text(args, "fileId"); GameObject found = null;
            if (string.IsNullOrEmpty(path) && string.IsNullOrEmpty(fileId)) throw new InvalidDataException("Inspect requires a path or source fileId.");
            foreach (GameObject value in SceneObjects())
            {
                Dictionary<string, object> source = live.DescribeSourceObject(value);
                bool match = !string.IsNullOrEmpty(fileId) ? source != null && Text(source, "fileId") == fileId : AgentRuntimeSnapshot.PathOf(value.transform) == path;
                if (!match) continue;
                if (found != null) throw new InvalidOperationException("Object reference is ambiguous."); found = value;
            }
            return found == null ? throw new InvalidOperationException("Object reference was not found.") : found;
        }

        private void Reply(Dictionary<string, object> request, string hash, object result, string code, string error)
        {
            string id = Text(request, "requestId"); if (!ValidId(id)) return;
            try { PublishReply(id, hash, result, code, error); }
            catch (Exception failure)
            {
                Warn("Cannot publish response " + id, failure);
                // Result serialization can fail after custom side effects.
                // Report uncertainty, without executing the handler again.
                try { PublishReply(id, hash, null, "outcome_unknown", "The request was accepted but its result could not be published. Inspect state before retrying."); }
                catch (Exception retryFailure) { Warn("Control response remains unavailable " + id, retryFailure); }
            }
        }

        private void PublishReply(string id, string hash, object result, string code, string error)
        {
            var value = new Dictionary<string, object>
            {
                ["schemaVersion"] = 1, ["requestId"] = id, ["requestSha256"] = hash, ["pid"] = pid,
                ["revision"] = session() == null ? string.Empty : session().Revision,
                ["success"] = code == null, ["result"] = result,
                ["error"] = code == null ? null : new Dictionary<string, object> { ["code"] = code, ["message"] = error == null ? string.Empty : error.Substring(0, Math.Min(error.Length, 8192)) },
            };
            string json = RawJson.Write(value);
            if (Encoding.UTF8.GetByteCount(json) > 4 * 1024 * 1024) throw new InvalidDataException("Runtime response exceeds supported bounds.");
            CliPlayArtifact.WriteText(ResponsePath(id), json + "\n");
        }

        private static void Warn(string context, Exception error)
        { string message = error.Message ?? string.Empty; Debug.LogWarning("[agent-control] " + context + ": " + message.Substring(0, Math.Min(message.Length, 1024))); }

        private void OnLog(string message, string stack, LogType type)
        {
            lock (logLock)
            {
                if (disposed) return; long sequence = ++logSequence;
                logs.Add(new Dictionary<string, object> { ["sequence"] = sequence, ["type"] = type.ToString(),
                    ["message"] = message == null ? string.Empty : message.Substring(0, Math.Min(message.Length, 8192)),
                    ["stackTrace"] = stack == null ? string.Empty : stack.Substring(0, Math.Min(stack.Length, 16384)) });
                if (logs.Count > 128) logs.RemoveAt(0);
            }
        }

        public void Dispose()
        {
            if (disposed) return; disposed = true;
            Application.logMessageReceivedThreaded -= OnLog;
            simulation.Dispose(); input.Dispose();
            if (captureRunner != null) Object.Destroy(captureRunner.gameObject);
            pendingScreens.Clear();
            if (paused) Time.timeScale = previousTimeScale;
        }

        private string ResponsePath(string id) { return Path.Combine(directory, id + ".response.json"); }
        private static bool ValidId(string value)
        { if (string.IsNullOrEmpty(value) || value.Length > 64) return false; foreach (char c in value) if (!char.IsLetterOrDigit(c) || c > 127) { if (c != '-') return false; } return true; }
        private static string Text(Dictionary<string, object> value, string key) { return value.TryGetValue(key, out object item) ? item as string ?? string.Empty : string.Empty; }
        private static long Integer(Dictionary<string, object> value, string key)
        { if (!value.TryGetValue(key, out object item)) return 0; if (!(item is long)) throw new InvalidDataException("Expected an integer: " + key); return (long)item; }
        private static float Number(Dictionary<string, object> value, string key, float fallback = 0)
        { if (!value.TryGetValue(key, out object item)) return fallback; if (!(item is long) && !(item is double)) throw new InvalidDataException("Expected a number: " + key); float result = Convert.ToSingle(item, CultureInfo.InvariantCulture); if (float.IsNaN(result) || float.IsInfinity(result)) throw new InvalidDataException("Expected a finite number: " + key); return result; }
        private static Vector3 ReadVector(Dictionary<string, object> value, string key)
        { if (!value.TryGetValue(key, out object item) || !(item is Dictionary<string, object> map)) throw new InvalidDataException("Expected a vector: " + key); return new Vector3(Number(map, "x"), Number(map, "y"), Number(map, "z")); }
        private static string Hash(byte[] bytes) { using (var hash = SHA256.Create()) return BitConverter.ToString(hash.ComputeHash(bytes)).Replace("-", string.Empty).ToLowerInvariant(); }
        private sealed class PendingFrames { internal string Id, Hash, Revision; internal Dictionary<string, object> Request; internal long Target; internal double Deadline; internal int Width, Height; }
        private sealed class AgentCaptureExecutionException : InvalidOperationException
        { internal AgentCaptureExecutionException(string message, Exception inner) : base(message, inner) { } }
    }
}
