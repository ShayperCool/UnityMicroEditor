// Validation helper only. Copy into Assets/Editor of an explicitly owned project.
using System;
using System.Collections;
using System.Collections.Generic;
using System.IO;
using System.Reflection;
using System.Security.Cryptography;
using Dungeon.CLIPlay;
using UnityEditor;
using UnityEditor.SceneManagement;
using UnityEngine;
using UnityEngine.LowLevel;
using UnityEngine.SceneManagement;
using Object = UnityEngine.Object;

namespace AgentParity.Editor
{
    [InitializeOnLoad]
    public static class DungeonEditorReference
    {
        private const string State = "DungeonEditorReference.";
        private static readonly List<LogEntry> logs = new List<LogEntry>();
        private static bool finishing;

        [Serializable] private sealed class Paths { public string[] values; }
        [Serializable] private sealed class LogEntry { public string type, message, stack; }
        [Serializable] private sealed class ComponentInfo { public string type, scriptPath, scriptGuid; }
        [Serializable] private sealed class ObjectInfo
        {
            public string path, scene; public bool activeSelf, activeInHierarchy;
            public int siblingIndex; public Vector3 position, localPosition, localScale;
            public Quaternion rotation, localRotation; public ComponentInfo[] components;
        }
        [Serializable] private sealed class ShipInfo
        {
            public string path, archetypeName, size, mission; public int seed, scenarioSeed, players;
            public int width, length, decks, rooms, doors, airlocks, zones, cells, crewCount;
            public bool layoutPresent, scenarioPresent, dimensionsValid, cellsMatchDimensions;
            public bool seedInAuthoredRandomRange, scenarioSeedInAuthoredRandomRange, playerAttached;
        }
        [Serializable] private sealed class BootstrapInfo
        {
            public string path, size; public int archetype, shipSeed, scenarioSeed, players;
            public bool generateOnStart, isDirected, bakedShipPresent;
        }
        [Serializable] private sealed class AssetInfo
        { public string name, type, guid, path; public long fileID; public bool hasIdentity; }
        [Serializable] private sealed class RendererInfo
        { public string path; public AssetInfo renderer, correspondingSource; public AssetInfo[] materials; }
        [Serializable] private sealed class ModificationInfo
        { public string propertyPath, value; public bool targetMissing; public AssetInfo target, objectReference; }
        [Serializable] private sealed class PrefabOracle
        {
            public string prefabPath, modelPath, prefabSHA256, modelSHA256;
            public bool prefabUnchanged, modelUnchanged, missingModelRendererAbsent;
            public long staleRendererFileID = -5824371076177060002;
            public int rendererCount, loadedModelAssetCount, modelGraphObjectCount, missingMaterialOverrideTargets;
            public AssetInfo[] loadedModelAssets, modelGraphObjects; public RendererInfo[] renderers; public ModificationInfo[] modifications;
        }
        [Serializable] private sealed class Report
        {
            public int schemaVersion = 1, pid, completedPlayTicks, unityFrame, screenWidth, screenHeight;
            public int objectCount, rendererCount, missingScriptCount, errorCount, exceptionCount;
            public string projectRoot, unityVersion, scenePath, sceneSHA256, stage, graphicsDevice, graphicsApi;
            public bool isPlaying, sourceSceneUnchanged;
            public string comparisonScope = "Authored identity/order/transforms and scenario invariants. Random generated objects are not an exact parity fixture.";
            public ObjectInfo[] objects; public ShipInfo[] ships; public BootstrapInfo[] bootstraps; public LogEntry[] logs;
        }

        static DungeonEditorReference()
        {
            Application.logMessageReceived += Log;
            EditorApplication.playModeStateChanged += PlayState;
            if (SessionState.GetString(State + "phase", "") == "playing")
                EditorApplication.delayCall += InstallTick;
        }

