using System;
using System.Collections;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Linq;
using System.Security.Cryptography;
using System.Text;
using UnityEditor;
using UnityEditor.Build;
using UnityEditor.Build.Reporting;
using UnityEditor.Compilation;
using UnityEditor.SceneManagement;
using UnityEngine;
using UnityEngine.SceneManagement;
using UnityEngine.Rendering;
using PackageInfo = UnityEditor.PackageManager.PackageInfo;

namespace UnityMicroEditor.Editor
{
    /// <summary>Owned file protocol for public Editor preparation and build APIs.</summary>
    [InitializeOnLoad]
    public static class AgentEditorBridge
    {
        // The releases the tool supports; the bridge serves the one it runs in.
        private static readonly string[] Releases = { "6000.6.3f1", "6000.0.71f1" };
        private static readonly string Profile = Array.IndexOf(Releases, Application.unityVersion) >= 0 ? Application.unityVersion : Releases[0];
        private const int RequestLimit = 256 * 1024;
        private const int ResponseLimit = 32 * 1024 * 1024;
        private static readonly string Project = Path.GetFullPath(Path.Combine(Application.dataPath, ".."));
        private static readonly string Mailbox = Path.Combine(Project, "Library/UnityMicroEditor/editor-bridge");
        private static readonly int Pid = System.Diagnostics.Process.GetCurrentProcess().Id;
        private static double nextPoll, nextHeartbeat;
        private static bool polling;

        static AgentEditorBridge() { if (!AssetDatabase.IsAssetImportWorkerProcess()) EditorApplication.update += Poll; }

        /// <summary>Starts a batch Editor owned by the bridge client; never exits another Editor.</summary>
        public static void RunBatch()
        {
            if (!Application.isBatchMode || Application.unityVersion != Profile)
                throw new InvalidOperationException("Editor bridge batch mode requires Unity " + string.Join(" or ", Releases) + ".");
            // A fresh headless Editor can start with no loaded scene. Its
            // public RestoreSceneManagerSetup API requires one active scene.
            if (!SceneManager.GetActiveScene().IsValid() || !SceneManager.GetActiveScene().isLoaded)
                EditorSceneManager.NewScene(NewSceneSetup.EmptyScene, NewSceneMode.Single);
            Poll();
        }

        private static void Poll()
        {
            if (polling || Application.unityVersion != Profile || EditorApplication.timeSinceStartup < nextPoll) return;
            nextPoll = EditorApplication.timeSinceStartup + .05;
            polling = true;
            try
            {
                Canonical(Mailbox); Directory.CreateDirectory(Mailbox);
                if (EditorApplication.timeSinceStartup >= nextHeartbeat)
                {
                    ReplaceOwned(Path.Combine(Mailbox, "ready.json"), AgentEditorJson.Write(Status()));
                    nextHeartbeat = EditorApplication.timeSinceStartup + .5;
                }
                if (EditorApplication.isCompiling || EditorApplication.isUpdating || BuildPipeline.isBuildingPlayer) return;
                if (StopOwnedBatch()) return;
                string[] requests = Directory.EnumerateFiles(Mailbox, "*.request.json").Take(4097).OrderBy(x => x, StringComparer.Ordinal).ToArray();
                if (requests.Length > 4096) throw new IOException("Editor bridge inbox exceeds its request bound.");
                foreach (string path in requests.Take(16)) Process(path);
            }
            catch (Exception error) { Debug.LogWarning("[editor-bridge] " + Bounded(error.Message, 1024)); }
            finally { polling = false; }
        }

        private static bool StopOwnedBatch()
        {
            string owner = Environment.GetEnvironmentVariable("UNITY_MICRO_EDITOR_BRIDGE_OWNER") ?? string.Empty;
            if (!Application.isBatchMode || owner.Length != 32 || !ValidId(owner)) return false;
            string stop = Path.Combine(Mailbox, "owner-" + owner + ".stop.json");
            if (!File.Exists(stop)) return false;
            Canonical(stop); if (new FileInfo(stop).Length > 4096) throw new InvalidDataException("Owned stop request exceeds its bound.");
            var request = AgentEditorJson.Parse(File.ReadAllText(stop)) as Dictionary<string, object>;
            if (request == null || Text(request, "ownerToken") != owner || Text(request, "project") != Project || Integer(request, "pid") != Pid)
                throw new InvalidDataException("Owned stop request does not identify this batch Editor.");
            File.Delete(stop); EditorApplication.Exit(0); return true;
        }

        private static void Process(string path)
        {
            string id = Path.GetFileName(path).Substring(0, Path.GetFileName(path).Length - ".request.json".Length);
            if (!ValidId(id)) return;
            string hash = string.Empty;
            bool accepted = false;
            try
            {
                string reply = Path.Combine(Mailbox, id + ".response.json");
                if (File.Exists(reply)) { Canonical(reply); File.Delete(path); return; }
                Canonical(path);
                var info = new FileInfo(path);
                if (!info.Exists || info.Length > RequestLimit) throw new InvalidDataException("Editor request exceeds its byte bound.");
                byte[] bytes = File.ReadAllBytes(path);
                if (bytes.Length > RequestLimit) throw new InvalidDataException("Editor request changed beyond its byte bound.");
                hash = Hash(bytes);
                var request = AgentEditorJson.Parse(new UTF8Encoding(false, true).GetString(bytes)) as Dictionary<string, object>;
                if (request == null || Text(request, "requestId") != id || Integer(request, "schemaVersion") != 1 || Text(request, "project") != Project
                    || Text(request, "unityVersion") != Profile || Path.GetFullPath(Text(request, "editor")) != Path.GetFullPath(EditorApplication.applicationPath))
                    throw new InvalidDataException("Editor request project, version, executable or identity is incompatible.");
                long expectedPid = Integer(request, "expectedPid");
                if (expectedPid != 0 && expectedPid != Pid) throw new InvalidDataException("Editor request targets another process.");
                string action = Text(request, "action");
                if (!Actions.Contains(action)) throw new InvalidDataException("Unsupported Editor bridge action.");
                if (!request.TryGetValue("arguments", out object value) || !(value is Dictionary<string, object> arguments))
                    throw new InvalidDataException("Editor arguments must be a JSON object.");
                string claim = Path.Combine(Mailbox, id + ".accepted.json");
                if (File.Exists(claim))
                {
                    Canonical(claim);
                    if (HashFile(claim) != hash) throw new InvalidDataException("Editor request ID already belongs to different immutable bytes.");
                    Reply(id, hash, null, "outcome_unknown", "This request was already accepted and its result is unavailable; inspect state before issuing another ID.");
                    File.Delete(path); return;
                }
                // The claim survives domain reload, process exit and response loss.
                File.Move(path, claim); accepted = true;
                Dictionary<string, object> result;
                switch (action)
                {
                    case "status": result = Status(); break;
                    case "export_graph": result = ExportGraph(); break;
                    case "scene_proof": result = SceneProof(Text(arguments, "scene")); break;
                    case "refresh":
                        RequireEditMode();
                        AssetDatabase.ImportAsset("Packages/com.unitymicroeditor.runtime", ImportAssetOptions.ImportRecursive | ImportAssetOptions.ForceUpdate | ImportAssetOptions.ForceSynchronousImport);
                        AssetDatabase.Refresh(ImportAssetOptions.ForceSynchronousImport);
                        CompilationPipeline.RequestScriptCompilation();
                        result = Status();
                        break;
                    case "build_project": result = BuildProject(arguments, id); break;
                    case "build_host": result = BuildHost(arguments, id); break;
                    case "prefab_prepare": RequireStaging(); result = PreparationAdapter("AgentPrefabAuthoring", arguments, id); break;
                    case "component_schema": result = PreparationAdapter("AgentEditorAuthoring", arguments, id); break;
                    case "native_prepare": RequireStaging(); RequireEditMode(); RequireCleanScenes(); result = AgentNativeAssetExporter.Execute(arguments, Mailbox, id); break;
                    default: throw new InvalidOperationException("Unsupported Editor action.");
                }
                bool failedBuild = (action == "build_project" || action == "build_host") && Text(result, "buildResult") != "Succeeded";
                Reply(id, hash, result, failedBuild ? "build_failed" : null, failedBuild ? "Unity project build did not succeed; inspect the returned build summary and Editor log." : null);
            }
            catch (Exception error)
            {
                Reply(id, hash, null, accepted ? "editor_action_failed" : "request_rejected", error.Message);
            }
        }

