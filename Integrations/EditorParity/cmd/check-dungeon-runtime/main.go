// check-dungeon-runtime checks actual attributed-tool public readbacks. Random
// ship seeds are validated against generator invariants, never compared equal.
package main

import (
	"bytes"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"os"
)

type bootstrap struct {
	Path, Size                                                                                       string
	Archetype, ShipSeed, ScenarioSeed, Players                                                       int
	GenerateOnStart, IsDirected, BakedShipPresent, LifetimeScopePresent, LifetimeScopeContainerReady bool
}
type ship struct {
	Path, ArchetypeName, Size, Mission                                           string
	Seed, ScenarioSeed, Players, Width, Length, Decks, Rooms, Cells, CrewCount   int
	LayoutPresent, ScenarioPresent, DimensionsValid, CellsMatchDimensions        bool
	SeedInAuthoredRandomRange, ScenarioSeedInAuthoredRandomRange, PlayerAttached bool
}
type readback struct {
	UnityVersion, GraphicsApi, GraphicsDevice                                                                       string
	Frame, ObjectCount, RendererCount, MissingScriptCount, MissingMaterialSlots, MissingShaders, UnsupportedShaders int
	Ships                                                                                                           []ship
	Bootstraps                                                                                                      []bootstrap
}
type logs struct {
	FirstAvailableSequence, LastSequence int
	Entries                              []struct{ Type, Message, StackTrace string }
}
type assertion struct {
	Name    string `json:"name"`
	Success bool   `json:"success"`
}

func unwrap(data []byte, key string) (json.RawMessage, error) {
	var value map[string]json.RawMessage
	if err := json.Unmarshal(data, &value); err != nil {
		return nil, err
	}
	if string(value["success"]) == "false" || string(value["isError"]) == "true" || len(value["error"]) > 0 && string(value["error"]) != "null" {
		return nil, errors.New("runtime operation failed")
	}
	if value[key] != nil {
		return data, nil
	}
	for _, field := range []string{"result", "structuredContent", "data"} {
		if nested := value[field]; len(nested) > 0 && string(nested) != "null" {
			if out, err := unwrap(nested, key); err == nil {
				return out, nil
			}
		}
	}
	return nil, fmt.Errorf("runtime result missing %s", key)
}
func read(path, key string, value any) error {
	data, err := os.ReadFile(path)
	if err != nil {
		return err
	}
	data, err = unwrap(data, key)
	if err != nil {
		return err
	}
	decoder := json.NewDecoder(bytes.NewReader(data))
	return decoder.Decode(value)
}
func verify(value readback, log logs) []assertion {
	checks := []assertion{}
	add := func(name string, ok bool) { checks = append(checks, assertion{name, ok}) }
	add("verified Unity profile with actual graphics device", value.UnityVersion == "6000.6.3f1" && value.GraphicsDevice != "" && value.GraphicsApi != "" && value.GraphicsApi != "Null")
	add("public runtime has generated objects and renderers", value.ObjectCount > 3 && value.RendererCount > 0)
	add("no missing runtime scripts", value.MissingScriptCount == 0)
	add("all observed material slots/shaders present and supported", value.MissingMaterialSlots == 0 && value.MissingShaders == 0 && value.UnsupportedShaders == 0)
	add("one original Bootstrap retained", len(value.Bootstraps) == 1)
	if len(value.Bootstraps) == 1 {
		b := value.Bootstraps[0]
		add("original random settings unchanged", b.Path == "Ship Bootstrap" && b.Archetype == -1 && b.ShipSeed == 0 && b.ScenarioSeed == 0 && b.Players == 1 && b.GenerateOnStart && !b.IsDirected && !b.BakedShipPresent)
		add("public VContainer scope and Container initialized", b.LifetimeScopePresent && b.LifetimeScopeContainerReady)
	}
	add("one generated ShipRuntime", len(value.Ships) == 1)
	if len(value.Ships) == 1 {
		s := value.Ships[0]
		add("public layout and scenario present", s.LayoutPresent && s.ScenarioPresent)
		add("random ship/scenario seeds valid", s.Seed >= 1 && s.Seed < 99999 && s.ScenarioSeed >= 1 && s.ScenarioSeed < 99999 && s.SeedInAuthoredRandomRange && s.ScenarioSeedInAuthoredRandomRange)
		add("public layout dimensions/cells/rooms consistent", s.Width > 0 && s.Length > 0 && s.Decks > 0 && s.Cells == s.Width*s.Length*s.Decks && s.Rooms > 0 && s.DimensionsValid && s.CellsMatchDimensions)
		add("one player/crew actually attached", s.Players == 1 && s.CrewCount == 1 && s.PlayerAttached)
		add("public generator selections named", s.ArchetypeName != "" && s.Size == "Medium" && s.Mission != "")
	}
	add("runtime log window complete from sequence1", log.FirstAvailableSequence == 1 || log.LastSequence == 0)
	healthy := true
	for _, entry := range log.Entries {
		if entry.Type == "Error" || entry.Type == "Exception" || entry.Type == "Assert" {
			healthy = false
		}
	}
	add("captured runtime has no errors/exceptions", healthy)
	return checks
}
func resultStatus(checks []assertion) (strict, game bool) {
	strict, game = true, true
	for _, check := range checks {
		if check.Success {
			continue
		}
		strict = false
		if check.Name != "runtime log window complete from sequence1" && check.Name != "captured runtime has no errors/exceptions" {
			game = false
		}
	}
	return
}
func run() error {
	runtime := flag.String("runtime", "", "project.dungeon.reference result")
	logPath := flag.String("logs", "", "actual runtime_query logs afterSequence0 result")
	output := flag.String("output", "", "assertion JSON output")
	flag.Parse()
	if *runtime == "" || *logPath == "" || *output == "" {
		return errors.New("provide runtime, logs and output")
	}
	var value readback
	var log logs
	if err := read(*runtime, "ships", &value); err != nil {
		return err
	}
	if err := read(*logPath, "entries", &log); err != nil {
		return err
	}
	checks := verify(value, log)
	passed, gamePassed := resultStatus(checks)
	report := map[string]any{"schemaVersion": 1, "success": passed, "gameInvariantsSuccess": gamePassed, "runtime": *runtime, "logs": *logPath, "checks": checks, "observed": value, "scope": "Actual public game state, material/shader health and generator invariants. gameInvariantsSuccess excludes the two log checks and never establishes complete runtime health. Native Editor and Player random generated worlds are not compared equal."}
	data, err := json.MarshalIndent(report, "", "  ")
	if err != nil {
		return err
	}
	if err = os.WriteFile(*output, append(data, '\n'), 0600); err != nil {
		return err
	}
	if !passed {
		return errors.New("actual runtime invariant assertions failed; inspect report")
	}
	fmt.Printf("%d actual Dungeon runtime assertions passed\n", len(checks))
	return nil
}
func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
