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

        static ObjectIdentifier[] Identifiers(AssetImportContext ctx, string path, StringBuilder report, string role)
        {
            ctx.DependsOnArtifact(path);
            string guid = AssetDatabase.AssetPathToGUID(path);
            if (string.IsNullOrEmpty(guid))
                throw new InvalidOperationException("the worker does not know " + path);
            if (AssetDatabase.LoadMainAssetAtPath(path) == null)
                throw new InvalidOperationException("the worker cannot load " + path);
            var ids = ContentBuildInterface.GetPlayerObjectIdentifiersInAsset(new GUID(guid), Target);
            report.Append(role).Append(' ').Append(guid).Append(' ').Append(ids.Length).Append(' ').Append(path).Append('\n');
            return ids;
        }

        static void Write(AssetImportContext ctx, StringBuilder report)
        {
            var written = new List<ObjectIdentifier>();
            var writtenPaths = new List<string>();
            var embedded = new List<ObjectIdentifier>();
            string only = null;
            var users = new List<ObjectIdentifier>();
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
                    default: throw new InvalidOperationException("unknown request statement " + verb);
                }
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