        private static readonly string[] Actions = { "status", "export_graph", "scene_proof", "refresh", "build_project", "build_host", "prefab_prepare", "component_schema", "native_prepare" };

        private static Dictionary<string, object> PreparationAdapter(string name, Dictionary<string, object> arguments, string id)
        {
            // Only the two named public adapters in this package's own Editor
            // assembly are callable. No request supplies a type or member name.
            Type type = typeof(AgentEditorBridge).Assembly.GetType("UnityMicroEditor.Editor." + name, false);
            var method = type == null ? null : type.GetMethod("Execute", System.Reflection.BindingFlags.Public | System.Reflection.BindingFlags.Static,
                null, new[] { typeof(Dictionary<string, object>), typeof(string), typeof(string) }, null);
            if (method == null) throw new InvalidOperationException("This installed package has no " + name + " preparation adapter.");
            try { return (Dictionary<string, object>)method.Invoke(null, new object[] { arguments, Mailbox, id }); }
            catch (System.Reflection.TargetInvocationException error) { throw error.InnerException ?? error; }
        }

        public static Dictionary<string, object> Status()
        {
            var dirty = new List<object>();
            for (int index = 0; index < SceneManager.sceneCount; index++)
            { Scene scene = SceneManager.GetSceneAt(index); if (scene.isDirty) dirty.Add(scene.path); }
            return new Dictionary<string, object> { ["schemaVersion"] = 1, ["backend"] = "unity_editor", ["project"] = Project,
                ["unityVersion"] = Application.unityVersion, ["editor"] = Path.GetFullPath(EditorApplication.applicationPath), ["pid"] = Pid,
                ["ownerToken"] = Environment.GetEnvironmentVariable("UNITY_MICRO_EDITOR_BRIDGE_OWNER") ?? string.Empty,
                ["batchMode"] = Application.isBatchMode, ["isPlaying"] = EditorApplication.isPlayingOrWillChangePlaymode,
                ["isCompiling"] = EditorApplication.isCompiling, ["isUpdating"] = EditorApplication.isUpdating,
                ["isBuilding"] = BuildPipeline.isBuildingPlayer, ["activeBuildTarget"] = EditorUserBuildSettings.activeBuildTarget.ToString(),
                ["dirtyScenes"] = dirty, ["actions"] = Actions, ["utcUnixMilliseconds"] = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds() };
        }

