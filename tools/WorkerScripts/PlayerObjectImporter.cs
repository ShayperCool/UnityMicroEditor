// Runs inside a standalone AssetImportWorker, compiled by the tool and staged
// into the worker's import context. It asks the Editor's own build code for
// the Player form of assets that were imported earlier in the same context,
// so a build's rules (variant enumeration, the render pipeline's keyword
// filters and IPreprocessShaders strippers, compilation, compression) are
// the engine's and the packages', not a reimplementation.
//
// The request is a text file, one statement per line:
//   write <asset path>   an asset whose Player objects are written
//   only <type name>     of the assets to write, only the objects of this type are written (a shader graph also
//                        holds a material nothing here delivers)
//   embed <asset path>   an asset the written ones refer to, which has no file of its own to be found in: its
//                        objects are written into the same file, as a build does for an asset no bundle holds
//   use <asset path>     an asset that uses the written ones (a material); it adds usage, not objects
//   need <asset path>    an asset that only has to be importable (settings the pipeline reads)
//   save <asset path>    an asset whose objects are saved as the Editor writes them as text, after the engine
//                        loaded them and so upgraded an older serialization; reported as "saved <base64> <path>".
//                        A request that only saves writes no Player objects.
//   settings <asset path> a render pipeline's global settings, whose entries a Player build keeps or strips is
//                        reported as "settings kept|stripped <managed reference ID> <type> <path>". A request
//                        that only asks this writes no Player objects.
//   scene <asset path>   a scene, written as a build writes it: prefab instances unpacked, EditorOnly objects
//                        left out, with the scene's settings managers. A request that writes a scene writes
//                        nothing else.
// The artifact holds the serialized file under the extension "player" and a
// report (the main object, a TextAsset) that lists what the file refers to.
// Every object keeps the local ID it has in its asset, in the written file
// and in a reference to it; an embedded object gets one made from its
// identity.
//
// Two members of the Editor are reached that are not public, because no
// public entry runs a build's shader callbacks short of a whole build:
// BuildPipelineInterfaces.InitializeBuildCallbacks and CleanupBuildCallbacks,
// which the scriptable build pipeline package calls as a friend assembly, and,
// only when a callback's constructor is refused inside an import, the two
// lists those calls fill (see Callbacks below). Everything else is public.
using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Reflection;
using System.Text;
using UnityEditor;
using UnityEditor.AssetImporters;
using UnityEditor.Build;
using UnityEditor.Build.Content;
using UnityEngine;
using UnityEngine.Rendering;

namespace UnityMicroEditor.Worker
{
    [ScriptedImporter(1, "umeplayer")]
    public sealed class PlayerObjectImporter : ScriptedImporter
    {
        const BuildTarget Target = BuildTarget.StandaloneLinux64;
        const string InternalName = "ume-player-objects";

        public override void OnImportAsset(AssetImportContext ctx)
        {
            var report = new StringBuilder();
            report.Append("ume-player-objects 1\n");
            try
            {
                Write(ctx, report);
            }
            catch (Exception error)
            {
                // The coordinator reads the reason from the report; an import
                // error would only say that the importer failed.
                report.Append("error ").Append(OneLine(error.GetType().Name + ": " + error.Message)).Append('\n');
            }
            var text = new TextAsset(report.ToString());
            ctx.AddObjectToAsset("report", text);
            ctx.SetMainObject(text);
        }

        static string OneLine(string text)
        {
            return text.Replace('\r', ' ').Replace('\n', ' ');
        }

