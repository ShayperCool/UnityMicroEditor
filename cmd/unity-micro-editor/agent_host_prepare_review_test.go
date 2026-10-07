package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"

	"unitymicroeditor/internal/project/authoring"
)

func TestAgentHostStageContainsAddressablesGeneratedSourceChanges(t *testing.T) {
	project := agentReviewProject(t)
	addressables := filepath.Join(project, "Assets/AddressableAssetsData")
	if err := os.MkdirAll(addressables, 0700); err != nil {
		t.Fatal(err)
	}
	for _, name := range []string{"link.xml", "link.xml.meta"} {
		if err := os.WriteFile(filepath.Join(addressables, name), []byte("original Addressables "+name), 0600); err != nil {
			t.Fatal(err)
		}
	}
	before, err := snapshotAgentSource(context.Background(), project)
	if err != nil {
		t.Fatal(err)
	}
	stage, snapshot, err := cloneAgentStaging(context.Background(), project, filepath.Join(project, "Library/UnityMicroEditor/preparations/host-test/host-stage"))
	if err != nil {
		t.Fatal(err)
	}
	for _, name := range []string{"link.xml", "link.xml.meta"} {
		if err := os.Remove(filepath.Join(stage, "Assets/AddressableAssetsData", name)); err != nil {
			t.Fatal(err)
		}
	}
	generated := filepath.Join(stage, "Assets/AddressableAssetsData/Linux")
	if err := os.MkdirAll(generated, 0700); err != nil {
		t.Fatal(err)
	}
	for _, name := range []string{"addressables_content_state.bin", "addressables_content_state.bin.meta"} {
		if err := os.WriteFile(filepath.Join(generated, name), []byte("generated build output"), 0600); err != nil {
			t.Fatal(err)
		}
	}
	if err := verifyAgentSnapshot(context.Background(), snapshot); err != nil {
		t.Fatal("stage build mutations reached authored roots", err)
	}
	after, err := snapshotAgentSource(context.Background(), project)
	if err != nil {
		t.Fatal(err)
	}
	first, _ := json.Marshal(before.Files)
	second, _ := json.Marshal(after.Files)
	if !bytes.Equal(first, second) {
		t.Fatal("host-stage preparation changed Assets, Packages or ProjectSettings")
	}
	if _, err := os.Lstat(filepath.Join(addressables, "Linux")); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("generated Linux source escaped its owned clone")
	}
}

func TestAgentHostStageNormalizesRelativePackagesWithoutChangingSourceManifest(t *testing.T) {
	project := agentReviewProject(t)
	packageRoot := filepath.Join(filepath.Dir(project), "registered-package")
	if err := os.Mkdir(packageRoot, 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(packageRoot, "package.json"), []byte(`{"name":"com.example.local"}`), 0600); err != nil {
		t.Fatal(err)
	}
	manifest := filepath.Join(project, "Packages/manifest.json")
	original := []byte("{\n  \"dependencies\": {\"com.example.local\": \"file:../../registered-package\"},\n  \"customValue\": 7\n}\n")
	if err := os.WriteFile(manifest, original, 0600); err != nil {
		t.Fatal(err)
	}
	stage, snapshot, err := cloneAgentStaging(context.Background(), project, filepath.Join(project, "Library/UnityMicroEditor/preparations/host-packages/host-stage"))
	if err != nil {
		t.Fatal(err)
	}
	var staged struct {
		Dependencies map[string]string `json:"dependencies"`
		CustomValue  int               `json:"customValue"`
	}
	if err := agentReadJSON(filepath.Join(stage, "Packages/manifest.json"), 1<<20, &staged); err != nil {
		t.Fatal(err)
	}
	if staged.Dependencies["com.example.local"] != "file:"+filepath.ToSlash(packageRoot) || staged.CustomValue != 7 {
		t.Fatalf("staged package identity changed: %+v", staged)
	}
	latest, err := os.ReadFile(manifest)
	if err != nil || !bytes.Equal(latest, original) {
		t.Fatal("normalization rewrote authored package bytes", err)
	}
	if err := verifyAgentSnapshot(context.Background(), snapshot); err != nil {
		t.Fatal(err)
	}
}

