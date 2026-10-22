using System;
using System.Collections.Generic;
using System.IO;
using System.Reflection;
using UnityEngine;

namespace UnityMicroEditor.PlayModeEditor
{
    // What the Editor answers in Play mode, for a Player whose code is
    // compiled as Play mode compiles it (with UNITY_EDITOR). The tool's linker
    // (tools/PlayModeLinker) writes stand-ins of the Editor's assemblies; a
    // stand-in member calls the public static method of the same name and
    // parameters in the class named after its type (`UnityEditor.EditorPrefs`
    // is `UnityEditor_EditorPrefs`), an instance member with the instance
    // first. Any other member of the Editor returns its type's default value.

    public static class UnityEditor_EditorApplication
    {
        public static bool get_isPlaying() => true;
        public static void set_isPlaying(bool value)
        {
            if (!value) LeavePlayMode();
        }
        public static bool get_isPlayingOrWillChangePlaymode() => true;
        public static bool get_isPaused() => false;
        public static bool get_isCompiling() => false;
        public static bool get_isUpdating() => false;
        public static double get_timeSinceStartup() => Time.realtimeSinceStartupAsDouble;

        /// <summary>
        /// What the code adds to <c>playModeStateChanged</c> hears that Play mode was entered once the
        /// first scene woke (<c>EditorLoop.EnteredPlayMode</c>), and that it is left as the Player quits.
        /// </summary>
        public static void add_playModeStateChanged(Delegate value) => EditorLoop.PlayModeStateChanged = Delegate.Combine(EditorLoop.PlayModeStateChanged, value);
        public static void remove_playModeStateChanged(Delegate value) => EditorLoop.PlayModeStateChanged = Delegate.Remove(EditorLoop.PlayModeStateChanged, value);
        public static void ExitPlaymode() => LeavePlayMode();

        /// <summary>
        /// The Editor's tools leave Play mode on their own checks (Addressables without a content
        /// build of the active target, Zenject without a scene's parents); the session keeps its
        /// Player, and says who asked.
        /// </summary>
        static void LeavePlayMode() =>
            Debug.LogWarning("[unity-micro-editor] the game's Editor code asked to leave Play mode; the Player keeps running.\n" + Environment.StackTrace);
    }

    /// <summary>
    /// What the asset database knows of the project's own files. A Player of Play mode code runs in
    /// the project's directory, as the Editor does, so the paths the code names (<c>Assets/...</c>)
    /// are the files themselves; the identity of each is its <c>.meta</c> file's. Packages, the
    /// assets' types and their loading are the Editor's alone.
    /// </summary>
    public static class UnityEditor_AssetDatabase
    {
        static Dictionary<string, string> paths;

        public static bool IsValidFolder(string path) =>
            !string.IsNullOrEmpty(path) && (path == "Assets" || path.StartsWith("Assets/", StringComparison.Ordinal)) && Directory.Exists(path);

        public static string AssetPathToGUID(string path) => string.IsNullOrEmpty(path) ? string.Empty : GuidOf(path + ".meta");

        public static string GUIDToAssetPath(string guid) =>
            guid != null && Paths().TryGetValue(guid, out string path) ? path : string.Empty;

        /// <summary>
        /// An asset of a <c>Resources</c> folder, as the Player holds it; the Player has no other
        /// asset by its path.
        /// </summary>
        public static UnityEngine.Object LoadAssetAtPath(string path, Type type) => Load(path, type ?? typeof(UnityEngine.Object));

        public static T LoadAssetAtPath<T>(string path) where T : UnityEngine.Object => Load(path, typeof(T)) as T;

        public static UnityEngine.Object LoadMainAssetAtPath(string path) => Load(path, typeof(UnityEngine.Object));

        static UnityEngine.Object Load(string path, Type type)
        {
            if (string.IsNullOrEmpty(path)) return null;
            const string folder = "/Resources/";
            int at = path.LastIndexOf(folder, StringComparison.Ordinal);
            if (at < 0) return null;
            string inside = path.Substring(at + folder.Length);
            string extension = Path.GetExtension(inside);
            if (extension.Length > 0) inside = inside.Substring(0, inside.Length - extension.Length);
            // Resources names an asset without its extension; the file's own main asset is the
            // one of the type its kind of file has.
            Type main = type == typeof(UnityEngine.Object) ? MainType(extension) ?? type : type;
            UnityEngine.Object asset = Resources.Load(inside, main);
            return asset != null && type.IsInstanceOfType(asset) ? asset : null;
        }