        // The render pipeline assets a build for the target is made with, as
        // the pipelines' own build code lists them: the public
        // BuildTargetExtensions of the render pipeline core package when
        // the project has it, else the engine's list for the platform.
        static List<RenderPipelineAsset> BuildPipelines()
        {
            var list = new List<RenderPipelineAsset>();
            foreach (var assembly in AppDomain.CurrentDomain.GetAssemblies())
            {
                var extensions = assembly.GetType("UnityEditor.Rendering.BuildTargetExtensions", false);
                if (extensions == null || !extensions.IsPublic) continue;
                var method = extensions.GetMethods(BindingFlags.Public | BindingFlags.Static)
                    .FirstOrDefault(m => m.Name == "TryGetRenderPipelineAssets" && m.IsGenericMethodDefinition && m.GetParameters().Length == 2);
                if (method == null) continue;
                method.MakeGenericMethod(typeof(RenderPipelineAsset)).Invoke(null, new object[] { Target, list });
                return list;
            }
            QualitySettings.GetAllRenderPipelineAssetsForPlatform("Standalone", ref list);
            return list;
        }

        // Starts the callbacks a build runs on shaders. The Editor collects
        // them by constructing every class. A constructor that does what an
        // import may not stops that collection half way, and a build without
        // its strippers would write other variants, so that is refused, with
        // one exception that is checked rather than assumed: the Universal
        // pipeline's UpdateShaderPrefilteringDataBeforeBuild. Its constructor
        // prepares each pipeline asset of the build in memory and then saves
        // it; the save is what an import refuses, after the first asset is
        // prepared. The class has no state and its callback does nothing, so
        // with one pipeline asset everything a build takes from it is in
        // place when the save is refused. Then, and only then, the lists are
        // completed here, that class without its constructor.
        static void Callbacks(Type interfaces, MethodInfo initialize, object wanted, List<RenderPipelineAsset> pipelines, StringBuilder report)
        {
            var flags = BindingFlags.Static | BindingFlags.NonPublic | BindingFlags.Public;
            try
            {
                initialize.Invoke(null, new[] { wanted });
                return;
            }
            catch (TargetInvocationException refused)
            {
                var cause = refused.InnerException ?? refused;
                report.Append("callbacks-refused ").Append(OneLine(cause.GetType().Name + ": " + cause.Message)).Append('\n');
            }
            var processors = interfaces.GetProperty("processors", flags)?.GetValue(null);
            var shaderList = processors?.GetType().GetField("shaderProcessors");
            var computeList = processors?.GetType().GetField("computeShaderProcessors");
            if (shaderList == null || computeList == null)
                throw new InvalidOperationException("this Editor keeps its build callbacks otherwise");
            var shaders = new List<IPreprocessShaders>();
            var computes = new List<IPreprocessComputeShaders>();
            foreach (var type in TypeCache.GetTypesDerivedFrom<IOrderedCallback>())
            {
                if (type.IsAbstract || type.IsInterface) continue;
                bool forShaders = typeof(IPreprocessShaders).IsAssignableFrom(type);
                bool forComputes = typeof(IPreprocessComputeShaders).IsAssignableFrom(type);
                if (!forShaders && !forComputes) continue;
                object instance;
                try
                {
                    instance = Activator.CreateInstance(type, true);
                }
                catch (Exception failed)
                {
                    var why = failed.InnerException ?? failed;
                    string said = OneLine(why.GetType().Name + ": " + why.Message);
                    if (type.FullName != "UnityEditor.Rendering.Universal.UpdateShaderPrefilteringDataBeforeBuild"
                        || !why.Message.Contains("AssetDatabase.SaveAssetIfDirty")
                        || type.GetFields(BindingFlags.Instance | BindingFlags.Public | BindingFlags.NonPublic).Length != 0)
                        throw new InvalidOperationException("the build callback " + type.FullName + " cannot be made inside an import (" + said + ")");
                    if (pipelines.Count != 1)
                        throw new InvalidOperationException("the build is made with " + pipelines.Count + " render pipeline assets ("
                            + string.Join(", ", pipelines.Select(p => p == null ? "none" : AssetDatabase.GetAssetPath(p)))
                            + "); the Universal pipeline prepares only the first before an import stops it");
                    report.Append("callback-unconstructed ").Append(type.FullName).Append(' ').Append(said).Append('\n');
                    instance = System.Runtime.Serialization.FormatterServices.GetUninitializedObject(type);
                }
                if (forShaders) shaders.Add((IPreprocessShaders)instance);
                if (forComputes) computes.Add((IPreprocessComputeShaders)instance);
            }
            // The Editor's own order: as the type cache lists them, then by callback order.
            shaders.Sort((a, b) => a.callbackOrder.CompareTo(b.callbackOrder));
            computes.Sort((a, b) => a.callbackOrder.CompareTo(b.callbackOrder));
            shaderList.SetValue(processors, shaders);
            computeList.SetValue(processors, computes);
        }

