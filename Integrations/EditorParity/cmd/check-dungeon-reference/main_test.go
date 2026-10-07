package main

import (
	"encoding/json"
	"testing"
)

func fixture() (reference, reference, oracle) {
	authored := reference{UnityVersion: "6000.6.3f1", Stage: "authoring", SceneSHA256: "unchanged", SourceSceneUnchanged: true, ObjectCount: 3, Bootstraps: []bootstrap{{Archetype: -1, Players: 1, GenerateOnStart: true}}}
	played := reference{UnityVersion: "6000.6.3f1", Stage: "play", SceneSHA256: "unchanged", SourceSceneUnchanged: true, IsPlaying: true, CompletedPlayTicks: 60, GraphicsApi: "OpenGLCore", ObjectCount: 50, RendererCount: 20,
		Ships: []ship{{Seed: 234, ScenarioSeed: 456, Players: 1, Width: 10, Length: 12, Decks: 2, Rooms: 5, Cells: 240, CrewCount: 1, LayoutPresent: true, ScenarioPresent: true, DimensionsValid: true, CellsMatchDimensions: true, SeedInAuthoredRandomRange: true, ScenarioSeedInAuthoredRandomRange: true, PlayerAttached: true}}}
	prefab := oracle{PrefabUnchanged: true, ModelUnchanged: true, MissingModelRendererAbsent: true, StaleRendererFileID: -5824371076177060002, RendererCount: 5, LoadedModelAssetCount: 1, ModelGraphObjectCount: 1, LoadedModelAssets: []identity{{FileID: 919132149155446097}}, ModelGraphObjects: []identity{{FileID: 919132149155446097}}}
	return authored, played, prefab
}
func TestIndependentSeedsAndNullModificationAPIResultAreAllowed(t *testing.T) {
	authored, played, prefab := fixture()
	for _, check := range verify(authored, played, prefab) {
		if !check.Success {
			t.Fatalf("unexpected failure: %s", check.Name)
		}
	}
}
func TestBadPublicRuntimeStateAndPresentStaleTargetAreRejected(t *testing.T) {
	authored, played, prefab := fixture()
	played.ErrorCount = 1
	played.CompletedPlayTicks = 59
	played.Ships[0].Cells--
	played.Ships[0].PlayerAttached = false
	prefab.ModelGraphObjects[0].FileID = prefab.StaleRendererFileID
	failed := map[string]bool{}
	for _, check := range verify(authored, played, prefab) {
		if !check.Success {
			failed[check.Name] = true
		}
	}
	for _, name := range []string{"native Play has no errors or exceptions", "60 actual PostLateUpdate ticks", "layout dimensions match actual public cells array", "single player attached to generated ship", "stale renderer is absent from current native model graph"} {
		if !failed[name] {
			t.Fatalf("did not reject %s", name)
		}
	}
}
func TestModelIdentityUsesExactInt64(t *testing.T) {
	var value oracle
	if err := json.Unmarshal([]byte(`{"staleRendererFileID":-5824371076177060002,"modelGraphObjects":[{"fileID":-5824371076177060002}]}`), &value); err != nil {
		t.Fatal(err)
	}
	if value.StaleRendererFileID != -5824371076177060002 || value.ModelGraphObjects[0].FileID != value.StaleRendererFileID {
		t.Fatal("model identity was rounded")
	}
}

func TestEditorSearchExceptionIsReportedSeparatelyWithoutHidingGameFailure(t *testing.T) {
	authored, played, prefab := fixture()
	played.ExceptionCount = 1
	played.Logs = []logEntry{{Type: "Exception", Stack: "UnityEditor.Search.SearchDatabase.GetDefaultSearchDatabase\nUnityEditor.Search.SearchInit.IndexationOnStartup"}}
	checks := verify(authored, played, prefab)
	if !gameInvariants(checks, played) {
		t.Fatal("known Editor Search exception incorrectly failed game invariants")
	}
	for _, check := range checks {
		if check.Name == "native Play has no errors or exceptions" && check.Success {
			t.Fatal("Editor exception was hidden from strict health assertion")
		}
	}
	played.Logs[0].Stack += "\nDungeon.ShipGen.ShipBootstrap.Start"
	if gameInvariants(verify(authored, played, prefab), played) {
		t.Fatal("game exception was misclassified as Editor-only")
	}
	played.Logs = nil
	if gameInvariants(verify(authored, played, prefab), played) {
		t.Fatal("unrecorded exception was treated as a known Editor-only exception")
	}
}
