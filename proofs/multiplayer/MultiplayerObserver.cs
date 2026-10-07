using System;
using System.Collections.Generic;
using System.IO;
using System.Security.Cryptography;
using System.Text;
using Dungeon.Net;
using Dungeon.ShipGen;
using UnityEngine;
using UnityEngine.SceneManagement;
using UnityEngine.UIElements;

namespace UnityPlay.MultiplayerProof
{
    // An opt-in test tool. It observes public scene/network state and invokes public
    // gameplay APIs only when a command is explicitly supplied through its file.
    public sealed class MultiplayerObserver : MonoBehaviour
    {
        private string reportPath, commandPath, role;
        private NetSession session;
        private ShipRuntime ship;
        private int appliedSnapshots, sequence, commandSequence;
        private int errors, exceptions;
        private int shipRenderers, shipColliders, shipTransforms;
        private GeometryDiagnostic shipGeometry = new();
        private readonly RaycastHit[] groundHits = new RaycastHit[64];
        private string layoutHash, scenarioHash, commandAction, commandError;
        private string lastError;
        private bool commandAccepted;
        private double nextSample, shipBuiltAt = -1;
        private double phaseFirstObservedInShipAt = -1, fpsSampledAt = -1, steadyFps, steadyFpsWindow;
        private int fpsSampledFrame, steadyFpsFrames;
        private readonly List<string> errorMessages = new();
        private SceneDiagnostic[] sceneDiagnostics = Array.Empty<SceneDiagnostic>();
        private double nextDiagnostics;
        private int diagnosticsSceneCount = -1;
        private UIDocumentDiagnostic[] uiDocuments = Array.Empty<UIDocumentDiagnostic>();
        private readonly Dictionary<string, UIAssetDiagnostic> uiAssetSnapshots = new();
        private double nextUIDiagnostics;
        private string uiDiagnosticsError;

        public static void __UnityPlay_RuntimeInitialize_MultiplayerProof()
        {
            string path = Environment.GetEnvironmentVariable("UNITY_PLAY_MULTIPLAYER_REPORT");
            if (string.IsNullOrEmpty(path)) return;
            if (!Path.IsPathRooted(path)) throw new InvalidDataException("Multiplayer report path must be absolute.");
            var root = new GameObject("UnityPlay Multiplayer Proof");
            DontDestroyOnLoad(root);
            var observer = root.AddComponent<MultiplayerObserver>();
            observer.reportPath = path;
            observer.commandPath = Environment.GetEnvironmentVariable("UNITY_PLAY_MULTIPLAYER_COMMAND");
            observer.role = Environment.GetEnvironmentVariable("UNITY_PLAY_MULTIPLAYER_ROLE") ?? "peer";
            if (!string.IsNullOrEmpty(observer.commandPath) && !Path.IsPathRooted(observer.commandPath))
                throw new InvalidDataException("Multiplayer command path must be absolute.");
            Application.logMessageReceived += observer.OnLog;
            observer.Publish();
        }

        private void OnDestroy()
        {
            Application.logMessageReceived -= OnLog;
            if (ship != null) ship.StateApplied -= OnStateApplied;
        }

        private void OnLog(string message, string stackTrace, LogType type)
        {
            if (type == LogType.Error || type == LogType.Assert) errors++;
            else if (type == LogType.Exception) exceptions++;
            else return;
            lastError = message;
            if (errorMessages.Count < 16) errorMessages.Add(message);
        }

        private void OnStateApplied() => appliedSnapshots++;

        private void Update()
        {
            double now = Time.realtimeSinceStartupAsDouble;
            if (now < nextSample) return;
            nextSample = now + .1;
            try
            {
                Discover();
                RunCommand();
                Publish();
            }
            catch (Exception error)
            {
                commandError = error.ToString();
                Publish();
            }
        }

        private void Discover()
        {
            ShipRuntime candidate = ship;
            foreach (Scene scene in ObservableScenes())
            {
                if (!scene.isLoaded) continue;
                foreach (GameObject root in scene.GetRootGameObjects())
                {
                    if (session == null)
                    {
                        NetSession[] sessions = root.GetComponentsInChildren<NetSession>(true);
                        if (sessions.Length > 0) session = sessions[0];
                    }
                    foreach (ShipRuntime runtime in root.GetComponentsInChildren<ShipRuntime>(true))
                        if (runtime != null && runtime.gameObject.activeInHierarchy && runtime.layout != null && runtime.scenario != null)
                            candidate = runtime;
                }
            }
            if (sceneDiagnostics.Length == 0 || diagnosticsSceneCount != SceneManager.sceneCount
                || session == null && Time.realtimeSinceStartupAsDouble >= nextDiagnostics)
                CaptureSceneDiagnostics();
            if (Time.realtimeSinceStartupAsDouble >= nextUIDiagnostics) CaptureUIDiagnostics();
            if (candidate == ship) return;
            if (ship != null) ship.StateApplied -= OnStateApplied;
            ship = candidate;
            appliedSnapshots = 0;
            layoutHash = scenarioHash = null;
            if (ship == null) return;
            ship.StateApplied += OnStateApplied;
            shipBuiltAt = Time.realtimeSinceStartupAsDouble;
            layoutHash = LayoutHash(ship.layout);
            scenarioHash = SHA(Encoding.UTF8.GetBytes(JsonUtility.ToJson(ship.scenario)));
            shipRenderers = ship.GetComponentsInChildren<Renderer>(true).Length;
            shipColliders = ship.GetComponentsInChildren<Collider>(true).Length;
            shipTransforms = ship.GetComponentsInChildren<Transform>(true).Length;
            shipGeometry = Geometry(ship.gameObject);
        }

