// Validation-only attributed tool. Create this in an explicitly owned Dungeon
// project through script_create; do not attach it to an authored scene.
using System.Collections.Generic;
using Dungeon.CLIPlay;
using Dungeon.ShipGen;
using UnityEngine;

namespace AgentParity.Validation
{
    public static class DungeonAgentReadback
    {
        [AgentTool("dungeon.reference", Description = "Read Dungeon public ship/layout/scenario/player state for runtime validation.", ReadOnly = true)]
        public static DungeonReadbackResult Read()
        {
            var result = new DungeonReadbackResult {
                frame = Time.frameCount, unityVersion = Application.unityVersion,
                graphicsDevice = SystemInfo.graphicsDeviceName, graphicsApi = SystemInfo.graphicsDeviceType.ToString()
            };
            var roots = new List<DungeonRootReadback>();
            var ships = new List<DungeonShipReadback>();
            var bootstraps = new List<DungeonBootstrapReadback>();
            foreach (GameObject root in AgentRuntimeSnapshot.LiveRoots())
            {
                roots.Add(new DungeonRootReadback { path = AgentRuntimeSnapshot.PathOf(root.transform), scene = root.scene.name, activeSelf = root.activeSelf });
                foreach (Transform transform in root.GetComponentsInChildren<Transform>(true))
                {
                    result.objectCount++;
                    foreach (Component component in transform.gameObject.GetComponents<Component>())
                    {
                        if (component == null) { result.missingScriptCount++; continue; }
                        var runtime = component as ShipRuntime;
                        var bootstrap = component as ShipBootstrap;
                        if (runtime != null) ships.Add(Ship(runtime));
                        if (bootstrap != null) bootstraps.Add(Bootstrap(bootstrap));
                        var renderer = component as Renderer;
                        if (renderer != null)
                        {
                            result.rendererCount++;
                            foreach (Material material in renderer.sharedMaterials)
                            {
                                if (material == null) { result.missingMaterialSlots++; continue; }
                                if (material.shader == null) result.missingShaders++;
                                else if (!material.shader.isSupported) result.unsupportedShaders++;
                            }
                        }
                    }
                }
            }
            result.roots = roots.ToArray(); result.ships = ships.ToArray(); result.bootstraps = bootstraps.ToArray();
            return result;
        }

        private static DungeonBootstrapReadback Bootstrap(ShipBootstrap value)
        {
            ShipLifetimeScope scope = value.GetComponent<ShipLifetimeScope>();
            return new DungeonBootstrapReadback {
                path = AgentRuntimeSnapshot.PathOf(value.transform), size = value.size.ToString(),
                archetype = value.archetype, shipSeed = value.shipSeed, scenarioSeed = value.scenarioSeed,
                players = value.players, generateOnStart = value.generateOnStart,
                isDirected = value.IsDirected, bakedShipPresent = value.BakedShip != null,
                lifetimeScopePresent = scope != null, lifetimeScopeContainerReady = scope != null && scope.Container != null
            };
        }
        private static DungeonShipReadback Ship(ShipRuntime value)
        {
            ShipLayout layout = value.layout; Scenario scenario = value.scenario;
            int width = layout == null ? 0 : layout.width, length = layout == null ? 0 : layout.length, decks = layout == null ? 0 : layout.decks;
            int seed = layout == null ? 0 : layout.seed, scenarioSeed = scenario == null ? 0 : scenario.seed;
            int cells = layout == null || layout.cells == null ? 0 : layout.cells.Length;
            return new DungeonShipReadback {
                path = AgentRuntimeSnapshot.PathOf(value.transform), archetypeName = layout == null ? "" : layout.archetypeName ?? "", size = layout == null ? "" : layout.size.ToString(),
                mission = scenario == null ? "" : scenario.mission.ToString(), seed = seed, scenarioSeed = scenarioSeed, players = scenario == null ? 0 : scenario.players,
                width = width, length = length, decks = decks, cells = cells, rooms = layout == null ? 0 : layout.rooms.Count, doors = layout == null ? 0 : layout.doors.Count,
                airlocks = layout == null ? 0 : layout.airlocks.Count, zones = layout == null ? 0 : layout.zones.Count, crewCount = value.CrewCount,
                playerAttached = value.Player != null, layoutPresent = layout != null, scenarioPresent = scenario != null,
                dimensionsValid = width > 0 && length > 0 && decks > 0, cellsMatchDimensions = width > 0 && length > 0 && decks > 0 && cells == width * length * decks,
                seedInAuthoredRandomRange = seed >= 1 && seed < 99999, scenarioSeedInAuthoredRandomRange = scenarioSeed >= 1 && scenarioSeed < 99999
            };
        }
    }

    public sealed class DungeonReadbackResult
    {
        public int frame, objectCount, rendererCount, missingScriptCount, missingMaterialSlots, missingShaders, unsupportedShaders;
        public string unityVersion, graphicsDevice, graphicsApi;
        public DungeonRootReadback[] roots; public DungeonShipReadback[] ships; public DungeonBootstrapReadback[] bootstraps;
    }
    public sealed class DungeonRootReadback { public string path, scene; public bool activeSelf; }
    public sealed class DungeonBootstrapReadback
    {
        public string path, size; public int archetype, shipSeed, scenarioSeed, players;
        public bool generateOnStart, isDirected, bakedShipPresent, lifetimeScopePresent, lifetimeScopeContainerReady;
    }
    public sealed class DungeonShipReadback
    {
        public string path, archetypeName, size, mission;
        public int seed, scenarioSeed, players, width, length, decks, rooms, doors, airlocks, zones, cells, crewCount;
        public bool layoutPresent, scenarioPresent, dimensionsValid, cellsMatchDimensions, seedInAuthoredRandomRange, scenarioSeedInAuthoredRandomRange, playerAttached;
    }
}