        static Type MainType(string extension)
        {
            switch (extension.ToLowerInvariant())
            {
                case ".prefab": case ".fbx": case ".obj": case ".blend": return typeof(GameObject);
                case ".mat": return typeof(Material);
                case ".png": case ".jpg": case ".jpeg": case ".tga": case ".psd": case ".tif": case ".tiff": case ".exr": case ".bmp": return typeof(Texture2D);
                case ".anim": return typeof(AnimationClip);
                case ".controller": case ".overridecontroller": return typeof(RuntimeAnimatorController);
                case ".shader": return typeof(Shader);
                case ".wav": case ".mp3": case ".ogg": case ".aif": case ".aiff": return typeof(AudioClip);
                case ".txt": case ".json": case ".bytes": case ".xml": case ".csv": return typeof(TextAsset);
                case ".ttf": case ".otf": return typeof(Font);
                default: return null;
            }
        }

        public static string[] GetAllAssetPaths()
        {
            var all = new List<string>(Paths().Values);
            all.Sort(StringComparer.Ordinal);
            return all.ToArray();
        }

        static Dictionary<string, string> Paths()
        {
            if (paths != null) return paths;
            paths = new Dictionary<string, string>(StringComparer.Ordinal);
            if (!Directory.Exists("Assets")) return paths;
            foreach (string meta in Directory.EnumerateFiles("Assets", "*.meta", SearchOption.AllDirectories))
            {
                string guid = GuidOf(meta);
                if (guid.Length > 0 && !paths.ContainsKey(guid))
                    paths.Add(guid, meta.Substring(0, meta.Length - ".meta".Length).Replace('\\', '/'));
            }
            return paths;
        }

        static string GuidOf(string meta)
        {
            try
            {
                foreach (string line in File.ReadLines(meta))
                    if (line.StartsWith("guid:", StringComparison.Ordinal)) return line.Substring(5).Trim();
            }
            catch (IOException) { }
            catch (UnauthorizedAccessException) { }
            return string.Empty;
        }
    }

    /// <summary>
    /// The build target the Editor has active: the one the project's code is compiled for
    /// (<c>--compile-for</c>), which the runner names in <c>UME_PLAY_MODE_BUILD_TARGET</c> as
    /// <c>UnityEditor.BuildTarget</c> numbers it.
    /// </summary>
    public static class UnityEditor_EditorUserBuildSettings
    {
        public static int get_activeBuildTarget() => ActiveTarget();
        public static int get_selectedBuildTargetGroup() => UnityEditor_BuildPipeline.GetBuildTargetGroup(ActiveTarget());
        public static int get_activeBuildTargetGroup() => UnityEditor_BuildPipeline.GetBuildTargetGroup(ActiveTarget());

        internal static int ActiveTarget()
        {
            string named = Environment.GetEnvironmentVariable("UME_PLAY_MODE_BUILD_TARGET");
            return int.TryParse(named, out int target) ? target : 24; // StandaloneLinux64
        }
    }

    public static class UnityEditor_BuildPipeline
    {
        /// <summary>The <c>BuildTargetGroup</c> of a <c>BuildTarget</c>, of the platforms a project is compiled for.</summary>
        public static int GetBuildTargetGroup(int target)
        {
            switch (target)
            {
                case 13: return 7; // Android
                case 9: return 4; // iOS
                case 20: return 13; // WebGL
                case 2: case 5: case 19: case 24: return 1; // Standalone
                default: return 0;
            }
        }
    }

    /// <summary>The engine's own members whose answer Play mode gives otherwise, for the game's code.</summary>
    public static class UnityEngine_Application
    {
        /// <summary>The Editor ignores a quit in Play mode; the session keeps its Player, and says who asked.</summary>
        public static void Quit() => Quit(0);

        public static void Quit(int exitCode) =>
            Debug.LogWarning("[unity-micro-editor] the game's code quit (" + exitCode + "), which Play mode ignores.\n" + Environment.StackTrace);
    }

    public static class UnityEditor_PlayerSettings
    {
        public static string get_productName() => Application.productName;
        public static string get_companyName() => Application.companyName;
        public static string get_bundleVersion() => Application.version;
    }

    /// <summary>Preferences of this run only; the Editor's own are the user's, and the Player does not read them.</summary>
    public static class UnityEditor_EditorPrefs
    {
        static readonly Dictionary<string, object> values = new Dictionary<string, object>();