        private IEnumerable<Scene> ObservableScenes()
        {
            Scene persistent = gameObject.scene;
            bool persistentListed = false;
            for (int index = 0; index < SceneManager.sceneCount; index++)
            {
                Scene scene = SceneManager.GetSceneAt(index);
                if (scene.handle == persistent.handle) persistentListed = true;
                yield return scene;
            }
            // NetworkManager moves its whole root into this scene in OnEnable.
            // Unity excludes DontDestroyOnLoad from SceneManager.sceneCount, but
            // this observer is itself persistent and can access the public Scene.
            if (!persistentListed && persistent.IsValid() && persistent.isLoaded) yield return persistent;
        }

        private void CaptureSceneDiagnostics()
        {
            var scenes = new List<SceneDiagnostic>();
            foreach (Scene scene in ObservableScenes())
            {
                var item = new SceneDiagnostic
                {
                    name = scene.name, path = scene.path, handle = scene.handle.GetRawData().ToString(),
                    loaded = scene.isLoaded, observerScene = scene.handle == gameObject.scene.handle,
                };
                var roots = new List<RootDiagnostic>();
                if (scene.isLoaded)
                    foreach (GameObject root in scene.GetRootGameObjects())
                    {
                        var entry = new RootDiagnostic
                        {
                            name = root.name, entityId = root.GetEntityId().ToString(), activeSelf = root.activeSelf,
                            activeInHierarchy = root.activeInHierarchy,
                        };
                        var behaviours = new List<BehaviourDiagnostic>();
                        foreach (MonoBehaviour component in root.GetComponentsInChildren<MonoBehaviour>(true))
                        {
                            if (component == null) { entry.missingBehaviours++; continue; }
                            if (behaviours.Count == 256) { entry.truncatedBehaviours = true; break; }
                            Type type = component.GetType();
                            behaviours.Add(new BehaviourDiagnostic
                            {
                                gameObjectName = component.gameObject.name, entityId = component.GetEntityId().ToString(),
                                type = type.FullName, assembly = type.Assembly.FullName,
                                moduleVersionId = type.Assembly.ManifestModule.ModuleVersionId.ToString(),
                                enabled = component.enabled, activeInHierarchy = component.gameObject.activeInHierarchy,
                                assignableToNetSession = component is NetSession,
                            });
                        }
                        entry.behaviours = behaviours.ToArray(); roots.Add(entry);
                    }
                item.roots = roots.ToArray(); scenes.Add(item);
            }
            sceneDiagnostics = scenes.ToArray();
            diagnosticsSceneCount = SceneManager.sceneCount;
            nextDiagnostics = Time.realtimeSinceStartupAsDouble + 1;
        }