        public static Dictionary<string, object> ExportGraph()
        {
            RequireEditMode();
            if (PlayerSettings.GetScriptingBackend(NamedBuildTarget.Standalone) != ScriptingImplementation.Mono2x)
                throw new InvalidOperationException("Public source graph capture requires the pinned Standalone Mono profile.");
            bool previousDevelopment = EditorUserBuildSettings.development;
            try
            {
            EditorUserBuildSettings.development = true;
            var assemblies = new List<object>(); var hostAssemblies = new List<object>(); var scripts = new List<object>(); var hashes = new Dictionary<string, object>();
            var roots = PackageInfo.GetAllRegisteredPackages().Where(package => !string.IsNullOrEmpty(package.name) && !string.IsNullOrEmpty(package.resolvedPath))
                .OrderBy(package => package.name, StringComparer.Ordinal).Select(package => (object)new Dictionary<string, object> { ["path"] = "Packages/" + package.name, ["source"] = Path.GetFullPath(package.resolvedPath) }).ToArray();
            var playerAssemblies = CompilationPipeline.GetAssemblies(AssembliesType.PlayerWithoutTestAssemblies).OrderBy(assembly => assembly.name, StringComparer.Ordinal).ToArray();
            string profileSource = playerAssemblies.SelectMany(assembly => assembly.sourceFiles).FirstOrDefault();
            if (string.IsNullOrEmpty(profileSource)) throw new InvalidOperationException("Player graph has no source for its public compilation define profile.");
            // Query defaultDefines only. No AssemblyBuilder build is invoked,
            // and its required source is an existing registered Player script.
            var profile = new AssemblyBuilder(Path.Combine(Mailbox, "define-profile.dll"), new[] { ResolveEditorSource(profileSource) }) {
                buildTarget = BuildTarget.StandaloneLinux64, buildTargetGroup = BuildTargetGroup.Standalone,
                flags = AssemblyBuilderFlags.DevelopmentBuild };
            string[] developmentDefaults = profile.defaultDefines;
            var editorDefines = CompilationPipeline.GetAssemblies(AssembliesType.Editor).ToDictionary(assembly => assembly.name, assembly => assembly.defines, StringComparer.Ordinal);
            int sourceCount = 0;
            foreach (var assembly in playerAssemblies)
            {
                string definitionPath = CompilationPipeline.GetAssemblyDefinitionFilePathFromAssemblyName(assembly.name);
                var definition = string.IsNullOrEmpty(definitionPath) ? new Dictionary<string, object>() : AgentEditorJson.Parse(File.ReadAllText(ResolveEditorSource(definitionPath))) as Dictionary<string, object>;
                if (definition == null) throw new InvalidDataException("Assembly definition must be a JSON object.");
                // CompilationPipeline's Player inventory can include the test
                // framework graph even when a normal BuildPlayer omits it.
                // Exclude only explicit public test metadata/dependencies.
                if (TestAssembly(assembly.name, definition)) continue;
                bool game = assembly.sourceFiles.Any(x => x.StartsWith("Assets/", StringComparison.Ordinal));
                string[] playerDefines = assembly.defines.Concat(developmentDefaults).Distinct(StringComparer.Ordinal).OrderBy(x => x, StringComparer.Ordinal).ToArray();
                var responseFiles = assembly.compilerOptions.ResponseFiles ?? new string[0];
                var responseData = responseFiles.Select(file => CompilationPipeline.ParseResponseFile(file, Project,
                    CompilationPipeline.GetSystemAssemblyDirectories(assembly.compilerOptions.ApiCompatibilityLevel))).ToArray();
                if (responseData.Any(data => data.Errors.Length != 0)) throw new InvalidDataException("Public compiler response file contains parse errors.");
                playerDefines = playerDefines.Concat(responseData.SelectMany(data => data.Defines)).Distinct(StringComparer.Ordinal).OrderBy(x => x, StringComparer.Ordinal).ToArray();
                string[] compiledReferences = assembly.compiledAssemblyReferences.Concat(responseData.SelectMany(data => data.FullPathReferences)).Select(Path.GetFullPath).Distinct(StringComparer.Ordinal).ToArray();
                if (assemblies.Count + hostAssemblies.Count >= 256 || (sourceCount += assembly.sourceFiles.Length) > 65536) throw new InvalidDataException("Player graph exceeds its assembly/source bound.");
                var sources = assembly.sourceFiles.Select(ResolveEditorSource).OrderBy(x => x, StringComparer.Ordinal).ToArray();
                foreach (string logical in assembly.sourceFiles)
                {
                    string source = ResolveEditorSource(logical);
                    Canonical(source); hashes[source] = HashFile(source);
                    if (File.Exists(source + ".meta")) { Canonical(source + ".meta"); hashes[source + ".meta"] = HashFile(source + ".meta"); }
                    string relative = logical.Replace('\\', '/');
                    if (Path.IsPathRooted(relative) && source.StartsWith(Application.dataPath + Path.DirectorySeparatorChar, StringComparison.Ordinal)) relative = "Assets" + source.Substring(Application.dataPath.Length);
                    var script = AssetDatabase.LoadAssetAtPath<MonoScript>(relative); Type type = script == null ? null : script.GetClass();
                    if (!game && type != null && (!(typeof(MonoBehaviour).IsAssignableFrom(type) || typeof(ScriptableObject).IsAssignableFrom(type))
                        || EditorOnlyHierarchy(type))) continue;
                    if (type != null) scripts.Add(new Dictionary<string, object> { ["guid"] = AssetDatabase.AssetPathToGUID(relative), ["path"] = source,
                        ["assembly"] = assembly.name, ["typeName"] = type.FullName, ["hostProvided"] = !game });
                }
                string path = Path.GetFullPath(assembly.outputPath); Canonical(path);
                var configuration = new Dictionary<string, object> { ["name"] = assembly.name, ["sources"] = sources,
                    ["dependencies"] = assembly.assemblyReferences.Select(x => x.name).OrderBy(x => x, StringComparer.Ordinal).ToArray(),
                    ["defines"] = playerDefines, ["nativeBaseAssemblies"] = new string[0],
                    ["compilerOptions"] = (assembly.compilerOptions.AdditionalCompilerArguments ?? new string[0]).Concat(responseData.SelectMany(data => data.OtherArguments)).ToArray(),
                    ["responseFiles"] = responseFiles.Select(ResolveEditorSource).ToArray(),
                    ["config"] = new Dictionary<string, object> { ["allowUnsafeCode"] = assembly.compilerOptions.AllowUnsafeCode || responseData.Any(data => data.Unsafe),
                        ["noEngineReferences"] = Boolean(definition, "noEngineReferences"), ["overrideReferences"] = Boolean(definition, "overrideReferences"),
                        ["precompiledReferences"] = definition.TryGetValue("precompiledReferences", out object precompiledNative) ? precompiledNative : new string[0] } };
                if (!game) { hostAssemblies.Add(new Dictionary<string, object> { ["name"] = assembly.name, ["path"] = path, ["sha256"] = HashFile(path), ["sources"] = sources,
                    ["defines"] = playerDefines, ["editorDefines"] = editorDefines[assembly.name], ["configuration"] = configuration,
                    ["compiledReferences"] = compiledReferences,
                    ["responseFiles"] = responseFiles.Select(ResolveEditorSource).ToArray() }); continue; }
                assemblies.Add(new Dictionary<string, object> { ["name"] = assembly.name, ["path"] = path, ["sha256"] = HashFile(path), ["sources"] = sources,
                    ["references"] = assembly.assemblyReferences.Select(x => x.name).OrderBy(x => x, StringComparer.Ordinal).ToArray(),
                    ["compiledReferences"] = compiledReferences,
                    ["responseFiles"] = responseFiles.Select(ResolveEditorSource).ToArray(),
                    ["compilerOptions"] = (assembly.compilerOptions.AdditionalCompilerArguments ?? new string[0]).Concat(responseData.SelectMany(data => data.OtherArguments)).ToArray(),
                    ["defines"] = playerDefines, ["nativeBaseAssemblies"] = new string[0],
                    ["config"] = new Dictionary<string, object> { ["allowUnsafeCode"] = assembly.compilerOptions.AllowUnsafeCode || responseData.Any(data => data.Unsafe),
                        ["noEngineReferences"] = Boolean(definition, "noEngineReferences"), ["overrideReferences"] = Boolean(definition, "overrideReferences"),
                        ["precompiledReferences"] = definition.TryGetValue("precompiledReferences", out object precompiled) ? precompiled : new string[0] } });
            }
            foreach (string relative in new[] { "ProjectSettings/ProjectVersion.txt", "ProjectSettings/ProjectSettings.asset", "Packages/manifest.json", "Packages/packages-lock.json" })
            { string path = Path.Combine(Project, relative); if (File.Exists(path)) { Canonical(path); hashes[path] = HashFile(path); } }
            return new Dictionary<string, object> { ["schemaVersion"] = 1, ["backend"] = "unity_editor", ["project"] = Project,
                ["unity"] = Path.GetFullPath(EditorApplication.applicationPath), ["unityVersion"] = Application.unityVersion,
                ["target"] = "StandaloneLinux64", ["scriptingBackend"] = "Mono2x",
                ["development"] = true, ["assemblies"] = assemblies, ["hostAssemblies"] = hostAssemblies, ["scripts"] = scripts, ["sourceRoots"] = roots, ["compilerInputs"] = hashes };
            }
            finally { EditorUserBuildSettings.development = previousDevelopment; }
        }

        private static bool TestAssembly(string name, Dictionary<string, object> definition)
        {
            if (name == "UnityEngine.TestRunner" || name == "UnityEditor.TestRunner") return true;
            foreach (string field in new[] { "optionalUnityReferences", "references", "precompiledReferences", "defineConstraints" })
            {
                if (!definition.TryGetValue(field, out object value) || !(value is List<object> entries)) continue;
                foreach (object entry in entries)
                    if (entry is string text && (text == "TestAssemblies" || text == "UnityEngine.TestRunner" || text == "UnityEditor.TestRunner"
                        || text == "nunit.framework.dll" || text == "UNITY_INCLUDE_TESTS")) return true;
            }
            return false;
        }

        private static bool EditorOnlyHierarchy(Type type)
        {
            // MonoScript.GetClass reflects the Editor compilation of a runtime
            // asmdef. Editor-only ScriptableSingleton subclasses can therefore
            // appear here although their public base cannot exist in a Player.
            for (Type current = type; current != null; current = current.BaseType)
            {
                string definingAssembly = current.Assembly.GetName().Name;
                if (definingAssembly == "UnityEditor" || definingAssembly.StartsWith("UnityEditor.", StringComparison.Ordinal)) return true;
            }
            return false;
        }

        private static string ResolveEditorSource(string path)
        {
            if (!path.StartsWith("Packages/", StringComparison.Ordinal)) return Path.GetFullPath(path);
            PackageInfo package = PackageInfo.FindForAssetPath(path); if (package == null) throw new IOException("Cannot resolve Player package source: " + path);
            string prefix = "Packages/" + package.name + "/";
            if (!path.StartsWith(prefix, StringComparison.Ordinal)) throw new InvalidDataException("Package source has incompatible logical identity.");
            string root = Path.GetFullPath(package.resolvedPath), result = Path.GetFullPath(Path.Combine(root, path.Substring(prefix.Length)));
            if (!result.StartsWith(root + Path.DirectorySeparatorChar, StringComparison.Ordinal)) throw new InvalidDataException("Package source escapes its registered root.");
            return result;
        }

