package main

import (
	"context"
	"os"
	"path/filepath"
	"testing"
)

func TestPreparationArtifactPreflightRejectsCorruptionAndExtraPlayerFiles(t *testing.T) {
	project := t.TempDir()
	directory := filepath.Join(project, "Library/UnityMicroEditor/preparations/verified")
	player := filepath.Join(directory, "Player/Project.x86_64")
	assembly := filepath.Join(directory, "Player/Project_Data/Managed/Game.dll")
	native := filepath.Join(project, "Library/UnityMicroEditor/native/objects.assets")
	manifest := filepath.Join(directory, "source/scene.json")
	baseline := filepath.Join(directory, "current.json")
	for _, path := range []string{player, assembly, native} {
		if err := os.MkdirAll(filepath.Dir(path), 0700); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, []byte("exact generated "+filepath.Base(path)), 0600); err != nil {
			t.Fatal(err)
		}
	}
	assemblySHA, _ := goFileHash(assembly)
	nativeSHA, _ := goFileHash(native)
	if err := agentImmutableJSON(manifest, map[string]any{"schemaVersion": 1, "supported": true, "projectRoot": project,
		"nativeContentFiles": []any{map[string]any{"path": native, "sha256": nativeSHA}}}); err != nil {
		t.Fatal(err)
	}
	if err := agentImmutableJSON(baseline, map[string]any{"player": player, "manifest": manifest,
		"compiler": goCompileManifest{Assemblies: []goCompileAssembly{{Name: "Game", Path: assembly, SHA256: assemblySHA}}}}); err != nil {
		t.Fatal(err)
	}
	receipt, err := captureAgentPreparationArtifacts(context.Background(), project, baseline, filepath.Join(directory, "artifacts.json"))
	if err != nil {
		t.Fatal(err)
	}
	if err := validateAgentPreparationArtifacts(project, baseline, player, receipt); err != nil {
		t.Fatal(err)
	}
	for _, path := range []string{player, assembly, native, manifest, baseline} {
		t.Run(filepath.Base(path), func(t *testing.T) {
			original, _ := os.ReadFile(path)
			if err := os.WriteFile(path, []byte("corrupt"), 0600); err != nil {
				t.Fatal(err)
			}
			if err := validateAgentPreparationArtifacts(project, baseline, player, receipt); err == nil {
				t.Fatal("corrupted regular artifact accepted")
			}
			if err := os.WriteFile(path, original, 0600); err != nil {
				t.Fatal(err)
			}
		})
	}
	extra := filepath.Join(filepath.Dir(player), "injected.dll")
	if err := os.WriteFile(extra, []byte("unexpected"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := validateAgentPreparationArtifacts(project, baseline, player, receipt); err == nil {
		t.Fatal("unrecorded Player file accepted")
	}
}
