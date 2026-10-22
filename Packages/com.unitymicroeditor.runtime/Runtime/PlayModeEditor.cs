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
    /// What the asset database knows of the project's files. A Player of Play mode code runs in the
    /// project's directory, as the Editor does, so a path below <c>Assets</c> is the file itself; a
    /// package's (<c>Packages/&lt;name&gt;/...</c>) lies where the session found the package, which the
    /// runner names in <c>UME_PLAY_MODE_PACKAGES</c>. The identity and labels of each are its
    /// <c>.meta</c> file's. An asset is loaded as the session holds it: from a <c>Resources</c>
    /// folder, among the assets the session wrote, or written from its source on the first request
    /// (<c>UME_PLAY_MODE_ASSETS</c>), which blocks the frame as the Editor's import would.
    /// </summary>
    public static class UnityEditor_AssetDatabase
    {
        sealed class Entry
        {
            internal string guid;
            internal string path;
            internal string[] labels;
        }

        static List<KeyValuePair<string, string>> roots;
        static Dictionary<string, Entry> entries;
        static readonly HashSet<string> refused = new HashSet<string>(StringComparer.Ordinal);

        public static bool IsValidFolder(string path)
        {
            string physical = Physical(path);
            return physical != null && Directory.Exists(physical);
        }

        public static string AssetPathToGUID(string path)
        {
            string physical = Physical(path);
            return physical == null ? string.Empty : Meta(physical + ".meta").guid;
        }

        public static string GUIDToAssetPath(string guid) =>
            guid != null && Entries().TryGetValue(guid, out Entry entry) ? entry.path : string.Empty;

        public static string[] GetAllAssetPaths()
        {
            var all = new List<string>();
            foreach (Entry entry in Entries().Values) all.Add(entry.path);
            all.Sort(StringComparer.Ordinal);
            return all.ToArray();
        }

        public static string[] FindAssets(string filter) => FindAssets(filter, null);

        /// <summary>
        /// The GUIDs of the assets a search finds: every term holds, <c>l:</c> a label of the asset's
        /// <c>.meta</c>, <c>t:</c> its main type (by its kind of file, or for an asset of a script by
        /// the script's name), any other a part of its name.
        /// </summary>
        public static string[] FindAssets(string filter, string[] searchInFolders)
        {
            var labels = new List<string>();
            var types = new List<string>();
            var names = new List<string>();
            foreach (string term in (filter ?? string.Empty).Split(new[] { ' ' }, StringSplitOptions.RemoveEmptyEntries))
            {
                if (term.StartsWith("l:", StringComparison.Ordinal)) labels.Add(term.Substring(2));
                else if (term.StartsWith("t:", StringComparison.Ordinal)) types.Add(term.Substring(2));
                else names.Add(term);
            }
            var found = new List<KeyValuePair<string, string>>();
            foreach (var item in Entries())
            {
                Entry entry = item.Value;
                string physical = Physical(entry.path);
                if (physical == null || Directory.Exists(physical)) continue;
                if (searchInFolders != null && searchInFolders.Length != 0 && !Array.Exists(searchInFolders,
                    folder => entry.path.StartsWith(folder.TrimEnd('/') + "/", StringComparison.Ordinal))) continue;
                if (labels.Count != 0 && !labels.Exists(label => Array.IndexOf(entry.labels, label) >= 0)) continue;
                string name = Path.GetFileNameWithoutExtension(entry.path);
                if (!names.TrueForAll(part => name.IndexOf(part, StringComparison.OrdinalIgnoreCase) >= 0)) continue;
                if (types.Count != 0 && !types.Exists(type => OfType(entry.path, physical, type))) continue;
                found.Add(new KeyValuePair<string, string>(entry.path, item.Key));
            }
            found.Sort((a, b) => string.CompareOrdinal(a.Key, b.Key));
            return found.ConvertAll(pair => pair.Value).ToArray();
        }

        public static UnityEngine.Object LoadAssetAtPath(string path, Type type) => Load(path, type ?? typeof(UnityEngine.Object));

        public static T LoadAssetAtPath<T>(string path) where T : UnityEngine.Object => Load(path, typeof(T)) as T;

        public static UnityEngine.Object LoadMainAssetAtPath(string path) => Load(path, typeof(UnityEngine.Object));

        static UnityEngine.Object Load(string path, Type type)
        {
            if (string.IsNullOrEmpty(path)) return null;
            string extension = Path.GetExtension(path);
            // The file's own main asset is the one of the type its kind of file has.
            Type main = type == typeof(UnityEngine.Object) ? MainType(extension) ?? type : type;
            const string folder = "/Resources/";
            int at = path.LastIndexOf(folder, StringComparison.Ordinal);
            if (at >= 0)
            {
                // Resources names an asset without its extension.
                string inside = path.Substring(at + folder.Length);
                if (extension.Length > 0) inside = inside.Substring(0, inside.Length - extension.Length);
                UnityEngine.Object held = Resources.Load(inside, main);
                if (held != null && type.IsInstanceOfType(held)) return held;
            }
            string guid = AssetPathToGUID(path);
            var session = UnityMicroEditor.Raw.RawSceneLoader.Current;
            if (guid.Length == 0 || session == null) return null;
            if (session.TryGetAsset(guid, main, out UnityEngine.Object asset)) return asset;
            if (!Written(session, path, guid)) return null;
            return session.TryGetAsset(guid, main, out asset) ? asset : null;
        }

        [Serializable]
        sealed class WrittenAssets
        {
            public string error;
            public UnityMicroEditor.Raw.NativeContentFileDescriptor[] nativeContentFiles;
            public UnityMicroEditor.Raw.NativeSceneAliasDescriptor[] aliases;
        }

        /// <summary>Has the session write an asset from its source and loads it, once per asset.</summary>
        static bool Written(UnityMicroEditor.Raw.RawSceneLoader.LiveSession session, string path, string guid)
        {
            string directory = Environment.GetEnvironmentVariable("UME_PLAY_MODE_ASSETS");
            if (string.IsNullOrEmpty(directory) || !refused.Add(guid)) return false;
            Debug.Log("[unity-micro-editor] the game's Editor code loads " + path + ", which the session writes from its source now and holds from the start of its next Player.");
            string request = Path.Combine(directory, guid + ".request.json");
            string response = Path.Combine(directory, guid + ".response.json");
            try
            {
                Directory.CreateDirectory(directory);
                File.Delete(response);
                File.WriteAllText(request + ".tmp", "{\"schemaVersion\":1,\"path\":\"" + path.Replace("\\", "\\\\").Replace("\"", "\\\"") + "\"}");
                File.Move(request + ".tmp", request);
                var waited = System.Diagnostics.Stopwatch.StartNew();
                while (!File.Exists(response))
                {
                    if (waited.Elapsed.TotalSeconds > 900) throw new TimeoutException("the session wrote nothing in 900 s");
                    System.Threading.Thread.Sleep(20);
                }
                var written = JsonUtility.FromJson<WrittenAssets>(File.ReadAllText(response));
                if (written == null || !string.IsNullOrEmpty(written.error))
                    throw new InvalidDataException(written == null ? "an empty answer" : written.error);
                session.LoadSourceAssets(written.nativeContentFiles, written.aliases);
                refused.Remove(guid);
                return true;
            }
            catch (Exception error)
            {
                Debug.LogWarning("[unity-micro-editor] " + path + " cannot be loaded from its source: " + error.Message);
                return false;
            }
        }

        static bool OfType(string path, string physical, string type)
        {
            Type main = MainType(Path.GetExtension(path));
            if (main != null) return main.Name == type || main.FullName == type;
            if (!path.EndsWith(".asset", StringComparison.OrdinalIgnoreCase)) return false;
            // An asset of a script names it; its class is named as its file.
            try
            {
                foreach (string line in File.ReadLines(physical))
                {
                    int at = line.IndexOf("m_Script: {fileID: 11500000, guid: ", StringComparison.Ordinal);
                    if (at < 0) continue;
                    string script = line.Substring(at + 36, 32);
                    return Path.GetFileNameWithoutExtension(GUIDToAssetPath(script)) == type;
                }
            }
            catch (IOException) { }
            catch (UnauthorizedAccessException) { }
            return false;
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

        /// <summary>The file a logical path names, or none.</summary>
        internal static string Physical(string path)
        {
            if (string.IsNullOrEmpty(path)) return null;
            path = path.Replace('\\', '/').TrimEnd('/');
            foreach (var root in Roots())
                if (path == root.Key || path.StartsWith(root.Key + "/", StringComparison.Ordinal))
                    return root.Value + path.Substring(root.Key.Length);
            return null;
        }

        /// <summary>Where <c>Assets</c> and each package (<c>Packages/&lt;name&gt;</c>) lie.</summary>
        internal static List<KeyValuePair<string, string>> Roots()
        {
            if (roots != null) return roots;
            roots = new List<KeyValuePair<string, string>> { new KeyValuePair<string, string>("Assets", Path.GetFullPath("Assets")) };
            string list = Environment.GetEnvironmentVariable("UME_PLAY_MODE_PACKAGES");
            if (!string.IsNullOrEmpty(list) && File.Exists(list))
                foreach (string line in File.ReadAllLines(list))
                {
                    int tab = line.IndexOf('\t');
                    if (tab > 0) roots.Add(new KeyValuePair<string, string>(line.Substring(0, tab), line.Substring(tab + 1)));
                }
            return roots;
        }

        static Dictionary<string, Entry> Entries()
        {
            if (entries != null) return entries;
            entries = new Dictionary<string, Entry>(StringComparer.Ordinal);
            foreach (var root in Roots())
            {
                if (!Directory.Exists(root.Value)) continue;
                foreach (string meta in Directory.EnumerateFiles(root.Value, "*.meta", SearchOption.AllDirectories))
                {
                    Entry entry = Meta(meta);
                    entry.path = root.Key + meta.Substring(root.Value.Length, meta.Length - root.Value.Length - ".meta".Length).Replace('\\', '/');
                    if (entry.guid.Length > 0 && !entries.ContainsKey(entry.guid)) entries.Add(entry.guid, entry);
                }
            }
            return entries;
        }

        static Entry Meta(string meta)
        {
            string guid = string.Empty;
            var labels = new List<string>();
            try
            {
                bool inLabels = false;
                foreach (string line in File.ReadLines(meta))
                {
                    if (line.StartsWith("guid:", StringComparison.Ordinal)) guid = line.Substring(5).Trim();
                    else if (line.StartsWith("labels:", StringComparison.Ordinal)) inLabels = true;
                    else if (inLabels && line.StartsWith("- ", StringComparison.Ordinal)) labels.Add(line.Substring(2).Trim());
                    else inLabels = false;
                }
            }
            catch (IOException) { }
            catch (UnauthorizedAccessException) { }
            return new Entry { guid = guid, labels = labels.ToArray() };
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

        /// <summary>
        /// The Editor's serialized object of the Player settings, which tools reach by reflection
        /// (Hive's splash of the active platform): the settings of
        /// <c>ProjectSettings/ProjectSettings.asset</c> as they are saved.
        /// </summary>
        public static object GetSerializedObject() => Serialized.Settings("ProjectSettings/ProjectSettings.asset");
    }

    /// <summary>
    /// A <c>SerializedObject</c> of an asset saved as text: the properties of its first object's top
    /// level, as the file holds them. A reference is loaded as the asset database loads it; what
    /// is changed is not written back.
    /// </summary>
    public static class UnityEditor_SerializedObject
    {
        public static void Update(object self) { }
        public static bool UpdateIfRequiredOrScript(object self) => false;
        public static bool ApplyModifiedProperties(object self) => false;
        public static bool ApplyModifiedPropertiesWithoutUndo(object self) => false;
        public static object FindProperty(object self, string propertyPath) => Serialized.Find(self, propertyPath);
    }

    public static class UnityEditor_SerializedProperty
    {
        // UnityEditor.SerializedPropertyType
        const int Generic = -1, Integer = 0, Float = 2, Text = 3, Color = 4, ObjectReference = 5;

        public static string get_name(object self) => Serialized.Of(self).Key;
        public static string get_propertyPath(object self) => Serialized.Of(self).Key;

        /// <summary>What the saved text says: a reference, a whole number, a number, a colour, a text, or something else.</summary>
        public static int get_propertyType(object self)
        {
            string value = Serialized.Of(self).Value;
            if (value.StartsWith("{fileID:", StringComparison.Ordinal)) return ObjectReference;
            if (value.StartsWith("{r:", StringComparison.Ordinal)) return Color;
            if (value.Length == 0 || value.StartsWith("{", StringComparison.Ordinal) || value.StartsWith("[", StringComparison.Ordinal)) return Generic;
            if (long.TryParse(value, System.Globalization.NumberStyles.Integer, System.Globalization.CultureInfo.InvariantCulture, out _)) return Integer;
            if (double.TryParse(value, System.Globalization.NumberStyles.Float, System.Globalization.CultureInfo.InvariantCulture, out _)) return Float;
            return Text;
        }

        public static bool get_boolValue(object self) => Serialized.Of(self).Value == "1";
        public static int get_intValue(object self) =>
            int.TryParse(Serialized.Of(self).Value, System.Globalization.NumberStyles.Integer, System.Globalization.CultureInfo.InvariantCulture, out int value) ? value : 0;
        public static long get_longValue(object self) =>
            long.TryParse(Serialized.Of(self).Value, System.Globalization.NumberStyles.Integer, System.Globalization.CultureInfo.InvariantCulture, out long value) ? value : 0;
        public static float get_floatValue(object self) =>
            float.TryParse(Serialized.Of(self).Value, System.Globalization.NumberStyles.Float, System.Globalization.CultureInfo.InvariantCulture, out float value) ? value : 0;
        public static string get_stringValue(object self) => Serialized.Of(self).Value;

        /// <summary>The asset a reference names by its GUID, loaded as the asset database loads it.</summary>
        public static UnityEngine.Object get_objectReferenceValue(object self)
        {
            string value = Serialized.Of(self).Value;
            int at = value.IndexOf("guid: ", StringComparison.Ordinal);
            if (at < 0) return null;
            string guid = value.Substring(at + 6, Math.Min(32, value.Length - at - 6));
            string path = UnityEditor_AssetDatabase.GUIDToAssetPath(guid);
            return path.Length == 0 ? null : UnityEditor_AssetDatabase.LoadMainAssetAtPath(path);
        }
    }

    /// <summary>The state of the stand-ins of the Editor's serialized objects and properties.</summary>
    internal static class Serialized
    {
        static readonly System.Runtime.CompilerServices.ConditionalWeakTable<object, Dictionary<string, string>> objects =
            new System.Runtime.CompilerServices.ConditionalWeakTable<object, Dictionary<string, string>>();
        static readonly System.Runtime.CompilerServices.ConditionalWeakTable<object, Holder> properties =
            new System.Runtime.CompilerServices.ConditionalWeakTable<object, Holder>();

        sealed class Holder { internal KeyValuePair<string, string> Property; }

        internal static object Settings(string path)
        {
            object made = New("UnityEditor.SerializedObject");
            if (made == null || !File.Exists(path)) return null;
            objects.Add(made, TopLevel(path));
            return made;
        }

        internal static object Find(object serializedObject, string name)
        {
            if (serializedObject == null || name == null || !objects.TryGetValue(serializedObject, out var values)
                || !values.TryGetValue(name, out string value)) return null;
            object made = New("UnityEditor.SerializedProperty");
            if (made != null) properties.Add(made, new Holder { Property = new KeyValuePair<string, string>(name, value) });
            return made;
        }

        internal static KeyValuePair<string, string> Of(object property) =>
            property != null && properties.TryGetValue(property, out Holder holder) ? holder.Property : new KeyValuePair<string, string>(string.Empty, string.Empty);

        /// <summary>An instance of a stand-in type of the Editor, made without its constructor.</summary>
        internal static object New(string fullName)
        {
            Type type = Type.GetType(fullName + ", UnityEditor.CoreModule", false);
            return type == null ? null : System.Runtime.Serialization.FormatterServices.GetUninitializedObject(type);
        }

        /// <summary>The keys of the first object of a text asset, each with its value; a flow mapping over several lines is joined.</summary>
        static Dictionary<string, string> TopLevel(string path)
        {
            var values = new Dictionary<string, string>(StringComparer.Ordinal);
            string key = null, value = null;
            foreach (string line in File.ReadLines(path))
            {
                if (line.StartsWith("--- ", StringComparison.Ordinal) && values.Count != 0) break;
                if (key != null && value.StartsWith("{", StringComparison.Ordinal) && !value.EndsWith("}", StringComparison.Ordinal))
                {
                    value += " " + line.Trim();
                    values[key] = value;
                    continue;
                }
                if (line.Length < 3 || line[0] != ' ' || line[1] != ' ' || line[2] == ' ' || line[2] == '-') continue;
                int colon = line.IndexOf(':');
                if (colon < 0) continue;
                key = line.Substring(2, colon - 2);
                value = line.Substring(colon + 1).Trim();
                values[key] = value;
            }
            return values;
        }
    }

    /// <summary>
    /// The Package Manager's list of the project's packages, which the Editor answers from what it
    /// resolved: the packages of <c>Packages/packages-lock.json</c> (those the manifest names, or all
    /// of them with the indirect ones), each with its <c>package.json</c>. The request is complete at
    /// once. No other request of the Package Manager is answered.
    /// </summary>
    public static class UnityEditor_PackageManager_Client
    {
        public static object List() => Packages.List(false);
        public static object List(bool offlineMode) => Packages.List(false);
        public static object List(bool offlineMode, bool includeIndirectDependencies) => Packages.List(includeIndirectDependencies);
    }

    public static class UnityEditor_PackageManager_Requests_Request
    {
        public static bool get_IsCompleted(object self) => Packages.Answered(self);
        // UnityEditor.PackageManager.StatusCode: InProgress, Success, Failure
        public static int get_Status(object self) => Packages.Answered(self) ? 1 : 0;
    }

    public static class UnityEditor_PackageManager_Requests_Request_1
    {
        public static object get_Result(object self) => Packages.Result(self);
    }

    public static class UnityEditor_PackageManager_PackageCollection
    {
        public static object GetEnumerator(object self) => Packages.Enumerator(self);
    }

    public static class UnityEditor_PackageManager_PackageInfo
    {
        public static string get_name(object self) => Packages.Info(self, "name");
        public static string get_version(object self) => Packages.Info(self, "version");
        public static string get_displayName(object self) => Packages.Info(self, "displayName");
        public static string get_description(object self) => Packages.Info(self, "description");
        public static string get_packageId(object self) => Packages.Info(self, "name") + "@" + Packages.Info(self, "version");
        public static string get_assetPath(object self) => "Packages/" + Packages.Info(self, "name");
        public static string get_resolvedPath(object self) => Packages.Info(self, "resolvedPath");
    }

    internal static class Packages
    {
        static readonly System.Runtime.CompilerServices.ConditionalWeakTable<object, object> results =
            new System.Runtime.CompilerServices.ConditionalWeakTable<object, object>();
        static readonly System.Runtime.CompilerServices.ConditionalWeakTable<object, List<object>> collections =
            new System.Runtime.CompilerServices.ConditionalWeakTable<object, List<object>>();
        static readonly System.Runtime.CompilerServices.ConditionalWeakTable<object, Dictionary<string, string>> infos =
            new System.Runtime.CompilerServices.ConditionalWeakTable<object, Dictionary<string, string>>();

        internal static object List(bool indirect)
        {
            object request = Serialized.New("UnityEditor.PackageManager.Requests.ListRequest");
            object collection = Serialized.New("UnityEditor.PackageManager.PackageCollection");
            if (request == null || collection == null) return request;
            var packages = new List<object>();
            var directories = new Dictionary<string, string>(StringComparer.Ordinal);
            foreach (var root in UnityEditor_AssetDatabase.Roots()) directories[root.Key] = root.Value;
            try
            {
                var lockFile = UnityMicroEditor.Raw.RawJson.Parse(File.ReadAllText("Packages/packages-lock.json")) as Dictionary<string, object>;
                if (lockFile != null && lockFile.TryGetValue("dependencies", out object listed) && listed is Dictionary<string, object> dependencies)
                {
                    var names = new List<string>(dependencies.Keys);
                    names.Sort(StringComparer.Ordinal);
                    foreach (string name in names)
                    {
                        var entry = dependencies[name] as Dictionary<string, object>;
                        if (!indirect && entry != null && entry.TryGetValue("depth", out object depth) && Convert.ToInt64(depth) != 0) continue;
                        var info = new Dictionary<string, string>(StringComparer.Ordinal) { ["name"] = name };
                        if (entry != null && entry.TryGetValue("version", out object version)) info["version"] = version as string ?? string.Empty;
                        if (directories.TryGetValue("Packages/" + name, out string directory))
                        {
                            info["resolvedPath"] = directory;
                            string manifest = Path.Combine(directory, "package.json");
                            if (File.Exists(manifest) && UnityMicroEditor.Raw.RawJson.Parse(File.ReadAllText(manifest)) is Dictionary<string, object> package)
                                foreach (string field in new[] { "version", "displayName", "description" })
                                    if (package.TryGetValue(field, out object text) && text is string known) info[field] = known;
                        }
                        object made = Serialized.New("UnityEditor.PackageManager.PackageInfo");
                        if (made == null) continue;
                        infos.Add(made, info);
                        packages.Add(made);
                    }
                }
            }
            catch (Exception error)
            {
                Debug.LogWarning("[unity-micro-editor] the project's packages cannot be listed: " + error.Message);
            }
            collections.Add(collection, packages);
            results.Add(request, collection);
            return request;
        }

        internal static bool Answered(object request) => request != null && results.TryGetValue(request, out _);

        internal static object Result(object request) => request != null && results.TryGetValue(request, out object result) ? result : null;

        internal static object Enumerator(object collection)
        {
            Type element = Type.GetType("UnityEditor.PackageManager.PackageInfo, UnityEditor.CoreModule", false);
            if (element == null) return null;
            var typed = (System.Collections.IList)Activator.CreateInstance(typeof(List<>).MakeGenericType(element));
            if (collection != null && collections.TryGetValue(collection, out List<object> packages))
                foreach (object package in packages) typed.Add(package);
            return typed.GetType().GetMethod("GetEnumerator").Invoke(typed, null);
        }

        internal static string Info(object package, string field) =>
            package != null && infos.TryGetValue(package, out var info) && info.TryGetValue(field, out string value) ? value : string.Empty;
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