func TestAgentHostWarmLibrarySeedExcludesRecursiveToolStagesAndChecksProvenance(t *testing.T) {
	project := agentReviewProject(t)
	// The source Editor is prepared with the runtime package before its stopped
	// Library can be seeded. The clone must not need its own package install.
	if err := installAgentPackage(project); err != nil {
		t.Fatal(err)
	}
	directory := filepath.Join(project, "Library/UnityMicroEditor/preparations/warm-host/host-stage")
	stage, snapshot, err := cloneAgentStaging(context.Background(), project, directory)
	if err != nil {
		t.Fatal(err)
	}
	artifact := filepath.Join(project, "Library/ArtifactDB")
	if err := os.WriteFile(artifact, []byte("completed owned importer database"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := seedAgentNativeLibrary(context.Background(), project, stage); err != nil {
		t.Fatal(err)
	}
	seeded, err := os.ReadFile(filepath.Join(stage, "Library/ArtifactDB"))
	if err != nil || string(seeded) != "completed owned importer database" {
		t.Fatal("completed Library seed absent", err)
	}
	if _, err := os.Lstat(filepath.Join(stage, "Library/UnityMicroEditor")); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("host/native tool stages or journals recursively entered Library seed")
	}
	var evidence map[string]any
	if err := agentReadJSON(filepath.Join(stage, ".micro-editor-library-seed.json"), 65536, &evidence); err != nil {
		t.Fatal(err)
	}
	if evidence["sourceProject"] != project || evidence["stageProject"] != stage || evidence["sourceEditorOwnedAndStopped"] != true {
		t.Fatal("Library seed lost its explicit stopped-source provenance", evidence)
	}
	if err := seedAgentNativeLibrary(context.Background(), project, stage); err != nil {
		t.Fatal("exact seed replay failed", err)
	}
	if err := seedAgentNativeLibrary(context.Background(), agentReviewProject(t), stage); err == nil {
		t.Fatal("completed seed reused for another authored source")
	}
	if err := verifyAgentSnapshot(context.Background(), snapshot); err != nil {
		t.Fatal("warm seed changed authored source", err)
	}
}

func agentHostReviewCachedFixture(t *testing.T) (*agentProjectService, string, string, string, agentSourceSnapshot) {
	t.Helper()
	project := agentReviewProject(t)
	id := "host-review-cached"
	directory := filepath.Join(project, "Library/UnityMicroEditor/preparations", id, "host-stage")
	stage, snapshot, err := cloneAgentStaging(context.Background(), project, directory)
	if err != nil {
		t.Fatal(err)
	}
	snapshotPath := filepath.Join(directory, "source-snapshot.json")
	if err := agentImmutableJSON(snapshotPath, snapshot); err != nil {
		t.Fatal(err)
	}
	phase := agentPhaseID(id, "host")
	output := filepath.Join(stage, "Builds/UnityMicroEditor", phase)
	player := filepath.Join(output, "Project.x86_64")
	for path, contents := range map[string]string{
		player: "native host executable",
		filepath.Join(output, "Project_Data/Managed/UnityMicroEditor.dll"): "pinned public runtime bridge",
		filepath.Join(output, "Project_Data/ScriptingAssemblies.json"):     `{"names":["UnityMicroEditor.dll","Assembly-CSharp.dll"]}`,
	} {
		if err := os.MkdirAll(filepath.Dir(path), 0700); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, []byte(contents), 0700); err != nil {
			t.Fatal(err)
		}
	}
	artifacts, err := snapshotAgentHostArtifacts(context.Background(), project, stage, phase, player)
	if err != nil {
		t.Fatal(err)
	}
	snapshotSHA, err := goFileHash(snapshotPath)
	if err != nil {
		t.Fatal(err)
	}
	service := &agentProjectService{project: project, editor: "/missing-review-editor", lifetime: context.Background()}
	record := agentHostPreparation{SchemaVersion: 1, Project: project, Editor: service.editor, RequestID: id, Stage: stage, Player: player, SnapshotSHA: snapshotSHA,
		Artifacts: artifacts, Evidence: map[string]any{"backend": "unity_editor", "sourceSnapshotVerified": true, "sourceProject": project, "stageProject": stage}}
	if err := agentImmutableJSON(filepath.Join(directory, "result.json"), record); err != nil {
		t.Fatal(err)
	}
	return service, id, directory, player, snapshot
}

