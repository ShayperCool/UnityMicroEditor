// check-dungeon-reference checks native Editor health and random-scenario
// invariants without treating a random generated world as an exact fixture.
package main

import (
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

type bootstrap struct {
	Archetype, ShipSeed, ScenarioSeed, Players    int
	GenerateOnStart, IsDirected, BakedShipPresent bool
}
type ship struct {
	Seed, ScenarioSeed, Players, Width, Length, Decks, Rooms, Cells, CrewCount   int
	LayoutPresent, ScenarioPresent, DimensionsValid, CellsMatchDimensions        bool
	SeedInAuthoredRandomRange, ScenarioSeedInAuthoredRandomRange, PlayerAttached bool
}
type reference struct {
	UnityVersion, Stage, SceneSHA256, GraphicsApi                                                  string
	CompletedPlayTicks, ObjectCount, RendererCount, MissingScriptCount, ErrorCount, ExceptionCount int
	IsPlaying, SourceSceneUnchanged                                                                bool
	Bootstraps                                                                                     []bootstrap
	Ships                                                                                          []ship
	Logs                                                                                           []logEntry
}
type logEntry struct{ Type, Message, Stack string }
type identity struct {
	GUID   string
	FileID int64
	Type   string
}
type oracle struct {
	PrefabUnchanged, ModelUnchanged, MissingModelRendererAbsent                                 bool
	StaleRendererFileID                                                                         int64
	RendererCount, LoadedModelAssetCount, ModelGraphObjectCount, MissingMaterialOverrideTargets int
	LoadedModelAssets, ModelGraphObjects                                                        []identity
}
type assertion struct {
	Name    string `json:"name"`
	Success bool   `json:"success"`
}

func verify(authored, played reference, prefab oracle) []assertion {
	checks := []assertion{}
	add := func(name string, value bool) { checks = append(checks, assertion{name, value}) }
	add("verified Unity profile", authored.UnityVersion == "6000.6.3f1" && played.UnityVersion == authored.UnityVersion)
	add("Edit and Play receipts are distinct checkpoints", authored.Stage == "authoring" && !authored.IsPlaying && played.Stage == "play" && played.IsPlaying)
	add("60 actual PostLateUpdate ticks", played.CompletedPlayTicks == 60)
	add("authored scene hash unchanged", authored.SceneSHA256 != "" && authored.SceneSHA256 == played.SceneSHA256 && authored.SourceSceneUnchanged && played.SourceSceneUnchanged)
	add("Play has a graphics device", played.GraphicsApi != "" && played.GraphicsApi != "Null")
	add("native Play has no errors or exceptions", played.ErrorCount == 0 && played.ExceptionCount == 0)
	add("native Play has no missing scripts", played.MissingScriptCount == 0)
	add("generated scene has renderers and more objects than authored scene", played.RendererCount > 0 && played.ObjectCount > authored.ObjectCount)
	add("one authored Bootstrap", len(authored.Bootstraps) == 1)
	if len(authored.Bootstraps) == 1 {
		b := authored.Bootstraps[0]
		add("authored random archetype and seeds retained", b.Archetype == -1 && b.ShipSeed == 0 && b.ScenarioSeed == 0 && b.Players == 1 && b.GenerateOnStart && !b.IsDirected && !b.BakedShipPresent)
	}
	add("one generated ShipRuntime", len(played.Ships) == 1)
	if len(played.Ships) == 1 {
		s := played.Ships[0]
		add("public layout and scenario present", s.LayoutPresent && s.ScenarioPresent)
		add("random seeds fall within original generator range", s.Seed >= 1 && s.Seed < 99999 && s.ScenarioSeed >= 1 && s.ScenarioSeed < 99999 && s.SeedInAuthoredRandomRange && s.ScenarioSeedInAuthoredRandomRange)
		add("layout dimensions match actual public cells array", s.Width > 0 && s.Length > 0 && s.Decks > 0 && s.Cells == s.Width*s.Length*s.Decks && s.DimensionsValid && s.CellsMatchDimensions && s.Rooms > 0)
		add("single player attached to generated ship", s.Players == 1 && s.CrewCount == 1 && s.PlayerAttached)
	}
	add("prefab and FBX content hashes unchanged", prefab.PrefabUnchanged && prefab.ModelUnchanged)
	add("public model asset inventory complete", prefab.LoadedModelAssetCount > 0 && prefab.LoadedModelAssetCount == len(prefab.LoadedModelAssets) && prefab.ModelGraphObjectCount == len(prefab.ModelGraphObjects))
	missing := prefab.MissingModelRendererAbsent && prefab.StaleRendererFileID == -5824371076177060002
	for _, item := range prefab.ModelGraphObjects {
		if item.FileID == prefab.StaleRendererFileID {
			missing = false
		}
	}
	add("stale renderer is absent from current native model graph", missing)
	add("public prefab loading retains live renderers", prefab.RendererCount > 0)
	// Whether GetPropertyModifications retains a null target is evidence, not
	// a required API behavior. The current model identity absence is mandatory.
	return checks
}

func readJSON(path string, value any) error {
	file, err := os.Open(path)
	if err != nil {
		return err
	}
	defer file.Close()
	return json.NewDecoder(file).Decode(value)
}

func gameInvariants(checks []assertion, played reference) bool {
	for _, check := range checks {
		if !check.Success && check.Name != "native Play has no errors or exceptions" {
			return false
		}
	}
	if played.ErrorCount != 0 {
		return false
	}
	knownSearch := 0
	for _, entry := range played.Logs {
		if entry.Type != "Exception" {
			continue
		}
		if !strings.Contains(entry.Stack, "UnityEditor.Search.SearchInit.IndexationOnStartup") ||
			!strings.Contains(entry.Stack, "UnityEditor.Search.SearchDatabase.GetDefaultSearchDatabase") ||
			strings.Contains(entry.Stack, "Dungeon.") || strings.Contains(entry.Stack, "AgentParity.") {
			return false
		}
		knownSearch++
	}
	return knownSearch == played.ExceptionCount
}

func run() error {
	directory := flag.String("reference", "", "native Editor evidence directory")
	output := flag.String("output", "", "assertion report JSON path")
	flag.Parse()
	if *directory == "" || *output == "" {
		return errors.New("provide reference and output")
	}
	var authored, played reference
	var prefab oracle
	for _, item := range []struct {
		name  string
		value any
	}{{"authoring.json", &authored}, {"play.json", &played}, {"shaft-rim-prefab-oracle.json", &prefab}} {
		if err := readJSON(filepath.Join(*directory, item.name), item.value); err != nil {
			return fmt.Errorf("%s: %w", item.name, err)
		}
	}
	checks := verify(authored, played, prefab)
	passed := true
	for _, check := range checks {
		if !check.Success {
			passed = false
		}
	}
	report := map[string]any{"schemaVersion": 1, "success": passed, "gameInvariantsSuccess": gameInvariants(checks, played), "reference": *directory, "checks": checks, "scope": "Native Editor invariants only. Random worlds and Pixel parity are not compared.", "missingMaterialOverrideTargets": prefab.MissingMaterialOverrideTargets, "editorLogs": played.Logs}
	data, err := json.MarshalIndent(report, "", "  ")
	if err != nil {
		return err
	}
	if err := os.WriteFile(*output, append(data, '\n'), 0600); err != nil {
		return err
	}
	if !passed {
		return errors.New("native Editor reference assertions failed; inspect report")
	}
	fmt.Printf("%d native Editor reference assertions passed\n", len(checks))
	return nil
}
func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