        public static Dictionary<string, object> SceneProof(string path)
        {
            RequireEditMode(); RequireCleanScenes(); path = ScenePath(path);
            string before = HashFile(Path.Combine(Project, path)); SceneSetup[] setup = EditorSceneManager.GetSceneManagerSetup();
            try
            {
                Scene scene = EditorSceneManager.OpenScene(path, OpenSceneMode.Single);
                var lighting = Lightmapping.lightingDataAsset; string guid = string.Empty; long fileId = 0;
                bool known = lighting != null && AssetDatabase.TryGetGUIDAndLocalFileIdentifier(lighting, out guid, out fileId);
                return new Dictionary<string, object> { ["schemaVersion"] = 1, ["backend"] = "unity_editor", ["projectRoot"] = Project,
                    ["unityVersion"] = Application.unityVersion, ["scenePath"] = path, ["sceneSHA256"] = before,
                    ["lightmapCount"] = LightmapSettings.lightmaps.Length, ["lightProbeCount"] = LightmapSettings.lightProbes == null ? 0 : LightmapSettings.lightProbes.count,
                    ["activeSceneCount"] = SceneManager.sceneCount, ["isPlaying"] = EditorApplication.isPlaying, ["sceneDirty"] = scene.isDirty,
                    ["lightingDataAssetPath"] = lighting == null ? null : AssetDatabase.GetAssetPath(lighting),
                    ["lightingDataIdentityKnown"] = known, ["lightingDataGuid"] = guid, ["lightingDataFileId"] = fileId };
            }
            finally
            {
                RestoreSceneSetup(setup);
                if (HashFile(Path.Combine(Project, path)) != before) throw new IOException("Authored scene changed while exporting its proof.");
            }
        }

        private static Dictionary<string, object> BuildProject(Dictionary<string, object> args, string id)
        {
            RequireEditMode(); RequireCleanScenes();
            if (EditorUserBuildSettings.activeBuildTarget != BuildTarget.StandaloneLinux64 || !BuildPipeline.IsBuildTargetSupported(BuildTargetGroup.Standalone, BuildTarget.StandaloneLinux64))
                throw new InvalidOperationException("Project builds require installed Linux64 support and an Editor already on that target.");
            if (!args.TryGetValue("scenes", out object value) || !(value is List<object> entries) || entries.Count < 1 || entries.Count > 64)
                throw new InvalidDataException("Build scenes must be an explicit saved list of 1 to 64 Assets/...unity paths.");
            string[] scenes = entries.Select(x => ScenePath(x as string ?? string.Empty)).ToArray();
            if (scenes.Distinct(StringComparer.Ordinal).Count() != scenes.Length) throw new InvalidDataException("Build scenes contain duplicates.");
            string directory = Text(args, "outputDirectory");
            if (directory.Length == 0) directory = Path.Combine(Project, "Builds/UnityMicroEditor", id);
            directory = Path.GetFullPath(directory);
            string root = Path.Combine(Project, "Builds/UnityMicroEditor") + Path.DirectorySeparatorChar;
            if (!directory.StartsWith(root, StringComparison.Ordinal)) throw new InvalidDataException("Build output must be a new directory inside this project's Builds/UnityMicroEditor.");
            Canonical(directory); if (Directory.Exists(directory) || File.Exists(directory)) throw new IOException("Build output already exists; refusing to overwrite it.");
            Directory.CreateDirectory(directory); string output = Path.Combine(directory, "Project.x86_64");
            ScriptingImplementation previous = PlayerSettings.GetScriptingBackend(NamedBuildTarget.Standalone);
            bool development = Boolean(args, "development"); SceneSetup[] setup = EditorSceneManager.GetSceneManagerSetup();
            try
            {
                PlayerSettings.SetScriptingBackend(NamedBuildTarget.Standalone, ScriptingImplementation.Mono2x);
                BuildReport report = BuildPipeline.BuildPlayer(new BuildPlayerOptions { scenes = scenes, locationPathName = output,
                    target = BuildTarget.StandaloneLinux64, targetGroup = BuildTargetGroup.Standalone, subtarget = (int)StandaloneBuildSubtarget.Player,
                    options = development ? BuildOptions.Development : BuildOptions.None });
                var result = new Dictionary<string, object> { ["backend"] = "unity_editor", ["outputDirectory"] = directory, ["outputPath"] = output,
                    ["scenes"] = scenes, ["target"] = "StandaloneLinux64", ["scriptingBackend"] = "Mono2x", ["development"] = development,
                    ["buildResult"] = report.summary.result.ToString(), ["totalBytes"] = report.summary.totalSize,
                    ["elapsedSeconds"] = report.summary.totalTime.TotalSeconds, ["errors"] = report.summary.totalErrors, ["warnings"] = report.summary.totalWarnings };
                // What failed, as the report's steps name it.
                if (report.summary.result != BuildResult.Succeeded)
                    result["errorMessages"] = report.steps.SelectMany(step => step.messages.Where(message => message.type == LogType.Error || message.type == LogType.Exception)
                        .Select(message => step.name + ": " + Bounded(message.content, 2000))).Take(20).ToArray();
                return result;
            }
            finally
            {
                try { PlayerSettings.SetScriptingBackend(NamedBuildTarget.Standalone, previous); }
                finally { try { SavePlayerSettings(); } finally { RestoreSceneSetup(setup); } }
            }
        }

        internal static HashSet<string> HostExcludedAssemblies;