        // What the compiler said about the written shaders. A program that
        // failed is left out of a build, which goes on; here it is refused.
        static void Messages(List<string> paths, StringBuilder report)
        {
            string first = null;
            foreach (var path in paths)
            {
                var main = AssetDatabase.LoadMainAssetAtPath(path);
                ShaderMessage[] messages = main is Shader shader ? ShaderUtil.GetShaderMessages(shader)
                    : main is ComputeShader compute ? ShaderUtil.GetComputeShaderMessages(compute)
                    : new ShaderMessage[0];
                foreach (var message in messages)
                {
                    string text = OneLine(message.platform + " " + message.file + ":" + message.line + " " + message.message);
                    report.Append("shader-message ").Append(message.severity).Append(' ').Append(text).Append('\n');
                    if (message.severity == UnityEditor.Rendering.ShaderCompilerMessageSeverity.Error && first == null)
                        first = path + ": " + text;
                }
            }
            if (first != null)
                throw new InvalidOperationException("a shader program does not compile: " + first);
        }

        // A local ID for an object that is written into a file of another
        // asset: positive, and the same for the same object every time.
        static long Stable(ObjectIdentifier id)
        {
            using (var md5 = System.Security.Cryptography.MD5.Create())
            {
                var hash = md5.ComputeHash(Encoding.ASCII.GetBytes(id.guid + "/" + id.localIdentifierInFile));
                long value = BitConverter.ToInt64(hash, 0) & long.MaxValue;
                return value == 0 ? 1 : value;
            }
        }

        // What the worker knows of an asset it cannot load: the type it
        // expects and the script that would be its class.
        static string Diagnose(string path)
        {
            var text = new StringBuilder();
            text.Append(" (main type ").Append(AssetDatabase.GetMainAssetTypeAtPath(path)?.FullName ?? "none");
            text.Append(", ").Append(AssetDatabase.LoadAllAssetsAtPath(path).Length).Append(" objects");
            foreach (string line in System.IO.File.ReadAllLines(path))
            {
                string trimmed = line.Trim();
                int start = trimmed.IndexOf("guid: ");
                if (!trimmed.StartsWith("m_Script:") || start < 0 || trimmed.Length < start + 38) continue;
                string guid = trimmed.Substring(start + 6, 32);
                string scriptPath = AssetDatabase.GUIDToAssetPath(guid);
                var script = string.IsNullOrEmpty(scriptPath) ? null : AssetDatabase.LoadAssetAtPath<MonoScript>(scriptPath);
                text.Append(", script ").Append(guid).Append(" at '").Append(scriptPath).Append("' ")
                    .Append(script == null ? "not loaded" : script.GetClass() == null ? "without a class" : script.GetClass().FullName);
                break;
            }
            return text.Append(')').ToString();
        }