        private void CaptureUIDiagnostics()
        {
            nextUIDiagnostics = Time.realtimeSinceStartupAsDouble + 1;
            uiDiagnosticsError = null;
            var documents = new List<UIDocumentDiagnostic>();
            try
            {
                foreach (Scene scene in ObservableScenes())
                {
                    if (!scene.isLoaded) continue;
                    foreach (GameObject root in scene.GetRootGameObjects())
                        foreach (UIDocument document in root.GetComponentsInChildren<UIDocument>(true))
                        {
                            if (document == null || !document.enabled || !document.gameObject.activeInHierarchy) continue;
                            var diagnostic = new UIDocumentDiagnostic
                            {
                                name = document.name, entityId = document.GetEntityId().ToString(), scene = scene.name,
                                enabled = document.enabled, activeInHierarchy = document.gameObject.activeInHierarchy,
                            };
                            documents.Add(diagnostic);
                            try
                            {
                                VisualTreeAsset tree = document.visualTreeAsset;
                                diagnostic.visualTreeAsset = CaptureUIAsset(tree);
                                var sourceSheets = new List<UIAssetDiagnostic>();
                                if (tree != null)
                                    foreach (StyleSheet sheet in tree.stylesheets) sourceSheets.Add(CaptureUIAsset(sheet));
                                diagnostic.sourceStyleSheets = sourceSheets.ToArray();
                                VisualElement elementRoot = document.rootVisualElement;
                                diagnostic.rootPresent = elementRoot != null;
                                if (elementRoot == null) continue;
                                diagnostic.rootPanelAttached = elementRoot.panel != null;
                                diagnostic.rootStyleSheets = CaptureStyleSheets(elementRoot);
                                var elements = new List<UIElementDiagnostic>();
                                var pending = new Stack<(VisualElement element, string path)>();
                                pending.Push((elementRoot, "root"));
                                while (pending.Count != 0)
                                {
                                    if (diagnostic.elementCount >= 4096) { diagnostic.truncated = true; break; }
                                    var (element, path) = pending.Pop();
                                    diagnostic.elementCount++;
                                    var classes = new List<string>();
                                    foreach (string item in element.GetClasses()) classes.Add(item);
                                    bool hidden = element.ClassListContains("hidden");
                                    if (hidden) diagnostic.hiddenClassElements++;
                                    // Named panels, classed controls, and sheet owners reveal both
                                    // source style attachment and the computed effect of .hidden.
                                    if (element == elementRoot || !string.IsNullOrEmpty(element.name)
                                        || classes.Count > 0 || element.styleSheets.count > 0)
                                        elements.Add(new UIElementDiagnostic
                                        {
                                            path = path, name = element.name, type = element.GetType().FullName,
                                            classes = classes.ToArray(), hiddenClass = hidden,
                                            resolvedDisplay = element.resolvedStyle.display.ToString(),
                                            inlineDisplay = element.style.display.value.ToString(),
                                            inlineDisplayKeyword = element.style.display.keyword.ToString(),
                                            visible = element.visible, panelAttached = element.panel != null,
                                            layout = element.layout, styleSheets = CaptureStyleSheets(element),
                                        });
                                    for (int index = element.hierarchy.childCount - 1; index >= 0; index--)
                                        pending.Push((element.hierarchy[index], path + "/" + index));
                                }
                                diagnostic.elements = elements.ToArray();
                            }
                            catch (Exception error) { diagnostic.error = error.ToString(); }
                        }
                }
            }
            catch (Exception error) { uiDiagnosticsError = error.ToString(); }
            uiDocuments = documents.ToArray();
        }

        private UIAssetDiagnostic[] CaptureStyleSheets(VisualElement element)
        {
            var sheets = new UIAssetDiagnostic[element.styleSheets.count];
            for (int index = 0; index < sheets.Length; index++) sheets[index] = CaptureUIAsset(element.styleSheets[index]);
            return sheets;
        }

        private UIAssetDiagnostic CaptureUIAsset(UnityEngine.Object asset)
        {
            if (asset == null) return new UIAssetDiagnostic { isNull = true };
            string id = asset.GetEntityId().ToString();
            string key = asset.GetType().FullName + ":" + id;
            if (uiAssetSnapshots.TryGetValue(key, out UIAssetDiagnostic existing)) return existing;
            var snapshot = new UIAssetDiagnostic { name = asset.name, entityId = id, type = asset.GetType().FullName };
            // Snapshot each native object once. The small reference DTO is reused in
            // subsequent reports; native rules/properties stay in ignored files.
            uiAssetSnapshots.Add(key, snapshot);
            try
            {
                if (asset is StyleSheet sheet)
                {
                    snapshot.contentHash = sheet.contentHash;
                    snapshot.importedWithErrors = sheet.importedWithErrors;
                    snapshot.importedWithWarnings = sheet.importedWithWarnings;
                }
                else if (asset is VisualTreeAsset tree)
                {
                    snapshot.contentHash = tree.contentHash;
                    snapshot.importedWithErrors = tree.importedWithErrors;
                    snapshot.importedWithWarnings = tree.importedWithWarnings;
                }
                string directory = Path.Combine(Path.GetDirectoryName(reportPath), Path.GetFileNameWithoutExtension(reportPath) + "-ui-assets");
                Directory.CreateDirectory(directory);
                snapshot.jsonFile = Path.Combine(directory, SHA(Encoding.UTF8.GetBytes(key)) + ".json");
                string json = JsonUtility.ToJson(asset, true);
                File.WriteAllText(snapshot.jsonFile, json);
                snapshot.jsonSHA256 = SHA(Encoding.UTF8.GetBytes(json));
            }
            catch (Exception error) { snapshot.error = error.ToString(); }
            return snapshot;
        }