        private static Dictionary<string, object> BuildHost(Dictionary<string, object> arguments, string id)
        {
            RequireEditMode(); RequireCleanScenes();
            string parent = "Assets/UnityMicroEditorGenerated";
            string scratch = parent + "/host-" + id;
            Canonical(Path.Combine(Project, scratch));
            if (Directory.Exists(scratch) || File.Exists(scratch + ".meta")) throw new IOException("Owned host scratch already exists; refusing to overwrite it.");
            bool createdParent = !AssetDatabase.IsValidFolder(parent);
            SceneSetup[] setup = EditorSceneManager.GetSceneManagerSetup();
            var excluded = new HashSet<string>(CompilationPipeline.GetAssemblies(AssembliesType.PlayerWithoutTestAssemblies)
                .Where(x => x.name != "UnityMicroEditor" && x.sourceFiles.Any(source => source.StartsWith("Assets/", StringComparison.Ordinal)))
                .Select(x => x.name), StringComparer.Ordinal);
            ManagedStrippingLevel stripping = PlayerSettings.GetManagedStrippingLevel(NamedBuildTarget.Standalone);
            bool stripEngine = PlayerSettings.stripEngineCode, background = PlayerSettings.runInBackground;
            SpritePackerMode spritePacker = EditorSettings.spritePackerMode;
            bool defaultGraphics = PlayerSettings.GetUseDefaultGraphicsAPIs(BuildTarget.StandaloneLinux64);
            GraphicsDeviceType[] graphics = PlayerSettings.GetGraphicsAPIs(BuildTarget.StandaloneLinux64);
            PreloadedIdentity[] preloaded = CapturePreloadedAssets();
            var sourceProfile = CaptureHostSourceProfile();
            try
            {
                if (createdParent && AssetDatabase.CreateFolder("Assets", "UnityMicroEditorGenerated").Length == 0) throw new IOException("Cannot create owned host scratch parent.");
                if (AssetDatabase.CreateFolder(parent, "host-" + id).Length == 0) throw new IOException("Cannot create owned host scratch.");
                AssetDatabase.CreateFolder(scratch, "Resources");
                string resources = scratch + "/Resources";
                foreach (string name in new[] { "Standard", "Unlit/Texture", "Unlit/Color" })
                {
                    Shader shader = Shader.Find(name); if (shader == null) throw new InvalidOperationException("Host shader is missing: " + name);
                    var material = new Material(shader); AssetDatabase.CreateAsset(material, resources + "/" + name.Replace('/', '_') + ".mat"); AssetDatabase.SaveAssetIfDirty(material);
                }
                Material skybox = AssetDatabase.GetBuiltinExtraResource<Material>("Default-Skybox.mat");
                if (skybox == null) throw new InvalidOperationException("The installed engine has no native Default-Skybox material.");
                if (AssetDatabase.CreateFolder(resources, "UnityPlayHostBuiltins").Length == 0) throw new IOException("Cannot create owned native built-in resources.");
                var skyboxCopy = new Material(skybox) { name = skybox.name };
                AssetDatabase.CreateAsset(skyboxCopy, resources + "/UnityPlayHostBuiltins/DefaultSkybox.mat");
                AssetDatabase.SaveAssetIfDirty(skyboxCopy);
                Scene scene = EditorSceneManager.NewScene(NewSceneSetup.EmptyScene, NewSceneMode.Single);
                string scenePath = scratch + "/Bootstrap.unity";
                if (!EditorSceneManager.SaveScene(scene, scenePath)) throw new IOException("Cannot save exclusive host bootstrap scene.");
                // The game loads the scenes of its Build Settings through
                // SceneManager by name, path or index. An empty scene of the same
                // name stands in for each at its build index; the raw session
                // builds the authored scene into it when the engine loads it.
                var builtScenes = new List<object> { scenePath };
                var standIns = new List<object>();
                EditorBuildSettingsScene[] enabledScenes = EditorBuildSettings.scenes.Where(x => x.enabled).ToArray();
                for (int index = 1; index < enabledScenes.Length; index++)
                {
                    string number = index.ToString(CultureInfo.InvariantCulture), folder = scratch + "/BuildScenes";
                    if (!AssetDatabase.IsValidFolder(folder) && AssetDatabase.CreateFolder(scratch, "BuildScenes").Length == 0) throw new IOException("Cannot create owned stand-in scene folder.");
                    if (AssetDatabase.CreateFolder(folder, number).Length == 0) throw new IOException("Cannot create owned stand-in scene folder.");
                    Scene standIn = EditorSceneManager.NewScene(NewSceneSetup.EmptyScene, NewSceneMode.Single);
                    string standInPath = folder + "/" + number + "/" + Path.GetFileNameWithoutExtension(enabledScenes[index].path) + ".unity";
                    if (!EditorSceneManager.SaveScene(standIn, standInPath)) throw new IOException("Cannot save a stand-in scene: " + standInPath);
                    builtScenes.Add(standInPath);
                    standIns.Add(new Dictionary<string, object> { ["index"] = index, ["path"] = enabledScenes[index].path, ["standIn"] = standInPath });
                }
                var buildArguments = new Dictionary<string, object>(arguments) { ["scenes"] = builtScenes, ["development"] = true };
                PlayerSettings.SetManagedStrippingLevel(NamedBuildTarget.Standalone, ManagedStrippingLevel.Disabled);
                PlayerSettings.stripEngineCode = false; PlayerSettings.runInBackground = true;
                // Authored preloads are reconstructed by the raw session.
                // Embedding them here would initialize gameplay before raw
                // hydration or require the excluded original gameplay DLLs.
                PlayerSettings.SetPreloadedAssets(Array.Empty<UnityEngine.Object>());
                // The host carries none of the game's content; its sprite
                // atlases come with the session's imports. Packing them for a
                // Linux build can fail on sizes set for a device alone.
                EditorSettings.spritePackerMode = SpritePackerMode.Disabled;
                PlayerSettings.SetUseDefaultGraphicsAPIs(BuildTarget.StandaloneLinux64, false);
                PlayerSettings.SetGraphicsAPIs(BuildTarget.StandaloneLinux64, new[] { GraphicsDeviceType.OpenGLCore });
                HostExcludedAssemblies = excluded;
                Dictionary<string, object> result = BuildProject(buildArguments, id);
                if (Text(result, "buildResult") != "Succeeded")
                    throw new InvalidOperationException("The host build failed (" + Text(result, "buildResult") + "): "
                        + string.Join(" | ", result.TryGetValue("errorMessages", out object messages) ? (string[])messages : Array.Empty<string>()));
                var afterBuild = CaptureHostSourceProfile();
                // A project's build callbacks may switch which precompiled
                // plugins a Player build takes (Odin Inspector swaps in its
                // NoEditor assemblies); the references after the build are the
                // Player's. Sources and configuration have to stay as they were.
                if (AgentEditorJson.Write(WithoutReferences(sourceProfile)) != AgentEditorJson.Write(WithoutReferences(afterBuild)))
                    throw new InvalidDataException("Host runtime sources/configuration changed while BuildPipeline executed: " + ProfileDifference(sourceProfile, afterBuild));
                sourceProfile = afterBuild;
                string player = Text(result, "outputPath");
                string playerData = Path.Combine(Path.GetDirectoryName(player), Path.GetFileNameWithoutExtension(player) + "_Data");
                var sourceAssemblies = (Dictionary<string, object>)sourceProfile["assemblies"];
                foreach (var pair in sourceAssemblies)
                {
                    var entry = (Dictionary<string, object>)pair.Value;
                    string dll = Path.Combine(playerData, "Managed", pair.Key + ".dll");
                    if (!File.Exists(dll)) throw new InvalidDataException("Built host lacks required runtime assembly: " + pair.Key);
                    entry["dllSha256"] = HashFile(dll);
                }
                sourceProfile["schemaVersion"] = 1; sourceProfile["backend"] = "unity_editor_build_sources";
                sourceProfile["unityVersion"] = Application.unityVersion; sourceProfile["target"] = "StandaloneLinux64";
                sourceProfile["scriptingBackend"] = "Mono2x"; sourceProfile["development"] = true;
                string sourceProof = Path.Combine(playerData, "UnityMicroEditorSourceProfile.json");
				if (File.Exists(sourceProof)) throw new IOException("Host source build proof already exists.");
				ReplaceOwned(sourceProof, AgentEditorJson.Write(sourceProfile));
                result["sourceBuildProof"] = sourceProof; result["sourceBuildProofSha256"] = HashFile(sourceProof);
                result["purpose"] = "raw_host"; result["excludedGameAssemblies"] = excluded.OrderBy(x => x, StringComparer.Ordinal).ToArray();
                result["hostDevelopment"] = true; result["buildSceneStandIns"] = standIns.ToArray();
                result["graphicsApi"] = "OpenGLCore"; result["packageProfile"] = HashFile(Path.Combine(Project, "Packages/manifest.json"));
                return result;
            }
            finally
            {
                HostExcludedAssemblies = null;
                try
                {
                    Exception restoreError = null;
                    foreach (Action restore in new Action[] {
                        () => PlayerSettings.SetManagedStrippingLevel(NamedBuildTarget.Standalone, stripping),
                        () => PlayerSettings.stripEngineCode = stripEngine,
                        () => PlayerSettings.runInBackground = background,
                        () => EditorSettings.spritePackerMode = spritePacker,
                        () => RestorePreloadedAssets(preloaded),
                        () => PlayerSettings.SetGraphicsAPIs(BuildTarget.StandaloneLinux64, graphics),
                        () => PlayerSettings.SetUseDefaultGraphicsAPIs(BuildTarget.StandaloneLinux64, defaultGraphics),
                        () => RestoreSceneSetup(setup),
                        () => SavePlayerSettings() })
                    {
                        try { restore(); }
                        catch (Exception error) { if (restoreError == null) restoreError = error; }
                    }
                    if (restoreError != null) throw new InvalidOperationException("Cannot restore host project state: " + restoreError.Message, restoreError);
                }
                finally
                {
                    if (AssetDatabase.IsValidFolder(scratch) && !AssetDatabase.DeleteAsset(scratch)) throw new IOException("Cannot remove owned host scratch.");
                    if (createdParent && AssetDatabase.IsValidFolder(parent) && Directory.GetFileSystemEntries(parent).Length == 0) AssetDatabase.DeleteAsset(parent);
                }
            }
        }

