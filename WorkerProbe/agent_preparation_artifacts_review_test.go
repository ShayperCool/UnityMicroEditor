package main

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
)

func TestReviewArtifactPreflightRequiresExactSiblingAssemblyConfiguration(t *testing.T) {
	for _, damage := range []string{"missing", "corrupt"} {
		t.Run(damage, func(t *testing.T) {
			project, baseline, manifest, compiler, _ := compilerLocatorFixture(t)
			player := filepath.Join(filepath.Dir(filepath.Dir(filepath.Dir(compiler.Assemblies[0].Path))), "Player.x86_64")
			if err := os.WriteFile(player, []byte("verified native player"), 0700); err != nil {
				t.Fatal(err)
			}
			if err := atomicJSON(manifest, map[string]any{"schemaVersion": 1, "supported": true, "projectRoot": project, "scenePath": "Assets/Main.unity", "unityVersion": fixtureUnityVersion}); err != nil {
				t.Fatal(err)
			}
			if err := atomicJSON(baseline, map[string]any{"schemaVersion": 1, "project": project, "compiler": compiler, "manifest": manifest, "player": player}); err != nil {
				t.Fatal(err)
			}
			artifacts, err := captureAgentPreparationArtifacts(context.Background(), project, baseline, filepath.Join(filepath.Dir(baseline), "artifacts.json"))
			if err != nil {
				t.Fatal(err)
			}
			preparation := agentPreparation{SchemaVersion: 1, Project: project, Scene: "Assets/Main.unity", RequestID: "one", Baseline: baseline, ArtifactReceipt: artifacts}
			if err := validateAgentCachedPreparation(project, fixtureUnityVersion, preparation.Scene, "one", filepath.Dir(baseline), preparation); err != nil {
				t.Fatal(err)
			}
			descriptor := filepath.Join(filepath.Dir(filepath.Dir(filepath.Dir(filepath.Dir(manifest)))), "compiler-cache/baseline-assemblies", compiler.Assemblies[0].AssemblyInputHash, "compile.json")
			if _, err := os.Stat(descriptor); err != nil {
				t.Fatal(err)
			}
			if damage == "missing" {
				if err := os.Remove(descriptor); err != nil {
					t.Fatal(err)
				}
			} else {
				if err := os.WriteFile(descriptor, []byte(`{"assemblyConfiguration":{}}`), 0600); err != nil {
					t.Fatal(err)
				}
			}
			accepted, locator, err := goCompilerManifest(baseline)
			if err != nil {
				t.Fatal(err)
			}
			if _, _, err := readGoAssemblyConfiguration(locator, accepted.Assemblies[0], accepted); err == nil {
				t.Fatal("damaged exact configuration did not prevent compiler startup")
			}
			if err := validateAgentCachedPreparation(project, fixtureUnityVersion, preparation.Scene, "one", filepath.Dir(baseline), preparation); err == nil {
				encoded, _ := json.Marshal(map[string]string{"missingFrozenArtifact": descriptor})
				t.Fatalf("preflight accepted damaged compiler configuration before healthy Player stop: %s", encoded)
			}
		})
	}
}

func TestReviewArtifactPreflightFreezesReferencedNativeCatalog(t *testing.T) {
	for _, damage := range []string{"missing", "corrupt"} {
		t.Run(damage, func(t *testing.T) {
			project := t.TempDir()
			directory := filepath.Join(project, "Library/UnityMicroEditor/preparations/one")
			baseline := filepath.Join(directory, "current.json")
			manifest := filepath.Join(directory, "source/scene.json")
			player := filepath.Join(directory, "Player/Player.x86_64")
			catalog := filepath.Join(project, "Library/UnityMicroEditor/native/native-catalog.json")
			if err := os.MkdirAll(filepath.Dir(player), 0700); err != nil {
				t.Fatal(err)
			}
			if err := os.WriteFile(player, []byte("native host bytes"), 0700); err != nil {
				t.Fatal(err)
			}
			if err := agentImmutableJSON(manifest, map[string]any{"schemaVersion": 1, "supported": true, "projectRoot": project, "scenePath": "Assets/Main.unity", "unityVersion": fixtureUnityVersion}); err != nil {
				t.Fatal(err)
			}
			if err := agentImmutableJSON(catalog, map[string]any{"schemaVersion": 1, "project": project, "assets": []any{}, "packs": []any{}}); err != nil {
				t.Fatal(err)
			}
			compiler := goCompileManifest{Schema: 1, Project: project, Unity: fixtureUnityVersion, NativeOnly: true, CompilerInputs: map[string]string{"generated-proof": "accepted"}}
			if err := agentImmutableJSON(baseline, map[string]any{"schemaVersion": 1, "project": project, "manifest": manifest, "player": player, "compiler": compiler, "nativeImport": map[string]string{"catalog": catalog}}); err != nil {
				t.Fatal(err)
			}
			artifacts, err := captureAgentPreparationArtifacts(context.Background(), project, baseline, filepath.Join(directory, "artifacts.json"))
			if err != nil {
				t.Fatal(err)
			}
			preparation := agentPreparation{SchemaVersion: 1, Project: project, Scene: "Assets/Main.unity", RequestID: "one", Baseline: baseline, ArtifactReceipt: artifacts, NativeCatalog: catalog}
			if err := validateAgentCachedPreparation(project, fixtureUnityVersion, preparation.Scene, "one", directory, preparation); err != nil {
				t.Fatal(err)
			}
			if damage == "missing" {
				if err := os.Remove(catalog); err != nil {
					t.Fatal(err)
				}
			} else {
				if err := os.WriteFile(catalog, []byte("broken generated catalog"), 0600); err != nil {
					t.Fatal(err)
				}
			}
			if err := validateAgentCachedPreparation(project, fixtureUnityVersion, preparation.Scene, "one", directory, preparation); err == nil {
				t.Fatal("preflight accepted missing/corrupt native catalog before healthy Player stop")
			}
		})
	}
}