        // The objects of each asset as the Editor writes them as text. They
        // are read from the source again, as MaterialInspector reads them,
        // which upgrades an older serialization; the references they hold
        // resolve to the assets imported in this context. The Editor maps a
        // path under Packages to the package's directory in GetFullPath.
        static void Save(List<string> paths, StringBuilder report)
        {
            if (paths.Count == 0) return;
            string directory = Path.Combine("Temp", "ume-saved");
            Directory.CreateDirectory(directory);
            for (int index = 0; index < paths.Count; index++)
            {
                var objects = UnityEditorInternal.InternalEditorUtility.LoadSerializedFileAndForget(Path.GetFullPath(paths[index]));
                if (objects == null || objects.Length == 0)
                    throw new InvalidOperationException("the worker cannot read " + paths[index]);
                string file = Path.Combine(directory, index + Path.GetExtension(paths[index]));
                UnityEditorInternal.InternalEditorUtility.SaveToSerializedFileAndForget(objects, file, true);
                if (!File.Exists(file))
                    throw new InvalidOperationException("the Editor saved no text of " + paths[index]);
                string text = Convert.ToBase64String(File.ReadAllBytes(file));
                report.Append("saved ").Append(text).Append(' ').Append(paths[index]).Append('\n');
            }
        }

        // The entries of a pipeline's global settings that a Player build keeps.
        // A build fills the runtime list of the settings container from the
        // Editor's list as SRP Core's RenderPipelineGraphicsSettingsStripper
        // decides: an entry whose type has active strippers is kept unless
        // every one of them can remove it, any other entry when it says it is
        // available in a Player build. The strippers are found and made as
        // SRP Core finds them (public types implementing
        // IRenderPipelineGraphicsSettingsStripper<T> with a public constructor),
        // and asked through that public interface. The entries are read with
        // the public serialization API.
        static void Settings(List<string> paths, StringBuilder report)
        {
            if (paths.Count == 0) return;
            Type generic = null, stripper = null;
            foreach (var assembly in AppDomain.CurrentDomain.GetAssemblies())
            {
                generic = generic ?? assembly.GetType("UnityEngine.Rendering.IRenderPipelineGraphicsSettingsStripper`1", false);
                stripper = stripper ?? assembly.GetType("UnityEngine.Rendering.IStripper", false);
            }
            var active = stripper?.GetProperty("active");
            var strippers = new Dictionary<Type, List<object>>();
            if (generic != null && active != null)
            {
                foreach (var type in TypeCache.GetTypesDerivedFrom(generic))
                {
                    if (type.IsAbstract || type.GetConstructor(BindingFlags.Public | BindingFlags.Instance, null, Type.EmptyTypes, null) == null)
                        continue;
                    foreach (var face in type.GetInterfaces())
                    {
                        if (!face.IsGenericType || face.GetGenericTypeDefinition() != generic) continue;
                        var instance = Activator.CreateInstance(type);
                        if (!(bool)active.GetValue(instance)) continue;
                        var settingsType = face.GetGenericArguments()[0];
                        if (!strippers.TryGetValue(settingsType, out var list)) strippers[settingsType] = list = new List<object>();
                        list.Add(instance);
                    }
                }
            }
            foreach (var path in paths)
            {
                var asset = AssetDatabase.LoadAssetAtPath<RenderPipelineGlobalSettings>(path);
                if (asset == null) throw new InvalidOperationException("no render pipeline global settings at " + path);
                var list = new SerializedObject(asset).FindProperty("m_Settings.m_SettingsList.m_List");
                if (list == null || !list.isArray)
                    throw new InvalidOperationException("the global settings at " + path + " keep their entries otherwise");
                for (int index = 0; index < list.arraySize; index++)
                {
                    var element = list.GetArrayElementAtIndex(index);
                    var settings = element.managedReferenceValue as IRenderPipelineGraphicsSettings;
                    if (settings == null) continue;
                    var type = settings.GetType();
                    bool kept;
                    if (strippers.TryGetValue(type, out var found))
                    {
                        var method = generic.MakeGenericType(type).GetMethod("CanRemoveSettings");
                        bool remove = true;
                        foreach (var instance in found) remove &= (bool)method.Invoke(instance, new object[] { settings });
                        kept = !remove;
                    }
                    else
                        kept = settings.isAvailableInPlayerBuild;
                    report.Append("settings ").Append(kept ? "kept " : "stripped ").Append(element.managedReferenceId)
                        .Append(' ').Append(type.FullName).Append(' ').Append(path).Append('\n');
                }
            }
        }