        private static Dictionary<string, object> CaptureHostSourceProfile()
        {
            var graph = ExportGraph();
            var roots = ((object[])graph["sourceRoots"]).Cast<Dictionary<string, object>>().ToArray();
            Func<string, string> logical = path => {
                path = Path.GetFullPath(path);
                if (path.StartsWith(Application.dataPath + Path.DirectorySeparatorChar, StringComparison.Ordinal)) return "Assets" + path.Substring(Application.dataPath.Length).Replace('\\', '/');
                foreach (var root in roots) {
                    string source = Text(root, "source");
                    if (path.StartsWith(source + Path.DirectorySeparatorChar, StringComparison.Ordinal)) return Text(root, "path") + "/" + path.Substring(source.Length + 1).Replace('\\', '/');
                }
                return path;
            };
            var entries = new Dictionary<string, object>();
            foreach (var raw in (List<object>)graph["hostAssemblies"])
            {
                var assembly = (Dictionary<string, object>)raw;
                var configuration = new Dictionary<string, object>((Dictionary<string, object>)assembly["configuration"]);
                string name = Text(assembly, "name");
                string[] sources = (string[])assembly["sources"];
                var hashes = new Dictionary<string, object>();
                foreach (string source in sources) {
                    hashes[logical(source)] = HashFile(source);
                    if (File.Exists(source + ".meta")) hashes[logical(source) + ".meta"] = HashFile(source + ".meta");
                }
                string definition = CompilationPipeline.GetAssemblyDefinitionFilePathFromAssemblyName(name);
                if (!string.IsNullOrEmpty(definition)) {
                    definition = ResolveEditorSource(definition);
                    hashes[logical(definition)] = HashFile(definition);
                    if (File.Exists(definition + ".meta")) hashes[logical(definition) + ".meta"] = HashFile(definition + ".meta");
                }
                configuration["sources"] = sources.Select(logical).OrderBy(x => x, StringComparer.Ordinal).ToArray();
                configuration["responseFiles"] = ((string[])configuration["responseFiles"]).Select(logical).OrderBy(x => x, StringComparer.Ordinal).ToArray();
                var references = new Dictionary<string, object>();
                foreach (string reference in (string[])assembly["compiledReferences"]) references[logical(reference)] = HashFile(reference);
                foreach (string response in (string[])assembly["responseFiles"]) hashes[logical(response)] = HashFile(response);
                entries[name] = new Dictionary<string, object> { ["name"] = name, ["configuration"] = configuration, ["sources"] = hashes, ["references"] = references };
            }
            return new Dictionary<string, object> { ["assemblies"] = entries };
        }

        private static Dictionary<string, object> WithoutReferences(Dictionary<string, object> profile)
        {
            var assemblies = new Dictionary<string, object>();
            foreach (var entry in (Dictionary<string, object>)profile["assemblies"])
            {
                var copy = new Dictionary<string, object>((Dictionary<string, object>)entry.Value);
                copy.Remove("references");
                assemblies[entry.Key] = copy;
            }
            return new Dictionary<string, object> { ["assemblies"] = assemblies };
        }

        // The first assembly and member whose captured value differs, for the error.
        private static string ProfileDifference(Dictionary<string, object> before, Dictionary<string, object> after)
        {
            var first = (Dictionary<string, object>)before["assemblies"];
            var second = (Dictionary<string, object>)after["assemblies"];
            foreach (string name in first.Keys.Union(second.Keys).OrderBy(x => x, StringComparer.Ordinal))
            {
                if (!first.TryGetValue(name, out object a)) return name + " appeared";
                if (!second.TryGetValue(name, out object b)) return name + " disappeared";
                var x = (Dictionary<string, object>)a; var y = (Dictionary<string, object>)b;
                foreach (string part in new[] { "configuration", "sources", "references" })
                {
                    var p = (Dictionary<string, object>)x[part]; var q = (Dictionary<string, object>)y[part];
                    foreach (string key in p.Keys.Union(q.Keys).OrderBy(k => k, StringComparer.Ordinal))
                    {
                        string left = p.TryGetValue(key, out object l) ? AgentEditorJson.Write(l) : "<absent>";
                        string right = q.TryGetValue(key, out object r) ? AgentEditorJson.Write(r) : "<absent>";
                        if (left != right) return name + " " + part + " " + key + ": " + Bounded(left, 400) + " -> " + Bounded(right, 400);
                    }
                }
            }
            return "no member differs";
        }

        private sealed class PreloadedIdentity
        { internal string Path, Guid; internal long FileId; internal bool IsNull; }

        private static PreloadedIdentity[] CapturePreloadedAssets()
        {
            return PlayerSettings.GetPreloadedAssets().Select(asset =>
            {
                if (asset == null) return new PreloadedIdentity { IsNull = true };
                string path = AssetDatabase.GetAssetPath(asset);
                if (path.Length == 0 || !AssetDatabase.TryGetGUIDAndLocalFileIdentifier(asset, out string guid, out long fileId))
                    throw new InvalidDataException("A preloaded asset lacks its authored persistent identity.");
                return new PreloadedIdentity { Path = path, Guid = guid, FileId = fileId };
            }).ToArray();
        }

        private static void RestorePreloadedAssets(PreloadedIdentity[] identities)
        {
            var assets = new List<UnityEngine.Object>();
            foreach (PreloadedIdentity identity in identities)
            {
                if (identity.IsNull) { assets.Add(null); continue; }
                // BuildPipeline can invalidate source handles. Reacquire the
                // persistent object rather than restoring a stale reference.
                var candidates = new List<UnityEngine.Object>(AssetDatabase.LoadAllAssetsAtPath(identity.Path));
                foreach (GameObject owner in candidates.OfType<GameObject>().ToArray())
                {
                    candidates.AddRange(owner.GetComponentsInChildren<Component>(true));
                    foreach (Transform child in owner.GetComponentsInChildren<Transform>(true)) candidates.Add(child.gameObject);
                }
                UnityEngine.Object restored = candidates.FirstOrDefault(asset => asset != null && AssetDatabase.TryGetGUIDAndLocalFileIdentifier(asset, out string guid, out long fileId) && guid == identity.Guid && fileId == identity.FileId);
                if (restored == null) throw new IOException("Could not restore authored preloaded asset identity: " + identity.Path);
                assets.Add(restored);
            }
            PlayerSettings.SetPreloadedAssets(assets.ToArray());
        }

        internal static bool SavingPlayerSettings;