        private void RunCommand()
        {
            if (string.IsNullOrEmpty(commandPath) || !File.Exists(commandPath)) return;
            var command = JsonUtility.FromJson<Command>(File.ReadAllText(commandPath));
            if (command == null || command.sequence <= commandSequence) return;
            commandSequence = command.sequence;
            commandAction = command.action;
            commandError = null;
            commandAccepted = false;
            if (session == null || ship == null || session.Current != NetSession.State.InShip)
                throw new InvalidOperationException("Command requires a built multiplayer ship.");
            switch (command.action)
            {
                case "toggle-zone-power":
                    if (!session.IsServer) throw new InvalidOperationException("Only the host can change sector power.");
                    ZoneRuntimeState zone = ship.ZoneState(command.zone);
                    if (zone == null) throw new InvalidDataException("Unknown zone.");
                    ship.SetPower(command.zone, !zone.powered);
                    break;
                case "use-door":
                    ShipDoor door = ship.Door(command.door);
                    NetworkPlayer local = LocalPlayer();
                    if (door == null || local == null || local.Player == null || local.Player.Interactor == null)
                        throw new InvalidDataException("Door or local player is unavailable.");
                    bool canUse = door.CanUse(local.Player.Interactor, out string reason);
                    if (!door.gameObject.activeInHierarchy || !canUse)
                        throw new InvalidOperationException("Door cannot be used: " + reason);
                    if (string.IsNullOrEmpty(door.Prompt(local.Player.Interactor)))
                        throw new InvalidOperationException("Door has no usable prompt.");
                    ship.Commands.Use(door, local.Player.Interactor);
                    break;
                case "start-bot":
                    NetworkPlayer botPlayer = LocalPlayer();
                    if (botPlayer == null) throw new InvalidOperationException("Local avatar is unavailable.");
                    botPlayer.StartBot();
                    break;
                case "stop-bot":
                    NetworkPlayer stoppedPlayer = LocalPlayer();
                    if (stoppedPlayer == null || !stoppedPlayer.TryGetComponent(out PlayerBot bot))
                        throw new InvalidOperationException("Local bot is unavailable.");
                    bot.enabled = false;
                    break;
                default: throw new InvalidDataException("Unknown multiplayer command: " + command.action);
            }
            commandAccepted = true;
        }

        private NetworkPlayer LocalPlayer()
        {
            if (session == null) return null;
            foreach (NetworkPlayer player in session.Players.Values)
                if (player != null && player.IsOwner) return player;
            return null;
        }