        static ObjectIdentifier[] Identifiers(AssetImportContext ctx, string path, StringBuilder report, string role)
        {
            ctx.DependsOnArtifact(path);
            string guid = AssetDatabase.AssetPathToGUID(path);
            if (string.IsNullOrEmpty(guid))
                throw new InvalidOperationException("the worker does not know " + path);
            if (AssetDatabase.LoadMainAssetAtPath(path) == null)
                throw new InvalidOperationException("the worker cannot load " + path + Diagnose(path));
            var ids = ContentBuildInterface.GetPlayerObjectIdentifiersInAsset(new GUID(guid), Target);
            report.Append(role).Append(' ').Append(guid).Append(' ').Append(ids.Length).Append(' ').Append(path).Append('\n');
            return ids;
        }

        // A scene as a build writes it. The Editor opens the scene from its
        // source for the build, unpacks its prefab instances and leaves out
        // its EditorOnly objects; every object of another file it refers to
        // is named, like a written asset's, by an external file of its own.
        static void WriteScene(AssetImportContext ctx, string scene, bool withDependencies, StringBuilder report)
        {
            var timer = System.Diagnostics.Stopwatch.StartNew();
            var settings = new BuildSettings { target = Target, group = BuildTargetGroup.Standalone, buildFlags = ContentBuildFlags.None };
            var usage = new BuildUsageTagSet();
            var info = ContentBuildInterface.CalculatePlayerDependenciesForScene(scene, settings, usage);
            report.Append("scene-dependencies ").Append(info.referencedObjects.Count).Append(' ').Append(timer.ElapsedMilliseconds).Append('\n');
            var global = info.globalUsage | ContentBuildInterface.GetGlobalUsageFromGraphicsSettings();
            var map = new BuildReferenceMap();
            string directory = Path.Combine("Temp", "ume-player-" + AssetDatabase.AssetPathToGUID(ctx.assetPath));
            Directory.CreateDirectory(directory);
            if (withDependencies)
            {
                // What the scene refers to, but the engine's own resources,
                // written into one file beside it, scripts first.
                const string Builtin = "0000000000000000e000000000000000";
                var dependencies = info.referencedObjects.Where(id => id.guid.ToString() != Builtin)
                    .OrderBy(id => ContentBuildInterface.GetTypeForObject(id) == typeof(MonoScript) ? 0 : 1).ToList();
                var depsCommand = new WriteCommand { fileName = "deps", internalName = "ume-deps", serializeObjects = new List<SerializationInfo>() };
                for (int index = 0; index < dependencies.Count; index++)
                {
                    var id = dependencies[index];
                    depsCommand.serializeObjects.Add(new SerializationInfo { serializationObject = id, serializationIndex = index + 1 });
                    map.AddMapping("ume-deps", index + 1, id);
                    var type = ContentBuildInterface.GetTypeForObject(id);
                    report.Append("dependency ").Append(index + 1).Append(' ').Append(type == null ? "?" : type.FullName).Append(' ').Append(id.guid).Append(' ').Append(id.localIdentifierInFile).Append('\n');
                }
                var depsResult = ContentBuildInterface.WriteSerializedFile(directory, new WriteParameters
                {
                    writeCommand = depsCommand,
                    settings = settings,
                    globalUsage = global,
                    usageSet = usage,
                    referenceMap = map,
                });
                foreach (var file in depsResult.resourceFiles)
                    report.Append("deps-file ").Append(file.fileAlias).Append(' ').Append(file.serializedFile ? 1 : 0).Append(' ').Append(Path.GetFileName(file.fileName)).Append('\n');
                foreach (var name in depsResult.externalFileReferences)
                    report.Append("deps-external ").Append(name.filePath).Append('\n');
            }
            foreach (var id in withDependencies ? new List<ObjectIdentifier>() : info.referencedObjects.ToList())
            {
                var type = ContentBuildInterface.GetTypeForObject(id);
                string file = "ume-external/" + id.guid + "/" + (int)id.fileType + "/" + id.localIdentifierInFile;
                map.AddMapping(file, id.localIdentifierInFile, id);
                report.Append("external ").Append(file).Append(' ').Append(id.localIdentifierInFile).Append(' ').Append(type == null ? "?" : type.FullName).Append(' ').Append(OneLine(id.filePath ?? "")).Append('\n');
            }
            // A scene file is named without an extension: the Editor drops it.
            var command = new WriteCommand { fileName = "scene", internalName = InternalName, serializeObjects = new List<SerializationInfo>() };
            timer.Restart();
            var result = ContentBuildInterface.WriteSceneSerializedFile(directory, new WriteSceneParameters
            {
                scenePath = scene,
                writeCommand = command,
                settings = settings,
                globalUsage = global,
                usageSet = usage,
                referenceMap = map,
            });
            report.Append("scene-written ").Append(result.serializedObjects.Count).Append(' ').Append(timer.ElapsedMilliseconds).Append('\n');
            foreach (var written in result.serializedObjects)
            {
                var id = written.serializedObject;
                var type = ContentBuildInterface.GetTypeForObject(id);
                report.Append("object ").Append(id.localIdentifierInFile).Append(' ').Append(type == null ? "?" : type.FullName).Append(' ').Append(id.guid).Append(' ').Append(id.localIdentifierInFile).Append('\n');
            }
            foreach (var file in result.resourceFiles)
                report.Append("file ").Append(file.fileAlias).Append(' ').Append(file.serializedFile ? 1 : 0).Append(' ').Append(Path.GetFileName(file.fileName)).Append('\n');
            foreach (var name in result.externalFileReferences)
                report.Append("scene-external ").Append(name.filePath).Append('\n');
            string output = Path.Combine(directory, command.fileName);
            if (!File.Exists(output))
                throw new InvalidOperationException("the Editor wrote no scene file");
            ctx.SetOutputArtifactFile("player", output);
            report.Append("done\n");
        }