        // Do not pass -quit: this method exits only after the Play checkpoint.
        public static void Run()
        {
            try
            {
                string output = Required("AGENT_DUNGEON_REFERENCE_OUTPUT");
                string scene = Required("AGENT_DUNGEON_REFERENCE_SCENE");
                string expectedProject = Path.GetFullPath(Required("AGENT_DUNGEON_REFERENCE_PROJECT"));
                string project = Path.GetFullPath(Path.Combine(Application.dataPath, ".."));
                if (!string.Equals(project, expectedProject, StringComparison.Ordinal))
                    throw new InvalidOperationException("Explicit reference project does not match the open Editor.");
                if (!scene.StartsWith("Assets/", StringComparison.Ordinal) || !File.Exists(Path.Combine(project, scene)))
                    throw new InvalidOperationException("The explicit authored scene is unavailable.");
                Directory.CreateDirectory(output);
                SessionState.SetString(State + "output", Path.GetFullPath(output));
                SessionState.SetString(State + "scene", scene);
                SessionState.SetString(State + "sha", Hash(Path.Combine(project, scene)));
                SessionState.SetInt(State + "ticks", 0);
                SessionState.SetString(State + "phase", "authoring");
                WritePrefabOracle();
                EditorSceneManager.OpenScene(scene, OpenSceneMode.Single);
                Report authored = Capture("authoring");
                var paths = new List<string>();
                foreach (ObjectInfo item in authored.objects) paths.Add(item.path);
                SessionState.SetString(State + "paths", JsonUtility.ToJson(new Paths { values = paths.ToArray() }));
                Write("authoring.json", JsonUtility.ToJson(authored, true));
                WriteShared("authoring-fields.json", paths);
                SessionState.SetString(State + "phase", "playing");
                EditorApplication.EnterPlaymode();
            }
            catch (Exception exception) { Fail(exception); }
        }

        private static void PlayState(PlayModeStateChange state)
        {
            if (SessionState.GetString(State + "phase", "") != "playing") return;
            if (state == PlayModeStateChange.EnteredPlayMode) InstallTick();
        }
        private sealed class ReferenceTick { }
        private static void InstallTick()
        {
            if (!EditorApplication.isPlaying || finishing) return;
            var loop = PlayerLoop.GetCurrentPlayerLoop();
            var systems = loop.subSystemList;
            for (int index = 0; index < systems.Length; index++)
            {
                if (systems[index].type != typeof(UnityEngine.PlayerLoop.PostLateUpdate)) continue;
                var children = new List<PlayerLoopSystem>(systems[index].subSystemList ?? new PlayerLoopSystem[0]);
                children.RemoveAll(value => value.type == typeof(ReferenceTick));
                children.Add(new PlayerLoopSystem { type = typeof(ReferenceTick), updateDelegate = Tick });
                systems[index].subSystemList = children.ToArray();
                loop.subSystemList = systems;
                PlayerLoop.SetPlayerLoop(loop);
                return;
            }
            Fail(new InvalidOperationException("Unity PostLateUpdate is unavailable."));
        }
        private static void Tick()
        {
            if (finishing || !EditorApplication.isPlaying) return;
            int ticks = SessionState.GetInt(State + "ticks", 0) + 1;
            SessionState.SetInt(State + "ticks", ticks);
            if (ticks < 60) return;
            finishing = true;
            try
            {
                Write("play.json", JsonUtility.ToJson(Capture("play"), true));
                Paths paths = JsonUtility.FromJson<Paths>(SessionState.GetString(State + "paths", ""));
                WriteShared("play-authored-fields.json", new List<string>(paths.values));
                SessionState.SetString(State + "phase", "done");
                EditorApplication.delayCall += () => EditorApplication.Exit(0);
            }
            catch (Exception exception) { Fail(exception); }
        }