        private void Publish()
        {
            if (string.IsNullOrEmpty(reportPath)) return;
            double now = Time.realtimeSinceStartupAsDouble;
            if (session != null && session.Current == NetSession.State.InShip)
            {
                if (phaseFirstObservedInShipAt < 0) phaseFirstObservedInShipAt = now;
                if (fpsSampledAt < 0 || now - phaseFirstObservedInShipAt < 2)
                { fpsSampledAt = now; fpsSampledFrame = Time.frameCount; }
                else if (now - fpsSampledAt >= 1)
                {
                    steadyFpsWindow = now - fpsSampledAt;
                    steadyFpsFrames = Time.frameCount - fpsSampledFrame;
                    steadyFps = steadyFpsFrames / steadyFpsWindow;
                    fpsSampledAt = now; fpsSampledFrame = Time.frameCount;
                }
            }
            else
            { phaseFirstObservedInShipAt = fpsSampledAt = -1; steadyFps = steadyFpsWindow = 0; steadyFpsFrames = 0; }
            var report = new Report
            {
                schema = 1, role = role, pid = System.Diagnostics.Process.GetCurrentProcess().Id,
                sequence = ++sequence, observedAtSeconds = now,
                phaseFirstObservedInShipAtSeconds = phaseFirstObservedInShipAt, steadyFramesPerSecond = steadyFps,
                steadyFpsWindowSeconds = steadyFpsWindow, steadyFpsFrames = steadyFpsFrames,
                sceneCount = SceneManager.sceneCount, errors = errors, exceptions = exceptions,
                lastError = lastError, errorMessages = errorMessages.ToArray(), commandSequence = commandSequence,
                commandAction = commandAction, commandAccepted = commandAccepted, commandError = commandError,
                shipBuiltAtSeconds = shipBuiltAt, shipStateApplications = appliedSnapshots,
                expectedSessionAssembly = typeof(NetSession).Assembly.FullName,
                expectedSessionModuleVersionId = typeof(NetSession).Assembly.ManifestModule.ModuleVersionId.ToString(),
                observedScenes = sceneDiagnostics,
                uiDocuments = uiDocuments, uiDiagnosticsError = uiDiagnosticsError,
            };
            var sceneNames = new List<string>();
            for (int index = 0; index < SceneManager.sceneCount; index++) sceneNames.Add(SceneManager.GetSceneAt(index).name);
            report.sceneNames = sceneNames.ToArray();
            if (session != null)
            {
                report.sessionFound = true;
                report.current = session.Current.ToString();
                report.status = session.Status;
                report.sessionError = session.Error;
                report.contentReady = session.ContentReady;
                report.roomCode = session.RoomCode;
                report.isServer = session.IsServer;
                report.localClientId = session.LocalClientId;
                report.generation = session.Generation;
                var members = new List<Member>();
                var players = new List<Player>();
                if (session.Network != null)
                {
                    report.phase = session.Network.Phase.Value.ToString();
                    report.sortie = SortieSnapshot.From(session.Network.Sortie.Value);
                    var manager = session.Network.NetworkManager;
                    report.listening = manager.IsListening;
                    report.connectedClient = manager.IsConnectedClient;
                    var ids = new List<ulong>();
                    foreach (ulong id in manager.ConnectedClientsIds) ids.Add(id);
                    ids.Sort();
                    report.connectedClientIds = ids.ToArray();
                    foreach (LobbyMember member in session.Network.Members)
                        members.Add(new Member { clientId = member.clientId, name = member.name.ToString(), slot = member.slot, inShip = member.inShip, down = member.down });
                }
                foreach (NetworkPlayer player in session.Players.Values)
                {
                    if (player == null) continue;
                    var avatar = new Player
                    {
                        ownerClientId = player.OwnerClientId, networkObjectId = player.NetworkObjectId,
                        isOwner = player.IsOwner, spawned = player.IsSpawned, displayName = player.DisplayName.Value.ToString(),
                        position = player.transform.position, yaw = player.transform.eulerAngles.y,
                        room = ship != null ? ship.RoomAt(player.transform.position) : -1,
                        alive = player.Player != null && player.Player.Vitals != null && player.Player.Vitals.Alive,
                        description = player.Describe(),
                        geometry = Geometry(player.gameObject),
                    };
                    if (ship != null)
                    {
                        int hits = Physics.RaycastNonAlloc(player.transform.position + Vector3.up * .35f,
                            Vector3.down, groundHits, 2.35f, ~0, QueryTriggerInteraction.Ignore);
                        float closest = float.PositiveInfinity;
                        for (int index = 0; index < hits; index++)
                        {
                            RaycastHit hit = groundHits[index];
                            if (hit.collider == null || !hit.collider.transform.IsChildOf(ship.transform)
                                || hit.normal.y < .5f || hit.distance >= closest) continue;
                            closest = hit.distance;
                            avatar.shipFloorSupported = true;
                            avatar.shipFloorCollider = hit.collider.name;
                            avatar.shipFloorDistance = hit.distance;
                        }
                    }
                    players.Add(avatar);
                }
                members.Sort((a, b) => a.clientId.CompareTo(b.clientId));
                players.Sort((a, b) => a.ownerClientId.CompareTo(b.ownerClientId));
                report.members = members.ToArray();
                report.players = players.ToArray();
            }
            if (ship != null && ship.layout != null && ship.scenario != null)
            {
                report.shipFound = true;
                report.shipName = ship.name;
                report.shipAuthority = ship.IsAuthority;
                report.shipSeed = ship.layout.seed;
                report.scenarioSeed = ship.scenario.seed;
                report.layoutSHA256 = layoutHash;
                report.scenarioSHA256 = scenarioHash;
                report.rooms = ship.layout.rooms.Count;
                report.zones = ship.layout.zones.Count;
                report.width = ship.layout.width;
                report.length = ship.layout.length;
                report.decks = ship.layout.decks;
                report.cells = ship.layout.cells.Length;
                report.crewCount = ship.CrewCount;
                report.interactables = ship.Interactables.Count;
                report.shipRenderers = shipRenderers;
                report.shipColliders = shipColliders;
                report.shipTransforms = shipTransforms;
                report.shipGeometry = shipGeometry;
                report.missionState = ship.Mission != null ? ship.Mission.state.ToString() : null;
                report.zonePower = new bool[ship.zones.Length];
                for (int index = 0; index < ship.zones.Length; index++) report.zonePower[index] = ship.zones[index].powered;
                NetworkPlayer local = LocalPlayer();
                var doors = new List<Door>();
                foreach (DoorInstance instance in ship.layout.doors)
                {
                    ShipDoor door = ship.Door(instance.id);
                    if (door == null) continue;
                    var item = new Door
                    {
                        doorId = door.doorId, kind = door.kind.ToString(), syncState = door.SyncState,
                        openAmount = door.OpenAmount, position = door.transform.position,
                        powered = ship.DoorPowered(door.doorId), locked = ship.IsLocked(door.doorId), jammed = ship.IsJammed(door.doorId),
                        externalControl = door.externalControl, interactableIndex = ship.IndexOf(door),
                        autoCloseDistance = door.autoCloseDistance, nearestCrewDistance = ship.NearestCrewDistance(door.transform.position),
                    };
                    if (local != null && local.Player != null && local.Player.Interactor != null)
                    {
                        item.canUseLocal = door.CanUse(local.Player.Interactor, out string reason);
                        item.refusedReason = reason;
                        item.prompt = door.Prompt(local.Player.Interactor);
                        item.holdTime = door.HoldTime(local.Player.Interactor);
                    }
                    doors.Add(item);
                }
                doors.Sort((a, b) => a.doorId.CompareTo(b.doorId));
                report.doors = doors.ToArray();
            }
            report.ready = report.sessionFound && report.current == "InShip" && report.shipFound
                && report.members.Length == 2 && report.players.Length == 2 && report.rooms > 0
                && GeometryReady(report.shipGeometry)
                && report.errors == 0 && report.exceptions == 0 && string.IsNullOrEmpty(report.sessionError);
            foreach (Member member in report.members) report.ready &= member.inShip && !member.down;
            foreach (Player player in report.players)
                report.ready &= player.spawned && player.alive && GeometryReady(player.geometry) && player.shipFloorSupported;
            Directory.CreateDirectory(Path.GetDirectoryName(reportPath));
            string temporary = reportPath + ".tmp";
            File.WriteAllText(temporary, JsonUtility.ToJson(report, true));
            if (File.Exists(reportPath)) File.Replace(temporary, reportPath, null);
            else File.Move(temporary, reportPath);
        }

