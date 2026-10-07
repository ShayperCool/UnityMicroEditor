package main

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"testing"
)

func TestAgentLibrarySeedRetainsImportedAssetsWithoutSourceProcessState(t *testing.T) {
	project := agentReviewProject(t)
	if err := installAgentPackage(project); err != nil {
		t.Fatal(err)
	}
	stage, snapshot, err := cloneAgentStaging(context.Background(), project, filepath.Join(project, "Library/UnityMicroEditor/preparations/seed-policy/host-stage"))
	if err != nil {
		t.Fatal(err)
	}
	retained := []string{"ArtifactDB", "SourceAssetDB", "ShaderCache/compiled", "PackageCache/com.example@pinned/Runtime.cs"}
	excluded := []string{"PackageManager/ProjectCache", "ScriptAssemblies/Game.dll", "Bee/Backend.log", "burst.pid", "ilpp.pid", "ArtifactDB-lock", "SourceAssetDB-lock", "resolver.lock"}
	for _, path := range append(append([]string{}, retained...), excluded...) {
		physical := filepath.Join(project, "Library", filepath.FromSlash(path))
		if err := os.MkdirAll(filepath.Dir(physical), 0700); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(physical, []byte(project+":"+path), 0600); err != nil {
			t.Fatal(err)
		}
	}
	if err := seedAgentNativeLibrary(context.Background(), project, stage); err != nil {
		t.Fatal(err)
	}
	for _, path := range retained {
		data, err := os.ReadFile(filepath.Join(stage, "Library", filepath.FromSlash(path)))
		if err != nil || string(data) != project+":"+path {
			t.Fatalf("imported asset cache %s was not retained: %v", path, err)
		}
	}
	for _, path := range excluded {
		if _, err := os.Lstat(filepath.Join(stage, "Library", filepath.FromSlash(path))); !errors.Is(err, os.ErrNotExist) {
			t.Fatalf("copied source process/compiler state %s: %v", path, err)
		}
		if _, err := os.Lstat(filepath.Join(project, "Library", filepath.FromSlash(path))); err != nil {
			t.Fatalf("changed original source cache %s: %v", path, err)
		}
	}
	if err := verifyAgentSnapshot(context.Background(), snapshot); err != nil {
		t.Fatal("seed changed authored source", err)
	}
	if err := seedAgentNativeLibrary(context.Background(), project, stage); err != nil {
		t.Fatal("same-policy seed replay failed", err)
	}
}
