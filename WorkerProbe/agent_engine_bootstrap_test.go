package main

import (
	"context"
	"os"
	"path/filepath"
	"testing"
)

func TestEngineBootstrapReceiptCapturesFactsWithoutPlayableBaseline(t *testing.T) {
	project := t.TempDir()
	id, scene := "bootstrap-test", "Assets/Test.unity"
	directory := filepath.Join(project, "Library/UnityMicroEditor/preparations", id)
	host := filepath.Join(directory, "host/Project.x86_64")
	graph := filepath.Join(directory, "graph.json")
	catalog := filepath.Join(directory, "native-catalog.json")
	dependency := filepath.Join(directory, "host/Project_Data/Managed/Game.dll")
	for _, file := range []string{host, graph, catalog, dependency} {
		if err := os.MkdirAll(filepath.Dir(file), 0700); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(file, []byte("original"), 0600); err != nil {
			t.Fatal(err)
		}
	}
	files, err := agentEngineBootstrapFiles(context.Background(), project, host, graph, catalog, "")
	if err != nil {
		t.Fatal(err)
	}
	result := agentPreparation{SchemaVersion: 1, Project: project, Scene: scene, RequestID: id, HostPlayer: host,
		ReferenceGraph: graph, NativeCatalog: catalog, ArtifactReceipt: filepath.Join(directory, "artifacts.json"),
		Backends: map[string]string{"preparation": "native_engine_bootstrap"}}
	if err := agentImmutableJSON(result.ArtifactReceipt, files); err != nil {
		t.Fatal(err)
	}
	if err := agentImmutableJSON(filepath.Join(directory, "result.json"), result); err != nil {
		t.Fatal(err)
	}
	if retry, err := recoverAgentEngineBootstrap(context.Background(), project, scene, id, host); err != nil || retry.ReferenceGraph == "" {
		t.Fatalf("retry forgot completed bootstrap/default-warming intent: %+v %v", retry, err)
	}
	if _, err := recoverAgentEngineBootstrap(context.Background(), project, scene, id, "/changed/host"); err == nil {
		t.Fatal("retry accepted a different host for its completed bootstrap")
	}
	if ordinary, err := recoverAgentEngineBootstrap(context.Background(), project, scene, "source-only-request", host); err != nil || ordinary.ReferenceGraph != "" {
		t.Fatalf("ordinary source-only preparation acquired defaults-warming intent: %+v %v", ordinary, err)
	}
	validate := func() error {
		return validateAgentEngineBootstrap(context.Background(), project, scene, id, directory, result)
	}
	if err := validate(); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(filepath.Join(directory, "current.json")); !os.IsNotExist(err) {
		t.Fatal("native bootstrap required or authored a playable baseline")
	}
	for _, file := range []string{host, graph, catalog, dependency} {
		if err := os.WriteFile(file, []byte("changed"), 0600); err != nil {
			t.Fatal(err)
		}
		if err := validate(); err == nil {
			t.Fatalf("bootstrap reused a changed artifact: %s", file)
		}
		if err := os.WriteFile(file, []byte("original"), 0600); err != nil {
			t.Fatal(err)
		}
	}
	extra := filepath.Join(filepath.Dir(dependency), "Unrecorded.dll")
	if err := os.WriteFile(extra, []byte("new"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := validate(); err == nil {
		t.Fatal("bootstrap reused a host with an unrecorded assembly")
	}
	if err := os.Remove(extra); err != nil {
		t.Fatal(err)
	}
	result.Baseline = filepath.Join(directory, "current.json")
	if err := validate(); err == nil {
		t.Fatal("legacy playable-baseline receipt accepted as native-only bootstrap")
	}
}

func TestEngineBootstrapSuppliedInputsAreReadOnlyOutsideTargetCache(t *testing.T) {
	project, external := t.TempDir(), t.TempDir()
	graph := filepath.Join(project, "Library/UnityMicroEditor/bootstrap/graph.json")
	host, catalog := filepath.Join(external, "Project.x86_64"), filepath.Join(external, "catalog.json")
	if err := os.MkdirAll(filepath.Dir(graph), 0700); err != nil {
		t.Fatal(err)
	}
	for _, file := range []string{graph, host, catalog} {
		if err := os.WriteFile(file, []byte("source"), 0600); err != nil {
			t.Fatal(err)
		}
	}
	if files, err := agentEngineBootstrapFiles(context.Background(), project, host, graph, catalog, ""); err != nil || len(files) != 3 {
		t.Fatalf("explicit read-only host/catalog were rejected: %v", err)
	}
	if _, err := agentEngineBootstrapFiles(context.Background(), project, host, catalog, catalog, ""); err == nil {
		t.Fatal("generated graph outside target cache was accepted")
	}
	if err := os.Remove(catalog); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(graph, catalog); err != nil {
		t.Fatal(err)
	}
	if _, err := agentEngineBootstrapFiles(context.Background(), project, host, graph, catalog, ""); err == nil {
		t.Fatal("symlink supplied catalog was accepted")
	}
}

func TestEngineProfileCacheSeparatesHostsWithIdenticalLaunchers(t *testing.T) {
	project := t.TempDir()
	first := filepath.Join(project, "Library/UnityMicroEditor/host-first/Project.x86_64")
	second := filepath.Join(project, "Library/UnityMicroEditor/host-second/Project.x86_64")
	proof := func(host string) string {
		return filepath.Join(filepath.Dir(goSourceManagedDirectory(host)), "UnityMicroEditorSourceProfile.json")
	}
	for _, host := range []string{first, second} {
		if err := os.MkdirAll(filepath.Dir(proof(host)), 0700); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(host, []byte("identical Unity launcher"), 0700); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(proof(host), []byte(`{"source":"first"}`), 0600); err != nil {
			t.Fatal(err)
		}
	}
	key := func(host string) string {
		t.Helper()
		path, err := agentSourceEngineProfileCachePath(project, host)
		if err != nil {
			t.Fatal(err)
		}
		return path
	}
	firstKey, secondKey := key(first), key(second)
	if firstKey == secondKey || key(first) != firstKey {
		t.Fatal("different host paths collided or unchanged host key was unstable")
	}
	if err := os.WriteFile(proof(first), []byte(`{"source":"new build"}`), 0600); err != nil {
		t.Fatal(err)
	}
	if key(first) == firstKey {
		t.Fatal("changed compiled-source proof reused the old engine profile key")
	}
	if err := os.Remove(proof(first)); err != nil {
		t.Fatal(err)
	}
	if _, err := agentSourceEngineProfileCachePath(project, first); err == nil {
		t.Fatal("host without compiled-source proof acquired an engine profile key")
	}
}
