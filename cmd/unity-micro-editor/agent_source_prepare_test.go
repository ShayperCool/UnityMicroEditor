package main

import (
	"context"
	"errors"
	"path/filepath"
	"testing"

	"unitymicroeditor/internal/project/authoring"
)

func TestSourcePreparationRetryKeepsResolvedSeedAfterActivation(t *testing.T) {
	project := t.TempDir()
	service := &agentProjectService{project: project, editor: "/pinned/Unity", scene: "Assets/First.unity", noEditor: true}
	firstBaseline := filepath.Join(project, "Library/UnityMicroEditor/preparations/previous/current.json")
	previous := agentPreparation{SchemaVersion: 1, Project: project, Scene: service.scene, Baseline: firstBaseline,
		HostPlayer: "/host/first", NativeCatalog: "/catalog/first", EngineProfile: "/profile/first",
		ReferenceGraph: filepath.Join(project, "Library/UnityMicroEditor/native-bootstrap/graph.json"),
		Backends:       map[string]string{"preparation": "go_authored_source"}}
	if err := agentImmutableJSON(filepath.Join(filepath.Dir(firstBaseline), "result.json"), previous); err != nil {
		t.Fatal(err)
	}
	service.baseline = firstBaseline
	directory := filepath.Join(project, "Library/UnityMicroEditor/preparations/retry")
	first, err := service.sourcePreparationSeed(map[string]any{}, directory)
	if err != nil {
		t.Fatal(err)
	}
	service.scene, service.baseline = "Assets/Second.unity", ""
	second, err := service.sourcePreparationSeed(map[string]any{}, directory)
	if err != nil || second != first || second.HostPlayer != previous.HostPlayer || second.ReferenceGraph == "" {
		t.Fatalf("retry changed the accepted seed: %#v %#v %v", first, second, err)
	}
	if _, err := service.sourcePreparationSeed(map[string]any{"scene": "Assets/Second.unity"}, directory); err == nil {
		t.Fatal("same request ID accepted different explicit options")
	}
}

func TestSourcePreparationSeedDoesNotUseOwnGraphAsPublicOracle(t *testing.T) {
	project := t.TempDir()
	baseline := filepath.Join(project, "Library/UnityMicroEditor/preparations/previous/current.json")
	service := &agentProjectService{project: project, editor: "/pinned/Unity", scene: "Assets/Test.unity", baseline: baseline}
	previous := agentPreparation{SchemaVersion: 1, Project: project, Scene: service.scene, Baseline: baseline, HostPlayer: "/host/first",
		Backends: map[string]string{"preparation": "go_authored_source"}}
	if err := agentImmutableJSON(filepath.Join(filepath.Dir(baseline), "result.json"), previous); err != nil {
		t.Fatal(err)
	}
	seed, err := service.sourcePreparationSeed(map[string]any{}, filepath.Join(project, "Library/UnityMicroEditor/preparations/retry"))
	if err != nil || seed.ReferenceGraph != "" {
		t.Fatalf("our graph was mislabeled as public engine evidence: %+v %v", seed, err)
	}
}

func TestSourcePreparationAndDefaultsBootstrapHonorNoEditor(t *testing.T) {
	service, _, _ := agentDefaultsTestFixture(t)
	service.scene = "Assets/Test.unity"
	_, err := service.prepareSource(context.Background(), map[string]any{"requestId": "missing-host"})
	var typed *authoring.Error
	if !errors.As(err, &typed) || typed.Code != "native_host_missing" {
		t.Fatalf("source preparation did not fail before Editor bootstrap: %v", err)
	}
	if _, err := service.captureComponentDefaults(context.Background()); !errors.As(err, &typed) || typed.Code != "editor_disabled" {
		t.Fatalf("defaults bootstrap bypassed no-editor: %v", err)
	}
	if service.client != nil {
		t.Fatal("no-editor preparation constructed Editor client")
	}
}