        private static bool GeometryReady(GeometryDiagnostic geometry) => geometry != null
            && geometry.meshRenderers > 0 && geometry.vertices > 0 && geometry.indices > 0
            && geometry.supportedShaderRenderers > 0 && geometry.activeSolidColliders > 0;

        // Readback of instantiated runtime components, independent of layout data.
        // Shadow-only first-person meshes count as geometry. Visibility in a camera
        // is intentionally left to the screenshot/render validation.
        private static GeometryDiagnostic Geometry(GameObject root)
        {
            var evidence = new GeometryDiagnostic();
            foreach (Renderer renderer in root.GetComponentsInChildren<Renderer>(true))
            {
                if (!renderer.enabled || !renderer.gameObject.activeInHierarchy) continue;
                Mesh mesh = null;
                if (renderer is SkinnedMeshRenderer skinned) mesh = skinned.sharedMesh;
                else if (renderer is MeshRenderer && renderer.TryGetComponent(out MeshFilter filter)) mesh = filter.sharedMesh;
                if (mesh == null || mesh.vertexCount == 0 || mesh.subMeshCount == 0) continue;
                long indices = 0;
                for (int index = 0; index < mesh.subMeshCount; index++) indices += mesh.GetIndexCount(index);
                if (indices == 0 || !ValidBounds(renderer.bounds)) continue;
                evidence.meshRenderers++;
                evidence.vertices += mesh.vertexCount;
                evidence.indices += indices;
                bool supported = false;
                foreach (Material material in renderer.sharedMaterials)
                    if (material != null && material.shader != null && material.shader.isSupported) supported = true;
                if (supported) evidence.supportedShaderRenderers++;
                else evidence.unsupportedShaderRenderers++;
                if (renderer is SkinnedMeshRenderer skin)
                {
                    evidence.skinnedMeshRenderers++;
                    Transform[] bones = skin.bones;
                    bool bound = bones.Length > 0 && mesh.bindposeCount == bones.Length;
                    foreach (Transform bone in bones) bound &= bone != null;
                    if (bound) evidence.boundSkinnedMeshRenderers++;
                }
            }
            foreach (Collider collider in root.GetComponentsInChildren<Collider>(true))
            {
                if (!collider.enabled || !collider.gameObject.activeInHierarchy || collider.isTrigger
                    || !ValidBounds(collider.bounds)) continue;
                if (collider is MeshCollider meshCollider && (meshCollider.sharedMesh == null
                    || meshCollider.sharedMesh.vertexCount == 0)) continue;
                evidence.activeSolidColliders++;
            }
            return evidence;
        }

        private static bool ValidBounds(Bounds bounds) => Finite(bounds.center) && Finite(bounds.size)
            && bounds.size.sqrMagnitude > .000001f;
        private static bool Finite(Vector3 value) => !float.IsNaN(value.x) && !float.IsInfinity(value.x)
            && !float.IsNaN(value.y) && !float.IsInfinity(value.y) && !float.IsNaN(value.z) && !float.IsInfinity(value.z);

