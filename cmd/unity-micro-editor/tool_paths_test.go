package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
)

func TestToolInstallationIsIndependentOfTargetProject(t *testing.T) {
	installation, err := microEditorRoot()
	if err != nil {
		t.Fatal(err)
	}
	t.Setenv("UNITY_MICRO_EDITOR_ROOT", installation)
	// A game project has neither compiler helpers nor runtime source copies.
	t.Chdir(t.TempDir())
	resolved, err := microEditorRoot()
	if err != nil || resolved != installation {
		t.Fatalf("installation from foreign project: %q, %v", resolved, err)
	}
}

func TestExplicitInvalidInstallationDoesNotFallBackToCheckout(t *testing.T) {
	t.Setenv("UNITY_MICRO_EDITOR_ROOT", t.TempDir())
	if _, err := microEditorRoot(); err == nil {
		t.Fatal("explicit invalid tool installation was silently replaced")
	}
}

func TestPinnedEngineAndLocalPackageAgree(t *testing.T) {
	var profile struct {
		UnityVersion  string `json:"unityVersion"`
		UnityRevision string `json:"unityRevision"`
	}
	if err := json.Unmarshal(engineProfile, &profile); err != nil {
		t.Fatal(err)
	}
	if profile.UnityVersion != fixtureUnityVersion || profile.UnityRevision != "45d8eee7de74" {
		t.Fatal("embedded profile differs from the native protocol implementation")
	}
	root, err := microEditorRoot()
	if err != nil {
		t.Fatal(err)
	}
	version, err := os.ReadFile(filepath.Join(root, "HostProject/ProjectSettings/ProjectVersion.txt"))
	if err != nil || string(version) != "m_EditorVersion: "+profile.UnityVersion+"\nm_EditorVersionWithRevision: "+profile.UnityVersion+" ("+profile.UnityRevision+")\n" {
		t.Fatalf("host version differs from embedded profile: %s, %v", version, err)
	}
	for _, location := range []string{"HostProject/Packages"} {
		if _, err := os.Stat(filepath.Join(root, location, "manifest.json")); err != nil {
			t.Fatal(err)
		}
		var manifest struct {
			Dependencies map[string]string `json:"dependencies"`
		}
		if err := readJSONFile(filepath.Join(root, location, "manifest.json"), &manifest); err != nil {
			t.Fatal(err)
		}
		dependency := manifest.Dependencies["com.unitymicroeditor.runtime"]
		if len(dependency) < 5 || dependency[:5] != "file:" {
			t.Fatal("host or integration lacks the local runtime package")
		}
		packageRoot := filepath.Join(root, location, dependency[5:])
		if _, err := os.Stat(filepath.Join(packageRoot, "Runtime/Dungeon.CLIPlay.asmdef")); err != nil {
			t.Fatalf("local runtime package cannot be resolved: %v", err)
		}
	}
}