        private static Report Capture(string stage)
        {
            var objects = new List<ObjectInfo>(); var ships = new List<ShipInfo>(); var bootstraps = new List<BootstrapInfo>();
            int renderers = 0, missing = 0;
            foreach (GameObject root in AgentRuntimeSnapshot.LiveRoots())
                foreach (Transform transform in root.GetComponentsInChildren<Transform>(true))
                {
                    GameObject value = transform.gameObject;
                    var components = new List<ComponentInfo>();
                    foreach (Component component in value.GetComponents<Component>())
                    {
                        if (component == null) { missing++; components.Add(new ComponentInfo { type = "missing_script" }); continue; }
                        string type = component.GetType().FullName;
                        var info = new ComponentInfo { type = type };
                        var mono = component as MonoBehaviour;
                        if (mono != null)
                        {
                            MonoScript script = MonoScript.FromMonoBehaviour(mono);
                            info.scriptPath = script == null ? "" : AssetDatabase.GetAssetPath(script);
                            info.scriptGuid = string.IsNullOrEmpty(info.scriptPath) ? "" : AssetDatabase.AssetPathToGUID(info.scriptPath);
                        }
                        components.Add(info);
                        if (component is Renderer) renderers++;
                        if (type == "Dungeon.ShipGen.ShipRuntime") ships.Add(Ship(component));
                        if (type == "Dungeon.ShipGen.ShipBootstrap") bootstraps.Add(Bootstrap(component));
                    }
                    objects.Add(new ObjectInfo {
                        path = AgentRuntimeSnapshot.PathOf(transform), scene = value.scene.name,
                        activeSelf = value.activeSelf, activeInHierarchy = value.activeInHierarchy,
                        siblingIndex = transform.parent == null ? RootIndex(value) : transform.GetSiblingIndex(),
                        position = transform.position, localPosition = transform.localPosition, localScale = transform.localScale,
                        rotation = transform.rotation, localRotation = transform.localRotation, components = components.ToArray()
                    });
                }
            string project = Path.GetFullPath(Path.Combine(Application.dataPath, ".."));
            string scene = SessionState.GetString(State + "scene", "");
            string hash = Hash(Path.Combine(project, scene));
            int errors = 0, exceptions = 0;
            foreach (LogEntry entry in logs) { if (entry.type == "Error" || entry.type == "Assert") errors++; if (entry.type == "Exception") exceptions++; }
            return new Report {
                pid = System.Diagnostics.Process.GetCurrentProcess().Id, projectRoot = project, unityVersion = Application.unityVersion,
                scenePath = scene, sceneSHA256 = hash, sourceSceneUnchanged = hash == SessionState.GetString(State + "sha", ""),
                stage = stage, completedPlayTicks = SessionState.GetInt(State + "ticks", 0), unityFrame = Time.frameCount,
                isPlaying = Application.isPlaying, graphicsDevice = SystemInfo.graphicsDeviceName,
                graphicsApi = SystemInfo.graphicsDeviceType.ToString(), screenWidth = Screen.width, screenHeight = Screen.height,
                objectCount = objects.Count, rendererCount = renderers, missingScriptCount = missing,
                errorCount = errors, exceptionCount = exceptions, objects = objects.ToArray(), ships = ships.ToArray(),
                bootstraps = bootstraps.ToArray(), logs = logs.ToArray()
            };
        }
        private static BootstrapInfo Bootstrap(Component value)
        {
            return new BootstrapInfo {
                path = AgentRuntimeSnapshot.PathOf(value.transform), size = Text(Field(value, "size")),
                archetype = Int(Field(value, "archetype")), shipSeed = Int(Field(value, "shipSeed")),
                scenarioSeed = Int(Field(value, "scenarioSeed")), players = Int(Field(value, "players")),
                generateOnStart = Bool(Field(value, "generateOnStart")), isDirected = Bool(Property(value, "IsDirected")),
                bakedShipPresent = Property(value, "BakedShip") as Object != null
            };
        }
        private static ShipInfo Ship(Component value)
        {
            object layout = Field(value, "layout"), scenario = Field(value, "scenario");
            int seed = Int(Field(layout, "seed")), scenarioSeed = Int(Field(scenario, "seed"));
            int width = Int(Field(layout, "width")), length = Int(Field(layout, "length")), decks = Int(Field(layout, "decks"));
            int cells = Count(Field(layout, "cells"));
            return new ShipInfo {
                path = AgentRuntimeSnapshot.PathOf(value.transform), layoutPresent = layout != null, scenarioPresent = scenario != null,
                archetypeName = Text(Field(layout, "archetypeName")), size = Text(Field(layout, "size")), mission = Text(Field(scenario, "mission")),
                seed = seed, scenarioSeed = scenarioSeed, players = Int(Field(scenario, "players")), width = width, length = length, decks = decks,
                rooms = Count(Field(layout, "rooms")), doors = Count(Field(layout, "doors")), airlocks = Count(Field(layout, "airlocks")),
                zones = Count(Field(layout, "zones")), cells = cells, crewCount = Int(Property(value, "CrewCount")),
                playerAttached = Property(value, "Player") as Object != null,
                dimensionsValid = width > 0 && length > 0 && decks > 0,
                cellsMatchDimensions = width > 0 && length > 0 && decks > 0 && cells == width * length * decks,
                seedInAuthoredRandomRange = seed >= 1 && seed < 99999, scenarioSeedInAuthoredRandomRange = scenarioSeed >= 1 && scenarioSeed < 99999
            };
        }
        private static object Field(object value, string name)
        {
            if (value == null) return null;
            FieldInfo field = value.GetType().GetField(name, BindingFlags.Public | BindingFlags.Instance);
            return field == null ? null : field.GetValue(value);
        }
        private static object Property(object value, string name)
        {
            if (value == null) return null;
            PropertyInfo property = value.GetType().GetProperty(name, BindingFlags.Public | BindingFlags.Instance);
            return property == null || property.GetGetMethod(false) == null || property.GetIndexParameters().Length != 0 ? null : property.GetValue(value, null);
        }
        private static int Int(object value) { return value == null ? 0 : Convert.ToInt32(value); }
        private static bool Bool(object value) { return value != null && Convert.ToBoolean(value); }
        private static string Text(object value) { return value == null ? "" : value.ToString(); }
        private static int Count(object value) { return value is ICollection ? ((ICollection)value).Count : 0; }
        private static int RootIndex(GameObject value)
        {
            GameObject[] roots = value.scene.GetRootGameObjects();
            for (int index = 0; index < roots.Length; index++) if (roots[index] == value) return index;
            throw new InvalidOperationException("Root absent from public scene enumeration.");
        }
        private static void WritePrefabOracle()
        {
            const string prefabPath = "Assets/Dungeon/ShipGen/Prefabs/VB/VB_R_ShaftRim.prefab";
            const string modelPath = "Assets/Dungeon/ShipGen/Art/Custom/VB/R_ShaftRim.fbx";
            var result = new PrefabOracle {
                prefabPath = prefabPath, modelPath = modelPath,
                prefabSHA256 = Hash(prefabPath), modelSHA256 = Hash(modelPath)
            };
            var modelAssets = new List<AssetInfo>();
            foreach (Object value in AssetDatabase.LoadAllAssetsAtPath(modelPath)) modelAssets.Add(Asset(value));
            result.loadedModelAssets = modelAssets.ToArray(); result.loadedModelAssetCount = modelAssets.Count;
            var modelObjects = new List<AssetInfo>();
            GameObject model = AssetDatabase.LoadAssetAtPath<GameObject>(modelPath);
            if (model == null) throw new InvalidOperationException("Reference FBX main GameObject is unavailable.");
            result.missingModelRendererAbsent = true;
            foreach (Transform transform in model.GetComponentsInChildren<Transform>(true))
            {
                modelObjects.Add(Asset(transform.gameObject));
                foreach (Component component in transform.gameObject.GetComponents<Component>())
                {
                    AssetInfo item = Asset(component); modelObjects.Add(item);
                    if (item.fileID == result.staleRendererFileID) result.missingModelRendererAbsent = false;
                }
            }
            result.modelGraphObjects = modelObjects.ToArray(); result.modelGraphObjectCount = modelObjects.Count;
            GameObject prefabAsset = AssetDatabase.LoadAssetAtPath<GameObject>(prefabPath);
            var modifications = new List<ModificationInfo>();
            foreach (PropertyModification modification in PrefabUtility.GetPropertyModifications(prefabAsset) ?? new PropertyModification[0])
            {
                bool missing = modification.target == null;
                modifications.Add(new ModificationInfo {
                    propertyPath = modification.propertyPath, value = modification.value,
                    targetMissing = missing, target = Asset(modification.target), objectReference = Asset(modification.objectReference)
                });
                if (missing && modification.propertyPath.StartsWith("m_Materials.Array.data[", StringComparison.Ordinal))
                    result.missingMaterialOverrideTargets++;
            }
            result.modifications = modifications.ToArray();
            GameObject contents = PrefabUtility.LoadPrefabContents(prefabPath);
            try
            {
                var renderers = new List<RendererInfo>();
                foreach (MeshRenderer renderer in contents.GetComponentsInChildren<MeshRenderer>(true))
                {
                    var materials = new List<AssetInfo>();
                    foreach (Material material in renderer.sharedMaterials) materials.Add(Asset(material));
                    renderers.Add(new RendererInfo {
                        path = AgentRuntimeSnapshot.PathOf(renderer.transform), renderer = Asset(renderer),
                        correspondingSource = Asset(PrefabUtility.GetCorrespondingObjectFromSource(renderer)), materials = materials.ToArray()
                    });
                }
                result.renderers = renderers.ToArray(); result.rendererCount = renderers.Count;
            }
            finally { PrefabUtility.UnloadPrefabContents(contents); }
            result.prefabUnchanged = result.prefabSHA256 == Hash(prefabPath);
            result.modelUnchanged = result.modelSHA256 == Hash(modelPath);
            Write("shaft-rim-prefab-oracle.json", JsonUtility.ToJson(result, true));
        }
        private static AssetInfo Asset(Object value)
        {
            if (value == null) return null;
            string guid; long fileID;
            bool identity = AssetDatabase.TryGetGUIDAndLocalFileIdentifier(value, out guid, out fileID);
            return new AssetInfo { name = value.name, type = value.GetType().FullName, path = AssetDatabase.GetAssetPath(value),
                hasIdentity = identity, guid = guid, fileID = fileID };
        }
        private static void WriteShared(string name, List<string> paths)
        {
            var selected = new Dictionary<string, GameObject>(StringComparer.Ordinal);
            foreach (GameObject root in AgentRuntimeSnapshot.LiveRoots())
                foreach (Transform transform in root.GetComponentsInChildren<Transform>(true))
                {
                    string path = AgentRuntimeSnapshot.PathOf(transform);
                    if (paths.Contains(path))
                    {
                        if (selected.ContainsKey(path)) throw new InvalidOperationException("Ambiguous authored path: " + path);
                        selected[path] = transform.gameObject;
                    }
                }
            var entries = new List<string>();
            foreach (string path in paths)
                if (selected.TryGetValue(path, out GameObject value)) entries.Add(AgentRuntimeSnapshot.CaptureJson(value));
            Write(name, "{\"objects\":[" + string.Join(",", entries) + "]}");
        }
        private static void Log(string message, string stack, LogType type)
        {
            if (logs.Count < 128 && (type == LogType.Error || type == LogType.Exception || type == LogType.Assert || type == LogType.Warning))
                logs.Add(new LogEntry { type = type.ToString(), message = message, stack = stack });
        }
        private static string Required(string name)
        { string value = Environment.GetEnvironmentVariable(name); if (string.IsNullOrEmpty(value)) throw new ArgumentException("Missing " + name); return value; }
        private static string Hash(string path)
        { using (var stream = File.OpenRead(path)) using (var sha = SHA256.Create()) return BitConverter.ToString(sha.ComputeHash(stream)).Replace("-", "").ToLowerInvariant(); }
        private static void Write(string name, string json)
        { File.WriteAllText(Path.Combine(SessionState.GetString(State + "output", Required("AGENT_DUNGEON_REFERENCE_OUTPUT")), name), json); }
        private static void Fail(Exception exception)
        {
            finishing = true;
            string output = Environment.GetEnvironmentVariable("AGENT_DUNGEON_REFERENCE_OUTPUT");
            if (!string.IsNullOrEmpty(output)) { Directory.CreateDirectory(output); File.WriteAllText(Path.Combine(output, "error.txt"), exception.ToString()); }
            Debug.LogException(exception);
            EditorApplication.Exit(1);
        }
    }
}