        private static string LayoutHash(ShipLayout layout)
        {
            using var stream = new MemoryStream();
            using var writer = new BinaryWriter(stream);
            writer.Write(1);
            writer.Write(layout.seed); writer.Write(layout.archetypeName ?? "");
            writer.Write((int)layout.size); writer.Write((int)layout.circulation);
            writer.Write(layout.width); writer.Write(layout.length); writer.Write(layout.decks);
            writer.Write(layout.cellSize); writer.Write(layout.deckHeight);
            writer.Write(layout.cells.Length);
            foreach (CellData cell in layout.cells)
            {
                writer.Write((int)cell.kind); writer.Write(cell.room); writer.Write(cell.zone); writer.Write(cell.band);
            }
            writer.Write(layout.rooms.Count);
            foreach (RoomInstance room in layout.rooms)
            {
                writer.Write(room.id); writer.Write(room.zone); writer.Write(room.deck); writer.Write(room.specId ?? "");
                writer.Write((int)room.tags); writer.Write((int)room.role); writer.Write((int)room.system);
                writer.Write(room.displayName ?? ""); writer.Write(room.number ?? "");
                writer.Write(room.deadEndAllowed); writer.Write(room.glassDoor); writer.Write(room.canopy);
                writer.Write(room.rect.x); writer.Write(room.rect.y); writer.Write(room.rect.width); writer.Write(room.rect.height);
                WriteCells(writer, room.cells); WriteInts(writer, room.doors); writer.Write(room.mirrorOf);
            }
            writer.Write(layout.zones.Count);
            foreach (ZoneInstance zone in layout.zones)
            {
                writer.Write(zone.id); writer.Write(zone.name ?? ""); writer.Write(zone.label ?? ""); writer.Write(zone.code ?? "");
                writer.Write((int)zone.transit); writer.Write(zone.deck); writer.Write((int)zone.lateral);
                writer.Write(zone.side); writer.Write(zone.band); writer.Write(zone.isTransit); writer.Write(zone.isService);
                writer.Write(zone.twin); writer.Write(zone.mirrorOf); WriteInts(writer, zone.rooms); WriteCells(writer, zone.cells);
            }
            writer.Write(layout.doors.Count);
            foreach (DoorInstance door in layout.doors)
            {
                writer.Write(door.id); writer.Write(door.deck); writer.Write(door.cell.x); writer.Write(door.cell.y);
                writer.Write((int)door.dir); writer.Write(door.roomA); writer.Write(door.roomB); writer.Write((int)door.kind);
            }
            writer.Write(layout.airlocks.Count);
            foreach (AirlockInstance airlock in layout.airlocks)
            {
                writer.Write(airlock.id); writer.Write((int)airlock.kind); writer.Write(airlock.door);
                writer.Write(airlock.room); writer.Write(airlock.deck); writer.Write(airlock.name ?? "");
            }
            writer.Write(layout.verticals.Count);
            foreach (VerticalLink vertical in layout.verticals)
            {
                writer.Write(vertical.id); writer.Write((int)vertical.kind); writer.Write(vertical.lowerDeck);
                writer.Write(vertical.lowerRoom); writer.Write(vertical.upperRoom);
                writer.Write(vertical.cell.x); writer.Write(vertical.cell.y); writer.Write((int)vertical.dir);
            }
            writer.Write(layout.bands.Count);
            foreach (BandInstance band in layout.bands)
            {
                writer.Write(band.id); writer.Write(band.deck); writer.Write(band.zStart); writer.Write(band.rows);
                writer.Write(band.leftZone); writer.Write(band.rightZone); writer.Write(band.centerZone); writer.Write(band.fullZone);
                writer.Write(band.transverseBefore);
            }
            writer.Flush();
            return SHA(stream.ToArray());
        }

        private static void WriteCells(BinaryWriter writer, List<Vector2Int> cells)
        {
            writer.Write(cells.Count);
            foreach (Vector2Int cell in cells) { writer.Write(cell.x); writer.Write(cell.y); }
        }

        private static void WriteInts(BinaryWriter writer, List<int> values)
        {
            writer.Write(values.Count);
            foreach (int value in values) writer.Write(value);
        }

        private static string SHA(byte[] data)
        {
            using SHA256 hash = SHA256.Create();
            return BitConverter.ToString(hash.ComputeHash(data)).Replace("-", "").ToLowerInvariant();
        }

        [Serializable] private sealed class Command
        {
            public int sequence;
            public string action;
            public int zone;
            public int door;
        }