        internal static void SavePlayerSettings()
        {
            PlayerSettings[] settings = Resources.FindObjectsOfTypeAll<PlayerSettings>()
                .Where(value => AssetDatabase.GetAssetPath(value) == "ProjectSettings/ProjectSettings.asset").ToArray();
            if (settings.Length != 1) throw new InvalidOperationException("Cannot identify the project PlayerSettings object for scoped persistence.");
            EditorUtility.SetDirty(settings[0]);
            // SaveAssetIfDirty does not flush the PlayerSettings backing file
            // in the verified Editor. SaveAssets does, with our public save
            // callback restricting publication to this exact settings path.
            SavingPlayerSettings = true;
            try { AssetDatabase.SaveAssets(); }
            finally { SavingPlayerSettings = false; }
        }

        private static void RequireStaging()
        {
            if (!Application.isBatchMode) throw new InvalidOperationException("Prefab preparation requires an isolated batch staging project.");
            string marker = Path.Combine(Project, "ProjectSettings/UnityMicroEditorStaging.json"); Canonical(marker);
            if (new FileInfo(marker).Length > 65536) throw new InvalidDataException("Staging marker exceeds its byte bound.");
            var proof = AgentEditorJson.Parse(File.ReadAllText(marker)) as Dictionary<string, object>;
            string source = proof == null ? string.Empty : Text(proof, "sourceProject");
            if (proof == null || Integer(proof, "schemaVersion") != 1 || !Path.IsPathRooted(source) || Path.GetFullPath(source) != source || source == Project)
                throw new InvalidDataException("Prefab preparation requires an explicit isolated staging-project marker.");
            Canonical(source); if (!Directory.Exists(source)) throw new DirectoryNotFoundException("Staging source project is absent.");
        }

        private static void RequireEditMode()
        { if (EditorApplication.isPlayingOrWillChangePlaymode) throw new InvalidOperationException("This operation requires Editor Edit Mode."); }

        internal static void RestoreSceneSetup(SceneSetup[] setup)
        {
            // GetSceneManagerSetup omits an unsaved empty headless scene.
            // RestoreSceneManagerSetup cannot accept that empty snapshot.
            if (setup.Length == 0 || !setup.Any(scene => scene.isLoaded && scene.isActive))
                EditorSceneManager.NewScene(NewSceneSetup.EmptyScene, NewSceneMode.Single);
            else EditorSceneManager.RestoreSceneManagerSetup(setup);
        }

        private static void RequireCleanScenes()
        {
            for (int index = 0; index < SceneManager.sceneCount; index++)
            {
                Scene scene = SceneManager.GetSceneAt(index);
                if (scene.isDirty || (scene.path.Length == 0 && scene.rootCount != 0)) throw new InvalidOperationException("Operation refuses dirty or unsaved loaded scenes: " + scene.path);
            }
        }

        private static string ScenePath(string path)
        {
            if (string.IsNullOrEmpty(path) || !path.StartsWith("Assets/", StringComparison.Ordinal) || path.Contains("\\") || !path.EndsWith(".unity", StringComparison.Ordinal))
                throw new InvalidDataException("Scene must be a saved Assets/...unity path.");
            string absolute = Path.GetFullPath(Path.Combine(Project, path));
            if (!absolute.StartsWith(Application.dataPath + Path.DirectorySeparatorChar, StringComparison.Ordinal) || absolute.Substring(Project.Length + 1).Replace('\\', '/') != path)
                throw new InvalidDataException("Scene path must be canonical inside Assets.");
            Canonical(absolute); if (AssetDatabase.LoadAssetAtPath<SceneAsset>(path) == null) throw new FileNotFoundException("Saved scene asset does not exist: " + path);
            return path;
        }

        private static void Reply(string id, string hash, object result, string code, string message)
        {
            try { PublishReply(id, hash, result, code, message); }
            catch (Exception error)
            {
                Debug.LogWarning("[editor-bridge] response unavailable " + id + ": " + Bounded(error.Message, 1024));
                try { PublishReply(id, hash, null, "outcome_unknown", "The request was accepted but its result could not be published; inspect state before issuing another ID."); }
                catch (Exception failure) { Debug.LogWarning("[editor-bridge] receipt remains unavailable: " + Bounded(failure.Message, 1024)); }
            }
        }

        private static void PublishReply(string id, string hash, object result, string code, string message)
        {
            var response = new Dictionary<string, object> { ["schemaVersion"] = 1, ["backend"] = "unity_editor", ["requestId"] = id,
                ["requestSha256"] = hash, ["project"] = Project, ["unityVersion"] = Profile, ["editor"] = Path.GetFullPath(EditorApplication.applicationPath),
                ["pid"] = Pid, ["status"] = "completed", ["success"] = code == null, ["result"] = result,
                ["error"] = code == null ? null : new Dictionary<string, object> { ["code"] = code, ["message"] = Bounded(message, 8192) } };
            byte[] bytes = new UTF8Encoding(false, true).GetBytes(AgentEditorJson.Write(response) + "\n");
            if (bytes.Length > ResponseLimit) throw new InvalidDataException("Editor result exceeds its byte bound.");
            string destination = Path.Combine(Mailbox, id + ".response.json"); Canonical(destination);
            string temporary = destination + "." + Guid.NewGuid().ToString("N") + ".tmp";
            try { File.WriteAllBytes(temporary, bytes); File.Move(temporary, destination); }
            finally { if (File.Exists(temporary)) File.Delete(temporary); }
        }

        private static void ReplaceOwned(string path, string text)
        {
            Canonical(path); string temporary = path + "." + Guid.NewGuid().ToString("N") + ".tmp";
            try { File.WriteAllText(temporary, text + "\n", new UTF8Encoding(false)); if (File.Exists(path)) File.Replace(temporary, path, null); else File.Move(temporary, path); }
            finally { if (File.Exists(temporary)) File.Delete(temporary); }
        }

        internal static void Canonical(string path)
        {
            if (!Path.IsPathRooted(path) || Path.GetFullPath(path) != path) throw new InvalidDataException("Bridge path must be absolute and canonical.");
            string current = path;
            while (!string.IsNullOrEmpty(current))
            {
                if ((File.Exists(current) || Directory.Exists(current)) && (File.GetAttributes(current) & FileAttributes.ReparsePoint) != 0)
                    throw new InvalidDataException("Bridge paths cannot contain symlinks.");
                current = Path.GetDirectoryName(current);
            }
        }

        private static bool ValidId(string id)
        { return !string.IsNullOrEmpty(id) && id.Length <= 64 && id.All(c => (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == '-'); }
        private static string Text(Dictionary<string, object> map, string key) { return map.TryGetValue(key, out object value) ? value as string ?? string.Empty : string.Empty; }
        private static long Integer(Dictionary<string, object> map, string key)
        { if (!map.TryGetValue(key, out object value)) return 0; if (!(value is long number)) throw new InvalidDataException("Expected integer: " + key); return number; }
        private static bool Boolean(Dictionary<string, object> map, string key)
        { if (!map.TryGetValue(key, out object value)) return false; if (!(value is bool boolean)) throw new InvalidDataException("Expected boolean: " + key); return boolean; }
        private static string Bounded(string text, int limit) { text = text ?? string.Empty; return text.Substring(0, Math.Min(text.Length, limit)); }
        private static string Hash(byte[] bytes) { using (var hash = SHA256.Create()) return BitConverter.ToString(hash.ComputeHash(bytes)).Replace("-", string.Empty).ToLowerInvariant(); }
        private static string HashFile(string path) { using (var stream = File.OpenRead(path)) using (var hash = SHA256.Create()) return BitConverter.ToString(hash.ComputeHash(stream)).Replace("-", string.Empty).ToLowerInvariant(); }
    }