        static void Write(AssetImportContext ctx, StringBuilder report)
        {
            var written = new List<ObjectIdentifier>();
            var writtenPaths = new List<string>();
            var embedded = new List<ObjectIdentifier>();
            string only = null;
            var users = new List<ObjectIdentifier>();
            var saved = new List<string>();
            var settings = new List<string>();
            var scenes = new List<string>();
            bool withDependencies = false;
            foreach (var raw in File.ReadAllLines(ctx.assetPath))
            {
                string line = raw.TrimEnd('\r');
                int space = line.IndexOf(' ');
                if (space <= 0) continue;
                string verb = line.Substring(0, space), value = line.Substring(space + 1);
                switch (verb)
                {
                    case "write":
                        written.AddRange(Identifiers(ctx, value, report, "written"));
                        writtenPaths.Add(value);
                        break;
                    case "only": only = value; break;
                    case "embed":
                        embedded.AddRange(Identifiers(ctx, value, report, "embedded"));
                        writtenPaths.Add(value);
                        break;
                    case "use": users.AddRange(Identifiers(ctx, value, report, "user")); break;
                    case "need": Identifiers(ctx, value, report, "needed"); break;
                    case "save":
                        Identifiers(ctx, value, report, "saved-source");
                        saved.Add(value);
                        break;
                    case "settings":
                        Identifiers(ctx, value, report, "settings-source");
                        settings.Add(value);
                        break;
                    case "scene":
                    case "scene-with-dependencies":
                        ctx.DependsOnSourceAsset(value);
                        scenes.Add(value);
                        withDependencies = verb == "scene-with-dependencies";
                        break;
                    default: throw new InvalidOperationException("unknown request statement " + verb);
                }
            }
            if (scenes.Count != 0)
            {
                if (scenes.Count != 1 || written.Count != 0 || embedded.Count != 0 || saved.Count != 0 || settings.Count != 0)
                    throw new InvalidOperationException("a request that writes a scene writes nothing else");
                WriteScene(ctx, scenes[0], withDependencies, report);
                return;
            }
            Save(saved, report);
            Settings(settings, report);
            if ((saved.Count != 0 || settings.Count != 0) && written.Count == 0)
            {
                report.Append("done\n");
                return;
            }
            if (only != null)
            {
                foreach (var id in written)
                {
                    var type = ContentBuildInterface.GetTypeForObject(id);
                    if (type == null || type.FullName != only)
                        report.Append("left ").Append(id.localIdentifierInFile).Append(' ').Append(type == null ? "?" : type.FullName).Append('\n');
                }
                written = written.Where(id => ContentBuildInterface.GetTypeForObject(id)?.FullName == only).ToList();
            }
            if (written.Count == 0)
                throw new InvalidOperationException("nothing to write");

            // What a build of this project is made with, for the coordinator
            // to compare with what it staged.
            report.Append("default-pipeline ").Append(GraphicsSettings.defaultRenderPipeline == null ? "none" : AssetDatabase.GetAssetPath(GraphicsSettings.defaultRenderPipeline)).Append('\n');
            for (int level = 0; level < QualitySettings.count; level++)
            {
                var asset = QualitySettings.GetRenderPipelineAssetAt(level);
                report.Append("quality ").Append(level).Append(' ').Append(asset == null ? "none" : AssetDatabase.GetAssetPath(asset)).Append('\n');
            }
            var pipelines = BuildPipelines();
            foreach (var asset in pipelines)
                report.Append("build-pipeline ").Append(asset == null ? "none" : AssetDatabase.GetAssetPath(asset)).Append('\n');
            report.Append("pipeline ").Append(GraphicsSettings.currentRenderPipeline == null ? "none" : AssetDatabase.GetAssetPath(GraphicsSettings.currentRenderPipeline)).Append('\n');
            foreach (var type in TypeCache.GetTypesDerivedFrom<IPreprocessShaders>().Select(t => t.FullName + ", " + t.Assembly.GetName().Name).OrderBy(n => n, StringComparer.Ordinal))
                report.Append("shader-preprocessor ").Append(type).Append('\n');
            foreach (var type in TypeCache.GetTypesDerivedFrom<IPreprocessComputeShaders>().Select(t => t.FullName + ", " + t.Assembly.GetName().Name).OrderBy(n => n, StringComparer.Ordinal))
                report.Append("compute-preprocessor ").Append(type).Append('\n');
            foreach (var guid in AssetDatabase.FindAssets("t:VolumeProfile").OrderBy(g => g, StringComparer.Ordinal))
                report.Append("volume-profile ").Append(guid).Append(' ').Append(AssetDatabase.GUIDToAssetPath(guid)).Append('\n');

            var own = written.ToArray();
            var global = ContentBuildInterface.GetGlobalUsageFromGraphicsSettings();
            var usage = new BuildUsageTagSet();
            var roots = users.Concat(own).ToArray();
            var allIds = roots.Concat(ContentBuildInterface.GetPlayerDependenciesForObjects(roots, Target, null)).Distinct().ToArray();
            ContentBuildInterface.CalculateBuildUsageTags(allIds, allIds, global, usage);

            var command = new WriteCommand { fileName = "objects.player", internalName = InternalName, serializeObjects = new List<SerializationInfo>() };
            var map = new BuildReferenceMap();
            var ownSet = new HashSet<ObjectIdentifier>(own);
            var embeddedSet = new HashSet<ObjectIdentifier>(embedded);
            var taken = new HashSet<long>(own.Select(id => id.localIdentifierInFile));
            foreach (var id in own)
            {
                var type = ContentBuildInterface.GetTypeForObject(id);
                long index = id.localIdentifierInFile;
                command.serializeObjects.Add(new SerializationInfo { serializationObject = id, serializationIndex = index });
                map.AddMapping(InternalName, index, id);
                report.Append("object ").Append(index).Append(' ').Append(type == null ? "?" : type.FullName).Append(' ').Append(id.guid).Append(' ').Append(id.localIdentifierInFile).Append('\n');
            }
            foreach (var id in ContentBuildInterface.GetPlayerDependenciesForObjects(own, Target, null))
            {
                if (ownSet.Contains(id)) continue;
                var type = ContentBuildInterface.GetTypeForObject(id);
                if (embeddedSet.Contains(id))
                {
                    long own64 = Stable(id);
                    if (!taken.Add(own64))
                        throw new InvalidOperationException("two written objects share the local ID " + own64);
                    command.serializeObjects.Add(new SerializationInfo { serializationObject = id, serializationIndex = own64 });
                    map.AddMapping(InternalName, own64, id);
                    report.Append("object ").Append(own64).Append(' ').Append(type == null ? "?" : type.FullName).Append(' ').Append(id.guid).Append(' ').Append(id.localIdentifierInFile).Append('\n');
                    continue;
                }
                long index = id.localIdentifierInFile;
                string file = "ume-external/" + id.guid + "/" + (int)id.fileType + "/" + id.localIdentifierInFile;
                map.AddMapping(file, index, id);
                report.Append("external ").Append(file).Append(' ').Append(index).Append(' ').Append(type == null ? "?" : type.FullName).Append(' ').Append(OneLine(id.filePath ?? "")).Append('\n');
            }

            // The scriptable build pipeline package starts the build callbacks
            // the same way: the entry is not public, and without it a build
            // through ContentBuildInterface runs no IPreprocessShaders.
            var interfaces = typeof(Editor).Assembly.GetType("UnityEditor.Build.BuildPipelineInterfaces", false);
            if (interfaces == null)
                throw new InvalidOperationException("this Editor starts its build callbacks otherwise");
            var flags = BindingFlags.Static | BindingFlags.NonPublic | BindingFlags.Public;
            var callbacks = interfaces.GetNestedType("BuildCallbacks", BindingFlags.NonPublic | BindingFlags.Public);
            var initialize = interfaces.GetMethod("InitializeBuildCallbacks", flags);
            var cleanup = interfaces.GetMethod("CleanupBuildCallbacks", flags);
            if (callbacks == null || initialize == null || cleanup == null)
                throw new InvalidOperationException("this Editor starts its build callbacks otherwise");
            object wanted = Enum.ToObject(callbacks, Convert.ToInt32(Enum.Parse(callbacks, "ShaderProcessors")) | Convert.ToInt32(Enum.Parse(callbacks, "ComputeShaderProcessors")));
            string directory = Path.Combine("Temp", "ume-player-" + AssetDatabase.AssetPathToGUID(ctx.assetPath));
            Directory.CreateDirectory(directory);
            WriteResult result;
            try
            {
                Callbacks(interfaces, initialize, wanted, pipelines, report);
                result = ContentBuildInterface.WriteSerializedFile(directory, new WriteParameters
                {
                    writeCommand = command,
                    settings = new BuildSettings { target = Target, group = BuildTargetGroup.Standalone, buildFlags = ContentBuildFlags.None },
                    globalUsage = global,
                    usageSet = usage,
                    referenceMap = map,
                });
            }
            finally
            {
                cleanup.Invoke(null, null);
            }
            foreach (var file in result.resourceFiles)
                report.Append("file ").Append(file.fileAlias).Append(' ').Append(file.serializedFile ? 1 : 0).Append(' ').Append(Path.GetFileName(file.fileName)).Append('\n');
            string output = Path.Combine(directory, command.fileName);
            if (!File.Exists(output))
                throw new InvalidOperationException("the Editor wrote no file");
            if (result.resourceFiles.Count(f => !f.serializedFile) != 0)
                throw new InvalidOperationException("the written objects keep data outside their file");
            Messages(writtenPaths, report);
            ctx.SetOutputArtifactFile("player", output);
            report.Append("done\n");
        }
    }
}
