using System;
using System.Collections.Generic;
using System.IO;
using System.Reflection;
using UnityEngine;

namespace UnityMicroEditor.PlayModeEditor
{
    /// <summary>
    /// The import context a scripted importer of the project is given when a Player of Play mode
    /// code runs it: its asset's path and what the importer adds.
    /// </summary>
    public static class UnityEditor_AssetImporters_AssetImportContext
    {
        public static string get_assetPath(object self) => ScriptedImports.Of(self).AssetPath;
        public static void AddObjectToAsset(object self, string identifier, UnityEngine.Object obj) => ScriptedImports.Of(self).Add(identifier, obj);
        public static void AddObjectToAsset(object self, string identifier, UnityEngine.Object obj, Texture2D thumbnail) => ScriptedImports.Of(self).Add(identifier, obj);
        public static void SetMainObject(object self, UnityEngine.Object obj) => ScriptedImports.Of(self).Main = obj;
        public static UnityEngine.Object get_mainObject(object self) => ScriptedImports.Of(self).Main;
    }

    /// <summary>
    /// Runs the project's scripted importers, which are the game's Editor code, in a Player of Play
    /// mode code when <c>UME_SCRIPTED_IMPORTS</c> names a request: for each asset the importer class
    /// named is made and its <c>OnImportAsset</c> called with an import context of the asset. The
    /// objects it adds are written as their serialized fields (<c>JsonUtility</c>), with their
    /// identifiers, classes and names; references between them by identifier. The Player then quits.
    /// </summary>
    public static class ScriptedImports
    {
        internal sealed class Context
        {
            internal string AssetPath;
            internal UnityEngine.Object Main;
            internal readonly List<KeyValuePair<string, UnityEngine.Object>> Objects = new List<KeyValuePair<string, UnityEngine.Object>>();
            internal void Add(string identifier, UnityEngine.Object obj) { if (obj != null) Objects.Add(new KeyValuePair<string, UnityEngine.Object>(identifier, obj)); }
        }

        static readonly System.Runtime.CompilerServices.ConditionalWeakTable<object, Context> contexts =
            new System.Runtime.CompilerServices.ConditionalWeakTable<object, Context>();

        internal static Context Of(object self) =>
            self != null && contexts.TryGetValue(self, out Context context) ? context : new Context();

        [RuntimeInitializeOnLoadMethod(RuntimeInitializeLoadType.SubsystemRegistration)]
        static void Run()
        {
            string request = Environment.GetEnvironmentVariable("UME_SCRIPTED_IMPORTS");
            if (string.IsNullOrEmpty(request)) return;
            var answers = new List<object>();
            string output = request + ".out.json";
            try
            {
                var parsed = UnityMicroEditor.Raw.RawJson.Parse(File.ReadAllText(request)) as Dictionary<string, object>;
                if (parsed != null && parsed.TryGetValue("output", out object named) && named is string path) output = path;
                var imports = parsed != null && parsed.TryGetValue("imports", out object list) ? list as List<object> : null;
                foreach (object item in imports ?? new List<object>())
                    if (item is Dictionary<string, object> entry) answers.Add(Import(entry));
            }
            catch (Exception error) { answers.Add(new Dictionary<string, object> { ["error"] = error.ToString() }); }
            File.WriteAllText(output, UnityMicroEditor.Raw.RawJson.Write(new Dictionary<string, object> { ["imports"] = answers }));
            Application.Quit();
        }

        static string Text(Dictionary<string, object> entry, string key) =>
            entry.TryGetValue(key, out object value) && value is string text ? text : string.Empty;

        static Type FindType(string fullName)
        {
            foreach (Assembly assembly in AppDomain.CurrentDomain.GetAssemblies())
            {
                Type type = assembly.GetType(fullName, false);
                if (type != null) return type;
            }
            return null;
        }

        static Dictionary<string, object> Import(Dictionary<string, object> entry)
        {
            var answer = new Dictionary<string, object> { ["guid"] = Text(entry, "guid") };
            try
            {
                Type importerType = FindType(Text(entry, "importer"));
                if (importerType == null) throw new InvalidOperationException("no class " + Text(entry, "importer"));
                object importer = Activator.CreateInstance(importerType, true);
                // The importer's own settings, as its .meta keeps them.
                if (entry.TryGetValue("settings", out object settings) && settings is Dictionary<string, object> values)
                    foreach (var value in values)
                    {
                        FieldInfo field = null;
                        for (Type type = importerType; type != null && field == null; type = type.BaseType)
                            field = type.GetField(value.Key, BindingFlags.Instance | BindingFlags.Public | BindingFlags.NonPublic | BindingFlags.DeclaredOnly);
                        if (field == null || value.Value == null) continue;
                        try
                        {
                            object converted = field.FieldType.IsEnum
                                ? Enum.ToObject(field.FieldType, Convert.ToInt64(value.Value, System.Globalization.CultureInfo.InvariantCulture))
                                : field.FieldType == typeof(bool)
                                    ? (object)(Convert.ToString(value.Value, System.Globalization.CultureInfo.InvariantCulture) == "1")
                                    : Convert.ChangeType(value.Value, field.FieldType, System.Globalization.CultureInfo.InvariantCulture);
                            field.SetValue(importer, converted);
                        }
                        catch (Exception) { }
                    }
                object context = Serialized.New("UnityEditor.AssetImporters.AssetImportContext");
                if (context == null) throw new InvalidOperationException("the Player holds no import context of the Editor");
                var state = new Context { AssetPath = Text(entry, "assetPath") };
                contexts.Add(context, state);
                MethodInfo import = importerType.GetMethod("OnImportAsset", BindingFlags.Instance | BindingFlags.Public | BindingFlags.NonPublic);
                if (import == null) throw new InvalidOperationException(importerType.FullName + " has no OnImportAsset");
                import.Invoke(importer, new[] { context });
                var identifiers = new Dictionary<int, string>();
                foreach (var added in state.Objects) identifiers[added.Value.GetInstanceID()] = added.Key;
                var objects = new List<object>();
                foreach (var added in state.Objects)
                {
                    UnityEngine.Object obj = added.Value;
                    Type type = obj.GetType();
                    if (!(obj is ScriptableObject) && !(obj is MonoBehaviour))
                        throw new NotSupportedException("an imported object of the engine class " + type.FullName);
                    objects.Add(new Dictionary<string, object>
                    {
                        ["identifier"] = added.Key,
                        ["class"] = type.FullName,
                        ["name"] = obj.name,
                        ["main"] = ReferenceEquals(obj, state.Main) || (state.Main == null && objects.Count == 0),
                        ["json"] = JsonUtility.ToJson(obj),
                    });
                }
                var references = new Dictionary<string, object>();
                foreach (var pair in identifiers) references[pair.Key.ToString(System.Globalization.CultureInfo.InvariantCulture)] = pair.Value;
                answer["objects"] = objects;
                answer["references"] = references;
            }
            catch (Exception error)
            {
                Exception cause = error is TargetInvocationException invoked && invoked.InnerException != null ? invoked.InnerException : error;
                answer["error"] = cause.GetType().Name + ": " + cause.Message;
            }
            return answer;
        }
    }
}