        public static bool HasKey(string key) => values.ContainsKey(key);
        public static void DeleteKey(string key) => values.Remove(key);
        public static void DeleteAll() => values.Clear();
        public static bool GetBool(string key) => Get(key, false);
        public static bool GetBool(string key, bool defaultValue) => Get(key, defaultValue);
        public static void SetBool(string key, bool value) => values[key] = value;
        public static int GetInt(string key) => Get(key, 0);
        public static int GetInt(string key, int defaultValue) => Get(key, defaultValue);
        public static void SetInt(string key, int value) => values[key] = value;
        public static float GetFloat(string key) => Get(key, 0f);
        public static float GetFloat(string key, float defaultValue) => Get(key, defaultValue);
        public static void SetFloat(string key, float value) => values[key] = value;
        public static string GetString(string key) => Get(key, string.Empty);
        public static string GetString(string key, string defaultValue) => Get(key, defaultValue);
        public static void SetString(string key, string value) => values[key] = value;

        static T Get<T>(string key, T defaultValue) => values.TryGetValue(key, out object value) && value is T known ? known : defaultValue;
    }

    /// <summary>The state an Editor session keeps; a run of the Player is one session.</summary>
    public static class UnityEditor_SessionState
    {
        static readonly Dictionary<string, object> values = new Dictionary<string, object>();

        public static bool GetBool(string key, bool defaultValue) => Get(key, defaultValue);
        public static void SetBool(string key, bool value) => values[key] = value;
        public static void EraseBool(string key) => values.Remove(key);
        public static int GetInt(string key, int defaultValue) => Get(key, defaultValue);
        public static void SetInt(string key, int value) => values[key] = value;
        public static void EraseInt(string key) => values.Remove(key);
        public static float GetFloat(string key, float defaultValue) => Get(key, defaultValue);
        public static void SetFloat(string key, float value) => values[key] = value;
        public static void EraseFloat(string key) => values.Remove(key);
        public static string GetString(string key, string defaultValue) => Get(key, defaultValue);
        public static void SetString(string key, string value) => values[key] = value;
        public static void EraseString(string key) => values.Remove(key);

        static T Get<T>(string key, T defaultValue) => values.TryGetValue(key, out object value) && value is T known ? known : defaultValue;
    }

    /// <summary>
    /// Calls what the code put in <c>EditorApplication.update</c> each frame and
    /// what it put in <c>EditorApplication.delayCall</c> once, as the Editor does
    /// in Play mode. Both are fields of the stand-in of <c>UnityEditor.CoreModule</c>,
    /// which exists only when the code names them.
    /// </summary>
    internal static class EditorLoop
    {
        internal static Delegate PlayModeStateChanged;
        static bool entered;
        static bool searched;

        // UnityEditor.PlayModeStateChange
        const int EnteredPlayModeState = 2, ExitingPlayModeState = 3;

        /// <summary>Tells <c>playModeStateChanged</c> that Play mode was entered, once.</summary>
        internal static void EnteredPlayMode()
        {
            if (entered) return;
            entered = true;
            Application.quitting += () => ChangePlayModeState(ExitingPlayModeState);
            ChangePlayModeState(EnteredPlayModeState);
        }

        static void ChangePlayModeState(int state)
        {
            if (PlayModeStateChanged == null) return;
            foreach (Delegate callback in PlayModeStateChanged.GetInvocationList())
            {
                ParameterInfo[] parameters = callback.Method.GetParameters();
                if (parameters.Length != 1 || !parameters[0].ParameterType.IsEnum) continue;
                Invoke(callback, Enum.ToObject(parameters[0].ParameterType, state));
            }
        }
        static FieldInfo update;
        static FieldInfo delayCall;

        internal static void Tick()
        {
            if (!searched)
            {
                searched = true;
                Type application = Type.GetType("UnityEditor.EditorApplication, UnityEditor.CoreModule", false);
                if (application == null) return;
                const BindingFlags flags = BindingFlags.Public | BindingFlags.Static;
                update = application.GetField("update", flags);
                delayCall = application.GetField("delayCall", flags);
            }
            if (delayCall != null && delayCall.GetValue(null) is Delegate once)
            {
                delayCall.SetValue(null, null);
                foreach (Delegate callback in once.GetInvocationList()) Invoke(callback);
            }
            if (update != null && update.GetValue(null) is Delegate each)
                foreach (Delegate callback in each.GetInvocationList()) Invoke(callback);
        }

        static void Invoke(Delegate callback, params object[] arguments)
        {
            try
            {
                callback.DynamicInvoke(arguments);
            }
            catch (TargetInvocationException exception)
            {
                Debug.LogException(exception.InnerException ?? exception);
            }
        }
    }
}
