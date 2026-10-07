package main

import "testing"

func fixture() (readback, logs) {
	v := readback{UnityVersion: "6000.6.3f1", GraphicsApi: "OpenGLCore", GraphicsDevice: "NVIDIA", ObjectCount: 19000, RendererCount: 15000,
		Bootstraps: []bootstrap{{Path: "Ship Bootstrap", Archetype: -1, Players: 1, GenerateOnStart: true, LifetimeScopePresent: true, LifetimeScopeContainerReady: true}},
		Ships:      []ship{{ArchetypeName: "Research Vessel", Size: "Medium", Mission: "Data", Seed: 357, ScenarioSeed: 658, Players: 1, Width: 10, Length: 12, Decks: 2, Rooms: 4, Cells: 240, CrewCount: 1, LayoutPresent: true, ScenarioPresent: true, DimensionsValid: true, CellsMatchDimensions: true, SeedInAuthoredRandomRange: true, ScenarioSeedInAuthoredRandomRange: true, PlayerAttached: true}}}
	return v, logs{FirstAvailableSequence: 1, LastSequence: 1}
}
func TestGeneratedRuntimeInvariantsPassWithIndependentSeeds(t *testing.T) {
	v, l := fixture()
	for _, check := range verify(v, l) {
		if !check.Success {
			t.Fatalf("unexpected failure %s", check.Name)
		}
	}
}
func TestWrongContainerCellsAndIncompleteLogWindowFail(t *testing.T) {
	v, l := fixture()
	v.Bootstraps[0].LifetimeScopeContainerReady = false
	v.Ships[0].Cells--
	l.FirstAvailableSequence = 2
	failed := map[string]bool{}
	for _, check := range verify(v, l) {
		if !check.Success {
			failed[check.Name] = true
		}
	}
	for _, name := range []string{"public VContainer scope and Container initialized", "public layout dimensions/cells/rooms consistent", "runtime log window complete from sequence1"} {
		if !failed[name] {
			t.Fatalf("missing failure %s", name)
		}
	}
}
func TestFailedAttributionReplyIsRejected(t *testing.T) {
	if _, err := unwrap([]byte(`{"success":false,"data":{"ships":[]}}`), "ships"); err == nil {
		t.Fatal("failure envelope accepted")
	}
	if _, err := unwrap([]byte(`{"success":true,"data":{"result":{"ships":[]}}}`), "ships"); err != nil {
		t.Fatal(err)
	}
}
func TestPartialLogsDoNotBecomeStrictHealthSuccess(t *testing.T) {
	v, l := fixture()
	l.FirstAvailableSequence = 48
	strict, game := resultStatus(verify(v, l))
	if strict || !game {
		t.Fatalf("strict=%v game=%v", strict, game)
	}
	v.MissingShaders = 1
	strict, game = resultStatus(verify(v, l))
	if strict || game {
		t.Fatal("shader failure accepted as game invariant success")
	}
}