        [Serializable] private sealed class Report
        {
            public int schema, pid, sequence, sceneCount, generation, errors, exceptions, shipStateApplications, commandSequence;
            public string role, current, status, sessionError, roomCode, phase, lastError, shipName, layoutSHA256, scenarioSHA256;
            public string missionState, commandAction, commandError;
            public string expectedSessionAssembly, expectedSessionModuleVersionId;
            public double observedAtSeconds, shipBuiltAtSeconds;
            public double phaseFirstObservedInShipAtSeconds, steadyFramesPerSecond, steadyFpsWindowSeconds;
            public int steadyFpsFrames;
            public bool ready, sessionFound, contentReady, isServer, listening, connectedClient, shipFound, shipAuthority, commandAccepted;
            public ulong localClientId;
            public ulong[] connectedClientIds = Array.Empty<ulong>();
            public string[] sceneNames = Array.Empty<string>(), errorMessages = Array.Empty<string>();
            public SortieSnapshot sortie = new();
            public Member[] members = Array.Empty<Member>();
            public Player[] players = Array.Empty<Player>();
            public Door[] doors = Array.Empty<Door>();
            public SceneDiagnostic[] observedScenes = Array.Empty<SceneDiagnostic>();
            public UIDocumentDiagnostic[] uiDocuments = Array.Empty<UIDocumentDiagnostic>();
            public string uiDiagnosticsError;
            public bool[] zonePower = Array.Empty<bool>();
            public int shipSeed, scenarioSeed, rooms, zones, width, length, decks, cells, crewCount, interactables;
            public int shipRenderers, shipColliders, shipTransforms;
            public GeometryDiagnostic shipGeometry = new();
        }

        // NGO's SortieState is a network struct without the Serializable marker.
        // This separate report contract preserves every public network field.
        [Serializable] public sealed class SortieSnapshot
        {
            public int archetype, size, shipSeed, scenarioSeed, crew, generation;
            public static SortieSnapshot From(SortieState state) => new SortieSnapshot
            {
                archetype = state.archetype, size = state.size, shipSeed = state.shipSeed,
                scenarioSeed = state.scenarioSeed, crew = state.crew, generation = state.generation,
            };
        }

        [Serializable] private sealed class UIDocumentDiagnostic
        {
            public string name, entityId, scene, error;
            public bool enabled, activeInHierarchy, rootPresent, rootPanelAttached, truncated;
            public int elementCount, hiddenClassElements;
            public UIAssetDiagnostic visualTreeAsset = new();
            public UIAssetDiagnostic[] sourceStyleSheets = Array.Empty<UIAssetDiagnostic>();
            public UIAssetDiagnostic[] rootStyleSheets = Array.Empty<UIAssetDiagnostic>();
            public UIElementDiagnostic[] elements = Array.Empty<UIElementDiagnostic>();
        }

        [Serializable] private sealed class UIAssetDiagnostic
        {
            public string name, entityId, type, jsonFile, jsonSHA256, error;
            public bool isNull, importedWithErrors, importedWithWarnings;
            public int contentHash;
        }

        [Serializable] private sealed class UIElementDiagnostic
        {
            public string path, name, type, resolvedDisplay, inlineDisplay, inlineDisplayKeyword;
            public string[] classes = Array.Empty<string>();
            public bool hiddenClass, visible, panelAttached;
            public Rect layout;
            public UIAssetDiagnostic[] styleSheets = Array.Empty<UIAssetDiagnostic>();
        }

        [Serializable] private sealed class SceneDiagnostic
        {
            public string name, path, handle;
            public bool loaded, observerScene;
            public RootDiagnostic[] roots = Array.Empty<RootDiagnostic>();
        }

        [Serializable] private sealed class RootDiagnostic
        {
            public string name, entityId;
            public int missingBehaviours;
            public bool activeSelf, activeInHierarchy, truncatedBehaviours;
            public BehaviourDiagnostic[] behaviours = Array.Empty<BehaviourDiagnostic>();
        }

        [Serializable] private sealed class BehaviourDiagnostic
        {
            public string gameObjectName, type, assembly, moduleVersionId, entityId;
            public bool enabled, activeInHierarchy, assignableToNetSession;
        }

        [Serializable] private sealed class Member
        {
            public ulong clientId;
            public string name;
            public int slot;
            public bool inShip, down;
        }

        [Serializable] private sealed class Player
        {
            public ulong ownerClientId, networkObjectId;
            public string displayName, description;
            public bool isOwner, spawned, alive;
            public Vector3 position;
            public float yaw;
            public int room;
            public GeometryDiagnostic geometry = new();
            public bool shipFloorSupported;
            public string shipFloorCollider;
            public float shipFloorDistance;
        }

        [Serializable] private sealed class GeometryDiagnostic
        {
            public int meshRenderers, supportedShaderRenderers, unsupportedShaderRenderers, activeSolidColliders;
            public int skinnedMeshRenderers, boundSkinnedMeshRenderers;
            public long vertices, indices;
        }

        [Serializable] private sealed class Door
        {
            public int doorId, syncState, interactableIndex;
            public string kind, prompt, refusedReason;
            public bool powered, locked, jammed, externalControl, canUseLocal;
            public float openAmount, holdTime, autoCloseDistance, nearestCrewDistance;
            public Vector3 position;
        }
    }

}