    // Filter only during our explicit raw-host build. Gameplay DLLs are later
    // supplied by the verified SDK compiler; their native startup table must
    // not execute uninstrumented project initializers before raw scene loading.
    internal sealed class AgentSettingsSaveFilter : AssetModificationProcessor
    {
        private static string[] OnWillSaveAssets(string[] paths)
        {
            if (!AgentEditorBridge.SavingPlayerSettings) return paths;
            return paths.Where(path => path == "ProjectSettings/ProjectSettings.asset").ToArray();
        }
    }

    internal sealed class AgentHostAssemblyFilter : IFilterBuildAssemblies
    {
        public int callbackOrder { get { return int.MaxValue; } }
        public string[] OnFilterAssemblies(BuildOptions options, string[] assemblies)
        {
            HashSet<string> excluded = AgentEditorBridge.HostExcludedAssemblies;
            return excluded == null ? assemblies : assemblies.Where(path => !excluded.Contains(Path.GetFileNameWithoutExtension(path))).ToArray();
        }
    }

    // Independent of Coplay and optional JSON packages. The small strict parser
    // shares the raw bridge's finite-number and duplicate-property protocol.
    internal static class AgentEditorJson
    {
        internal static object Parse(string text) { var reader = new Reader(text); object value = reader.Value(0); reader.Finish(); return value; }
        internal static string Write(object value) { var result = new StringBuilder(); Append(result, value, 0); return result.ToString(); }
        private static void Append(StringBuilder output, object value, int depth)
        {
            if (depth > 128) throw new InvalidDataException("JSON nesting exceeds bounds.");
            if (value == null) { output.Append("null"); return; }
            if (value is string text) { Quote(output, text); return; }
            if (value is bool boolean) { output.Append(boolean ? "true" : "false"); return; }
            if (value is IDictionary map)
            {
                output.Append('{'); bool first = true;
                foreach (DictionaryEntry pair in map) { if (!(pair.Key is string key)) throw new InvalidDataException("JSON object keys must be strings."); if (!first) output.Append(','); first = false; Quote(output, key); output.Append(':'); Append(output, pair.Value, depth + 1); }
                output.Append('}'); return;
            }
            if (value is IEnumerable items)
            { output.Append('['); bool first = true; foreach (object item in items) { if (!first) output.Append(','); first = false; Append(output, item, depth + 1); } output.Append(']'); return; }
            if (!(value is byte) && !(value is sbyte) && !(value is short) && !(value is ushort) && !(value is int) && !(value is uint) && !(value is long) && !(value is ulong) && !(value is float) && !(value is double) && !(value is decimal)) throw new InvalidDataException("Unsupported JSON value type.");
            if (value is float single && (float.IsNaN(single) || float.IsInfinity(single)) || value is double number && (double.IsNaN(number) || double.IsInfinity(number))) throw new InvalidDataException("JSON numbers must be finite.");
            output.Append(Convert.ToString(value, CultureInfo.InvariantCulture));
        }
        private static void Quote(StringBuilder output, string text)
        {
            output.Append('"'); foreach (char value in text) switch (value) { case '"': output.Append("\\\""); break; case '\\': output.Append("\\\\"); break; case '\n': output.Append("\\n"); break; case '\r': output.Append("\\r"); break; case '\t': output.Append("\\t"); break; default: if (value < 32) output.Append("\\u").Append(((int)value).ToString("x4")); else output.Append(value); break; } output.Append('"');
        }
        private sealed class Reader
        {
            private readonly string text; private int index;
            internal Reader(string text) { this.text = text ?? throw new ArgumentNullException(nameof(text)); }
            internal void Finish() { Space(); if (index != text.Length) Fail("Trailing JSON data"); }
            internal object Value(int depth)
            {
                if (depth > 128) Fail("JSON nesting exceeds bounds"); Space(); if (index == text.Length) Fail("Unexpected JSON end"); char next = text[index];
                if (next == '"') return String();
                if (next == '{') { index++; var map = new Dictionary<string, object>(StringComparer.Ordinal); Space(); if (Take('}')) return map; while (true) { Space(); string key = String(); Space(); Require(':'); if (map.ContainsKey(key)) Fail("Duplicate JSON key"); map.Add(key, Value(depth + 1)); Space(); if (Take('}')) return map; Require(','); } }
                if (next == '[') { index++; var list = new List<object>(); Space(); if (Take(']')) return list; while (true) { list.Add(Value(depth + 1)); Space(); if (Take(']')) return list; Require(','); } }
                if (next == 't') { Literal("true"); return true; } if (next == 'f') { Literal("false"); return false; } if (next == 'n') { Literal("null"); return null; }
                int start = index; Take('-'); if (index == text.Length || !Digit(text[index])) Fail("Invalid number"); if (!Take('0')) while (index < text.Length && Digit(text[index])) index++;
                if (Take('.')) { int before = index; while (index < text.Length && Digit(text[index])) index++; if (before == index) Fail("Invalid fraction"); }
                if (Take('e') || Take('E')) { if (!Take('+')) Take('-'); int before = index; while (index < text.Length && Digit(text[index])) index++; if (before == index) Fail("Invalid exponent"); }
                string token = text.Substring(start, index - start); if (long.TryParse(token, NumberStyles.Integer, CultureInfo.InvariantCulture, out long integer)) return integer;
                if (!double.TryParse(token, NumberStyles.Float, CultureInfo.InvariantCulture, out double number) || double.IsNaN(number) || double.IsInfinity(number)) Fail("Invalid finite number"); return number;
            }
            private string String()
            {
                Require('"'); var output = new StringBuilder(); while (index < text.Length) { char value = text[index++]; if (value == '"') return output.ToString(); if (value < 32) Fail("Control character in string"); if (value != '\\') { output.Append(value); continue; } if (index == text.Length) Fail("Unfinished escape"); switch (text[index++]) { case '"': output.Append('"'); break; case '\\': output.Append('\\'); break; case '/': output.Append('/'); break; case 'b': output.Append('\b'); break; case 'f': output.Append('\f'); break; case 'n': output.Append('\n'); break; case 'r': output.Append('\r'); break; case 't': output.Append('\t'); break; case 'u': if (index + 4 > text.Length || !ushort.TryParse(text.Substring(index, 4), NumberStyles.HexNumber, CultureInfo.InvariantCulture, out ushort code)) Fail("Invalid Unicode escape"); else { output.Append((char)code); index += 4; } break; default: Fail("Invalid escape"); break; } } Fail("Unfinished string"); return null;
            }
            private void Literal(string value) { if (index + value.Length > text.Length || text.Substring(index, value.Length) != value) Fail("Invalid literal"); index += value.Length; }
            private void Space() { while (index < text.Length && (text[index] == ' ' || text[index] == '\n' || text[index] == '\r' || text[index] == '\t')) index++; }
            private static bool Digit(char value) { return value >= '0' && value <= '9'; }
            private bool Take(char value) { if (index < text.Length && text[index] == value) { index++; return true; } return false; }
            private void Require(char value) { if (!Take(value)) Fail("Expected JSON character " + value); }
            private void Fail(string message) { throw new InvalidDataException(message + " at " + index + "."); }
        }
    }
}