func TestAgentHostReviewCachedReplayVerifiesArtifactsWithoutStartingEditor(t *testing.T) {
	service, id, _, player, snapshot := agentHostReviewCachedFixture(t)
	defer service.Close()
	for replay := 0; replay < 2; replay++ {
		host, evidence, err := prepareAgentHost(context.Background(), service, id)
		if err != nil || host != player || evidence["sourceSnapshotVerified"] != true {
			t.Fatalf("cached host replay %d failed: %s %+v %v", replay, host, evidence, err)
		}
	}
	if service.client != nil {
		t.Fatal("cached host replay acquired a source Editor client")
	}
	if err := verifyAgentSnapshot(context.Background(), snapshot); err != nil {
		t.Fatal(err)
	}
	service.editor = "/another-review-editor"
	var conflict *authoring.Error
	if _, _, err := prepareAgentHost(context.Background(), service, id); !errors.As(err, &conflict) || conflict.Code != "request_id_conflict" {
		t.Fatalf("changed Editor reused the stable host request: %v", err)
	}
}

func TestAgentHostReviewCachedReplayRejectsChangedSourceAndHostOutput(t *testing.T) {
	for _, changed := range []string{"asset-meta", "packages", "project-settings", "player-bytes", "extra-output", "missing-bridge", "output-symlink", "foreign-provenance", "snapshot-proof"} {
		t.Run(changed, func(t *testing.T) {
			service, id, directory, player, _ := agentHostReviewCachedFixture(t)
			defer service.Close()
			stage := filepath.Join(directory, "project")
			output := filepath.Dir(player)
			switch changed {
			case "asset-meta", "packages", "project-settings":
				path := filepath.Join(service.project, "Assets/Sample.cs.meta")
				if changed == "packages" {
					path = filepath.Join(service.project, "Packages/manifest.json")
				}
				if changed == "project-settings" {
					path = filepath.Join(service.project, "ProjectSettings/ProjectVersion.txt")
				}
				if err := os.WriteFile(path, []byte("changed authored source"), 0600); err != nil {
					t.Fatal(err)
				}
			case "player-bytes":
				if err := os.WriteFile(player, []byte("different native executable"), 0700); err != nil {
					t.Fatal(err)
				}
			case "extra-output":
				if err := os.WriteFile(filepath.Join(output, "unexpected.dll"), []byte("new unverified artifact"), 0600); err != nil {
					t.Fatal(err)
				}
			case "missing-bridge":
				if err := os.Remove(filepath.Join(output, "Project_Data/Managed/UnityMicroEditor.dll")); err != nil {
					t.Fatal(err)
				}
			case "output-symlink":
				if err := os.Symlink(service.project, filepath.Join(output, "linked")); err != nil {
					t.Fatal(err)
				}
			case "foreign-provenance", "snapshot-proof":
				var record agentHostPreparation
				if err := agentReadJSON(filepath.Join(directory, "result.json"), 8<<20, &record); err != nil {
					t.Fatal(err)
				}
				if changed == "foreign-provenance" {
					record.Stage = stage + "-another"
				} else {
					record.SnapshotSHA = strings.Repeat("0", 64)
				}
				data, _ := json.Marshal(record)
				if err := os.WriteFile(filepath.Join(directory, "result.json"), data, 0600); err != nil {
					t.Fatal(err)
				}
			}
			host, evidence, err := prepareAgentHost(context.Background(), service, id)
			if err == nil || host != "" || evidence != nil {
				t.Fatalf("unverified host became reusable: %s %+v %v", host, evidence, err)
			}
			if service.client != nil {
				t.Fatal("invalid cached host started a source Editor")
			}
		})
	}
}

func TestAgentHostReviewBuildFailureAndCancellationPreserveAllAuthoredRoots(t *testing.T) {
	for _, cancelled := range []bool{false, true} {
		t.Run(fmt.Sprintf("cancelled=%t", cancelled), func(t *testing.T) {
			project := agentReviewProject(t)
			before, err := snapshotAgentSource(context.Background(), project)
			if err != nil {
				t.Fatal(err)
			}
			service := &agentProjectService{project: project, editor: "/missing-review-editor", lifetime: context.Background()}
			defer service.Close()
			ctx, cancel := context.WithCancel(context.Background())
			defer cancel()
			if cancelled {
				cancel()
			}
			host, evidence, err := prepareAgentHost(ctx, service, "host-failed-review")
			if err == nil || host != "" || evidence != nil {
				t.Fatal("failed host preparation reported reusable output")
			}
			after, snapshotErr := snapshotAgentSource(context.Background(), project)
			if snapshotErr != nil || !reflect.DeepEqual(before.Files, after.Files) {
				t.Fatal("failed/cancelled host changed authored source", snapshotErr)
			}
			if _, err := os.Stat(filepath.Join(project, "Library/UnityMicroEditor/preparations/host-failed-review/host-stage/result.json")); !errors.Is(err, os.ErrNotExist) {
				t.Fatal("failed host published successful result")
			}
		})
	}
}
