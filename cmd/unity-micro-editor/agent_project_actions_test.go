package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"unitymicroeditor/internal/agentcontrol"
	"unitymicroeditor/internal/project/authoring"
)

func agentReviewProject(t *testing.T) string {
	t.Helper()
	project := t.TempDir()
	for _, directory := range []string{"Assets", "Packages", "ProjectSettings"} {
		if err := os.Mkdir(filepath.Join(project, directory), 0700); err != nil {
			t.Fatal(err)
		}
	}
	files := map[string]string{
		"ProjectSettings/ProjectVersion.txt": "m_EditorVersion: " + fixtureUnityVersion + "\n",
		"Packages/manifest.json":             `{"dependencies":{"com.example.test":"1.2.3"},"scopedRegistries":[{"name":"example","url":"https://example.invalid"}],"projectMarker":{"value":7}}` + "\n",
		"Assets/Sample.cs":                   "public class Sample { public int value = 1; }\n",
		"Assets/Sample.cs.meta":              "fileFormatVersion: 2\nguid: 11111111111111111111111111111111\nMonoImporter:\n  externalObjects: {}\n  serializedVersion: 2\n",
		"Assets/Other.cs":                    "public class Other { public int value = 2; }\n",
		"Assets/Other.cs.meta":               "fileFormatVersion: 2\nguid: 22222222222222222222222222222222\nMonoImporter:\n  externalObjects: {}\n  serializedVersion: 2\n",
	}
	for path, content := range files {
		if err := os.WriteFile(filepath.Join(project, path), []byte(content), 0600); err != nil {
			t.Fatal(err)
		}
	}
	return project
}

func TestAgentImmutableReceiptRejectsChangedArgumentsAndSymlink(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "requests/id.json")
	original := map[string]any{"requestId": "original", "arguments": map[string]any{"amount": 1}}
	if err := agentImmutableJSON(path, original); err != nil {
		t.Fatal(err)
	}
	before, _ := os.ReadFile(path)
	if err := agentImmutableJSON(path, original); err != nil {
		t.Fatal("exact request replay failed:", err)
	}
	changed := map[string]any{"requestId": "original", "arguments": map[string]any{"amount": 2}}
	var conflict *authoring.Error
	if err := agentImmutableJSON(path, changed); !errors.As(err, &conflict) || conflict.Code != "request_id_conflict" {
		t.Fatalf("changed identity not rejected: %v", err)
	}
	after, _ := os.ReadFile(path)
	if !bytes.Equal(before, after) {
		t.Fatal("conflict overwrote original receipt")
	}
	outside := filepath.Join(t.TempDir(), "outside.json")
	if err := os.Symlink(outside, filepath.Join(directory, "alias")); err != nil {
		t.Fatal(err)
	}
	if err := agentImmutableJSON(filepath.Join(directory, "alias"), original); err == nil {
		t.Fatal("symlink destination accepted")
	}
	if _, err := os.Lstat(outside); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("rejected receipt wrote outside")
	}
}

func TestAgentPackageInstallPreservesOtherEntriesAndDetectsConflict(t *testing.T) {
	project := agentReviewProject(t)
	if err := installAgentPackage(project); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(project, "Packages/manifest.json")
	var manifest map[string]any
	if err := agentReadJSON(path, 1<<20, &manifest); err != nil {
		t.Fatal(err)
	}
	dependencies := manifest["dependencies"].(map[string]any)
	if dependencies["com.example.test"] != "1.2.3" || manifest["projectMarker"].(map[string]any)["value"] != float64(7) || len(manifest["scopedRegistries"].([]any)) != 1 {
		t.Fatal("unrelated manifest entries changed")
	}
	installed, _ := os.ReadFile(path)
	if err := installAgentPackage(project); err != nil {
		t.Fatal(err)
	}
	repeated, _ := os.ReadFile(path)
	if !bytes.Equal(installed, repeated) {
		t.Fatal("idempotent install rewrote manifest")
	}
	backups, err := filepath.Glob(filepath.Join(project, "Library/UnityMicroEditor/package-install/*.json"))
	if err != nil || len(backups) != 1 {
		t.Fatal("durable package backup absent")
	}
	dependencies["com.unitymicroeditor.runtime"] = "file:/different/package"
	changed, _ := json.Marshal(manifest)
	if err := os.WriteFile(path, changed, 0600); err != nil {
		t.Fatal(err)
	}
	var conflict *authoring.Error
	if err := installAgentPackage(project); !errors.As(err, &conflict) || conflict.Code != "package_conflict" {
		t.Fatalf("package mismatch not rejected: %v", err)
	}
	after, _ := os.ReadFile(path)
	if !bytes.Equal(after, changed) {
		t.Fatal("conflict changed target manifest")
	}
}

func TestAgentEditorValidationPrecedesPackageInstallation(t *testing.T) {
	for _, invalid := range []string{"profile", "executable"} {
		t.Run(invalid, func(t *testing.T) {
			project := agentReviewProject(t)
			manifest := filepath.Join(project, "Packages/manifest.json")
			before, err := os.ReadFile(manifest)
			if err != nil {
				t.Fatal(err)
			}
			editor := "/bin/true"
			if invalid == "profile" {
				if err := os.WriteFile(filepath.Join(project, "ProjectSettings/ProjectVersion.txt"), []byte("m_EditorVersion: 2022.3.0f1\n"), 0600); err != nil {
					t.Fatal(err)
				}
			} else {
				editor = filepath.Join(project, "missing-editor")
			}
			service := &agentProjectService{project: project, editor: editor, lifetime: context.Background()}
			defer service.Close()
			if _, err := service.editorClient(context.Background()); err == nil {
				t.Fatal("invalid Editor target accepted")
			}
			after, err := os.ReadFile(manifest)
			if err != nil || !bytes.Equal(before, after) {
				t.Fatal("rejected target changed authored package manifest", err)
			}
			backups, err := filepath.Glob(filepath.Join(project, "Library/UnityMicroEditor/package-install/*.json"))
			if err != nil || len(backups) != 0 {
				t.Fatal("rejected target installed the runtime package")
			}
		})
	}
}

func TestAgentInvalidEditorProfileDoesNotInstallAuthoredPackage(t *testing.T) {
	project := agentReviewProject(t)
	manifest := filepath.Join(project, "Packages/manifest.json")
	before, _ := os.ReadFile(manifest)
	if err := os.WriteFile(filepath.Join(project, "ProjectSettings/ProjectVersion.txt"), []byte("m_EditorVersion: 6000.5.0f1\n"), 0600); err != nil {
		t.Fatal(err)
	}
	service := &agentProjectService{project: project, editor: "/bin/true", lifetime: context.Background()}
	defer service.Close()
	_, err := service.HandleSync(context.Background(), "component_schema", json.RawMessage(`{"componentType":"BoxCollider","requestId":"invalid-profile-test"}`))
	if err == nil {
		t.Fatal("unsupported project profile accepted")
	}
	after, _ := os.ReadFile(manifest)
	if !bytes.Equal(before, after) {
		t.Fatal("rejected Editor profile still modified authored package manifest")
	}
}

func TestAgentSnapshotDetectsMetadataChangesAdditionsAndSymlinks(t *testing.T) {
	for _, change := range []string{"metadata", "addition", "symlink"} {
		t.Run(change, func(t *testing.T) {
			project := agentReviewProject(t)
			snapshot, err := snapshotAgentSource(context.Background(), project)
			if err != nil {
				t.Fatal(err)
			}
			switch change {
			case "metadata":
				if err := os.WriteFile(filepath.Join(project, "Assets/Sample.cs.meta"), []byte("guid: 33333333333333333333333333333333\n"), 0600); err != nil {
					t.Fatal(err)
				}
			case "addition":
				if err := os.WriteFile(filepath.Join(project, "Assets/new.bin"), []byte{1, 2, 3}, 0600); err != nil {
					t.Fatal(err)
				}
			case "symlink":
				if err := os.Symlink(t.TempDir(), filepath.Join(project, "Assets/escaped")); err != nil {
					t.Fatal(err)
				}
			}
			if err := verifyAgentSnapshot(context.Background(), snapshot); err == nil {
				t.Fatal("changed snapshot accepted")
			}
		})
	}
}

func TestAgentCachedCloneRefusesSubstitutedProjectSymlink(t *testing.T) {
	project := agentReviewProject(t)
	directory := filepath.Join(project, "Library/UnityMicroEditor/stages/symlink")
	snapshot, err := snapshotAgentSource(context.Background(), project)
	if err != nil {
		t.Fatal(err)
	}
	if err := agentImmutableJSON(filepath.Join(directory, "clone-complete.json"), snapshot); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(t.TempDir(), filepath.Join(directory, "project")); err != nil {
		t.Fatal(err)
	}
	if _, _, err := cloneAgentStaging(context.Background(), project, directory); err == nil {
		t.Fatal("cached project symlink reached launch boundary")
	}
}

func TestAgentCloneRebindsItsOwnedMarkerWithoutChangingSource(t *testing.T) {
	project := agentReviewProject(t)
	marker := filepath.Join(project, "ProjectSettings/UnityMicroEditorStaging.json")
	if err := agentImmutableJSON(marker, map[string]any{"schemaVersion": 1, "sourceProject": t.TempDir()}); err != nil {
		t.Fatal(err)
	}
	before, _ := os.ReadFile(marker)
	stage, _, err := cloneAgentStaging(context.Background(), project, filepath.Join(project, "Library/UnityMicroEditor/clone-marker-test"))
	if err != nil {
		t.Fatal(err)
	}
	var stagedMarker struct {
		SchemaVersion int    `json:"schemaVersion"`
		SourceProject string `json:"sourceProject"`
	}
	if err := agentReadJSON(filepath.Join(stage, "ProjectSettings/UnityMicroEditorStaging.json"), 1<<20, &stagedMarker); err != nil || stagedMarker.SchemaVersion != 1 || stagedMarker.SourceProject != project {
		t.Fatalf("new shadow identity not bound to source: %+v %v", stagedMarker, err)
	}
	after, _ := os.ReadFile(marker)
	if !bytes.Equal(before, after) {
		t.Fatal("shadow planning changed source staging identity")
	}
}

func TestAgentCanceledQueuedActionDoesNotExecute(t *testing.T) {
	service := &agentProjectService{project: agentReviewProject(t)}
	service.mu.Lock()
	defer service.mu.Unlock()
	ctx, cancel := context.WithTimeout(context.Background(), 50*time.Millisecond)
	defer cancel()
	var executed atomic.Bool
	done := make(chan error, 1)
	go func() {
		_, err := service.locked(ctx, func() (any, error) { executed.Store(true); return nil, nil })
		done <- err
	}()
	select {
	case err := <-done:
		if !errors.Is(err, context.DeadlineExceeded) {
			t.Fatalf("wrong cancellation: %v", err)
		}
	case <-time.After(500 * time.Millisecond):
		t.Fatal("queued action ignored caller cancellation")
	}
	if executed.Load() {
		t.Fatal("canceled action executed work")
	}
}

func TestAgentStagedPublicationReplaysAfterCoordinatorRestart(t *testing.T) {
	project := agentReviewProject(t)
	store, err := authoring.NewStore(project)
	if err != nil {
		t.Fatal(err)
	}
	first, err := store.Read(context.Background(), "Assets/Sample.cs")
	if err != nil {
		t.Fatal(err)
	}
	second, err := store.Read(context.Background(), "Assets/Other.cs")
	if err != nil {
		t.Fatal(err)
	}
	id := "staged-crash-test"
	args := map[string]any{"operationId": id, "action": "revert", "path": first.Path, "expectedRevision": first.Revision}
	directory := filepath.Join(project, "Library/UnityMicroEditor/authoring-stages", bytesSHA([]byte(id)))
	if err := agentImmutableJSON(filepath.Join(directory, "request.json"), map[string]any{"schemaVersion": 1, "project": project, "arguments": args}); err != nil {
		t.Fatal(err)
	}
	plan := authoring.StagedPublicationRequest{OperationID: id, Inputs: []authoring.PublicationInput{{Path: first.Path, ExpectedRevision: first.Revision}, {Path: second.Path, ExpectedRevision: second.Revision}}, Outputs: []authoring.PublicationOutput{{Path: first.Path, Content: strings.Replace(first.Content, "= 1", "= 3", 1)}, {Path: second.Path, Content: strings.Replace(second.Content, "= 2", "= 4", 1)}}}
	if err := agentImmutableJSON(filepath.Join(directory, "publication.json"), plan); err != nil {
		t.Fatal(err)
	}
	raw, _ := json.Marshal(args)
	service := &agentProjectService{project: project, lifetime: context.Background()}
	receipt, err := service.Handle(context.Background(), "prefab_edit", raw)
	if err != nil {
		t.Fatal(err)
	}
	publication := receipt.(map[string]any)["publication"].(authoring.PublicationResult)
	if !publication.Changed || publication.Replayed || len(publication.Assets) != 2 {
		t.Fatalf("publication %+v", publication)
	}
	restarted := &agentProjectService{project: project, lifetime: context.Background()}
	receipt, err = restarted.Handle(context.Background(), "prefab_edit", raw)
	if err != nil {
		t.Fatal(err)
	}
	if !receipt.(map[string]any)["publication"].(authoring.PublicationResult).Replayed {
		t.Fatal("restart did not recover published receipt")
	}
	for _, source := range []authoring.Source{first, second} {
		updated, err := store.Read(context.Background(), source.Path)
		if err != nil || updated.Metadata != source.Metadata {
			t.Fatal("staged publication changed metadata")
		}
	}
	args["action"] = "unpack"
	changed, _ := json.Marshal(args)
	var conflict *authoring.Error
	if _, err := restarted.Handle(context.Background(), "prefab_edit", changed); !errors.As(err, &conflict) || conflict.Code != "request_id_conflict" {
		t.Fatalf("changed staged arguments reused receipt: %v", err)
	}
}

func TestAgentStagedPublicationPreservesExactRequestNumbersAcrossRestart(t *testing.T) {
	project := agentReviewProject(t)
	store, err := authoring.NewStore(project)
	if err != nil {
		t.Fatal(err)
	}
	source, err := store.Read(context.Background(), "Assets/Sample.cs")
	if err != nil {
		t.Fatal(err)
	}
	id := "staged-exact-int64"
	args := map[string]any{"operationId": id, "action": "set_override", "path": source.Path, "expectedRevision": source.Revision,
		"value": json.Number("9223372036854775807"), "fields": map[string]any{"ids": []any{json.Number("9007199254740993"), json.Number("-9223372036854775808")}, "enabled": true}}
	directory := filepath.Join(project, "Library/UnityMicroEditor/authoring-stages", bytesSHA([]byte(id)))
	if err := agentImmutableJSON(filepath.Join(directory, "request.json"), map[string]any{"schemaVersion": 1, "project": project, "arguments": args}); err != nil {
		t.Fatal(err)
	}
	plan := authoring.StagedPublicationRequest{OperationID: id, Inputs: []authoring.PublicationInput{{Path: source.Path, ExpectedRevision: source.Revision}},
		Outputs: []authoring.PublicationOutput{{Path: source.Path, Content: "public class Sample { public long value = 9223372036854775807; }\n"}}}
	if err := agentImmutableJSON(filepath.Join(directory, "publication.json"), plan); err != nil {
		t.Fatal(err)
	}
	raw, _ := json.Marshal(args)
	for iteration := 0; iteration < 2; iteration++ {
		service := &agentProjectService{project: project, lifetime: context.Background()}
		result, err := service.HandleSync(context.Background(), "prefab_edit", raw)
		service.Close()
		if err != nil {
			t.Fatalf("exact staged request iteration %d: %v", iteration, err)
		}
		publication := result.(map[string]any)["publication"].(authoring.PublicationResult)
		if publication.Replayed != (iteration == 1) {
			t.Fatal("durable replay was not retained", publication)
		}
	}
	updated, err := store.Read(context.Background(), source.Path)
	if err != nil || updated.Content != plan.Outputs[0].Content || updated.Metadata != source.Metadata {
		t.Fatal("exact long/source metadata changed", err)
	}
	args["value"] = json.Number("9223372036854775806")
	changed, _ := json.Marshal(args)
	service := &agentProjectService{project: project, lifetime: context.Background()}
	defer service.Close()
	var conflict *authoring.Error
	if _, err := service.HandleSync(context.Background(), "prefab_edit", changed); !errors.As(err, &conflict) || conflict.Code != "request_id_conflict" {
		t.Fatalf("neighboring 64-bit integer reused durable receipt: %v", err)
	}
	for _, invalid := range []json.RawMessage{json.RawMessage(`{} {}`), json.RawMessage(`[]`), json.RawMessage(`null`)} {
		if _, err := service.HandleSync(context.Background(), "prefab_edit", invalid); !errors.As(err, &conflict) || conflict.Code != "invalid_request" {
			t.Fatalf("invalid JSON object accepted: %s: %v", invalid, err)
		}
	}
}

func TestAgentConfigurePreparedRejectsEscapesAndKeepsLastAcceptedBaseline(t *testing.T) {
	project := agentReviewProject(t)
	lifetime, cancel := context.WithCancel(context.Background())
	controller, err := agentcontrol.New(lifetime, agentcontrol.Options{Project: project, Runner: "/bin/true"})
	if err != nil {
		cancel()
		t.Fatal(err)
	}
	defer controller.Close()
	defer cancel()
	baseline := filepath.Join(project, "Library/UnityMicroEditor/Preparation/current.json")
	if err := os.MkdirAll(filepath.Dir(baseline), 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(baseline, []byte("{}\n"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := controller.ConfigurePrepared(baseline, "Assets/Main.unity"); err != nil {
		t.Fatal(err)
	}
	for _, invalid := range []struct{ baseline, scene string }{{filepath.Join(t.TempDir(), "current.json"), "Assets/Main.unity"}, {baseline, "Assets/../Packages/Main.unity"}, {baseline, "Assets/Main.prefab"}} {
		if err := controller.ConfigurePrepared(invalid.baseline, invalid.scene); err == nil {
			t.Fatalf("invalid preparation accepted %+v", invalid)
		}
	}
	alias := filepath.Join(project, "Library/UnityMicroEditor/Preparation/alias.json")
	if err := os.Symlink(baseline, alias); err != nil {
		t.Fatal(err)
	}
	if err := controller.ConfigurePrepared(alias, "Assets/Main.unity"); err == nil {
		t.Fatal("prepared symlink accepted")
	}
}

func TestAgentRuntimeStartCannotInterleavePreparationActivation(t *testing.T) {
	for _, async := range []bool{false, true} {
		t.Run(fmt.Sprintf("async=%t", async), func(t *testing.T) {
			project := agentReviewProject(t)
			runner := filepath.Join(t.TempDir(), "waiting-runner")
			if err := os.WriteFile(runner, []byte("#!/bin/sh\nexec sleep 30\n"), 0700); err != nil {
				t.Fatal(err)
			}
			controller, err := agentcontrol.New(context.Background(), agentcontrol.Options{Project: project, Runner: runner})
			if err != nil {
				t.Fatal(err)
			}
			defer controller.Close()
			oldBaseline := filepath.Join(project, "Library/UnityMicroEditor/preparations/old/current.json")
			newBaseline := filepath.Join(project, "Library/UnityMicroEditor/preparations/new/current.json")
			for _, baseline := range []string{oldBaseline, newBaseline} {
				if err := agentImmutableJSON(baseline, map[string]any{"project": project}); err != nil {
					t.Fatal(err)
				}
			}
			if err := controller.ConfigurePrepared(oldBaseline, "Assets/Main.unity"); err != nil {
				t.Fatal(err)
			}
			if _, err := controller.Start(); err != nil {
				t.Fatal(err)
			}
			service := &agentProjectService{project: project, scene: "Assets/Main.unity", baseline: oldBaseline, controller: controller, Async: async, lifetime: context.Background()}
			defer service.Close()
			done := make(chan error, 1)
			_, err = service.locked(context.Background(), func() (any, error) {
				if _, err := controller.Stop(context.Background()); err != nil {
					return nil, err
				}
				started := make(chan struct{})
				go func() {
					close(started)
					_, err := service.Runtime(context.Background(), "session_control", json.RawMessage(`{"action":"start"}`))
					done <- err
				}()
				<-started
				select {
				case err := <-done:
					return nil, fmt.Errorf("runtime start entered Stop/Configure gap: %v", err)
				case <-time.After(100 * time.Millisecond):
				}
				if err := controller.ConfigurePrepared(newBaseline, "Assets/Main.unity"); err != nil {
					return nil, err
				}
				service.stateMu.Lock()
				service.baseline = newBaseline
				service.stateMu.Unlock()
				return controller.Start()
			})
			if err != nil {
				t.Fatal(err)
			}
			select {
			case err := <-done:
				if err != nil {
					t.Fatal(err)
				}
			case <-time.After(time.Second):
				t.Fatal("runtime start remained blocked after activation completed")
			}
		})
	}
}

func TestAgentCachedPreparationValidatesIdentityAndBaselineBeforeActivation(t *testing.T) {
	for _, changed := range []string{"valid", "project", "scene", "requestId", "foreign-baseline", "baseline-symlink", "baseline-directory", "malformed-baseline", "baseline-project", "manifest-foreign", "manifest-missing", "manifest-unsupported", "manifest-scene", "player-missing", "player-symlink"} {
		t.Run(changed, func(t *testing.T) {
			project := agentReviewProject(t)
			id := "cached-preparation"
			directory := filepath.Join(project, "Library/UnityMicroEditor/preparations", id)
			baseline := filepath.Join(directory, "current.json")
			result := agentPreparation{SchemaVersion: 1, Project: project, Scene: "Assets/Main.unity", RequestID: id, Baseline: baseline}
			manifest := filepath.Join(directory, "source/scene.json")
			player := filepath.Join(directory, "Player/Project.x86_64")
			manifestValue := map[string]any{"schemaVersion": 1, "projectRoot": project, "scenePath": result.Scene, "unityVersion": fixtureUnityVersion, "supported": true}
			if err := agentImmutableJSON(manifest, manifestValue); err != nil {
				t.Fatal(err)
			}
			if err := os.MkdirAll(filepath.Dir(player), 0700); err != nil {
				t.Fatal(err)
			}
			if err := os.WriteFile(player, []byte("prepared native Player"), 0700); err != nil {
				t.Fatal(err)
			}
			baselineValue := map[string]any{"schemaVersion": 1, "project": project, "manifest": manifest, "player": player,
				"compiler": goCompileManifest{Schema: 1, Project: project, Unity: "/missing-editor", CompilerInputs: map[string]string{"public-input": "verified"}}}
			if err := agentImmutableJSON(baseline, baselineValue); err != nil {
				t.Fatal(err)
			}
			switch changed {
			case "project":
				result.Project = t.TempDir()
			case "scene":
				result.Scene = "Assets/Other.unity"
			case "requestId":
				result.RequestID = "another-preparation"
			case "foreign-baseline":
				result.Baseline = filepath.Join(t.TempDir(), "current.json")
			case "baseline-symlink":
				outside := filepath.Join(t.TempDir(), "current.json")
				if err := os.WriteFile(outside, []byte("{}"), 0600); err != nil {
					t.Fatal(err)
				}
				if err := os.Remove(baseline); err != nil {
					t.Fatal(err)
				}
				if err := os.Symlink(outside, baseline); err != nil {
					t.Fatal(err)
				}
			case "baseline-directory":
				if err := os.Remove(baseline); err != nil {
					t.Fatal(err)
				}
				if err := os.Mkdir(baseline, 0700); err != nil {
					t.Fatal(err)
				}
			case "malformed-baseline":
				if err := os.WriteFile(baseline, []byte("{broken"), 0600); err != nil {
					t.Fatal(err)
				}
			case "baseline-project", "manifest-foreign":
				if changed == "baseline-project" {
					baselineValue["project"] = t.TempDir()
				} else {
					baselineValue["manifest"] = filepath.Join(t.TempDir(), "scene.json")
				}
				data, _ := json.Marshal(baselineValue)
				if err := os.WriteFile(baseline, data, 0600); err != nil {
					t.Fatal(err)
				}
			case "manifest-missing":
				if err := os.Remove(manifest); err != nil {
					t.Fatal(err)
				}
			case "manifest-unsupported", "manifest-scene":
				if changed == "manifest-unsupported" {
					manifestValue["supported"] = false
				} else {
					manifestValue["scenePath"] = "Assets/Other.unity"
				}
				data, _ := json.Marshal(manifestValue)
				if err := os.WriteFile(manifest, data, 0600); err != nil {
					t.Fatal(err)
				}
			case "player-missing", "player-symlink":
				if err := os.Remove(player); err != nil {
					t.Fatal(err)
				}
				if changed == "player-symlink" {
					if err := os.Symlink("/bin/true", player); err != nil {
						t.Fatal(err)
					}
				}
			}
			if err := agentImmutableJSON(filepath.Join(directory, "result.json"), result); err != nil {
				t.Fatal(err)
			}
			runner := filepath.Join(t.TempDir(), "waiting-runner")
			if err := os.WriteFile(runner, []byte("#!/bin/sh\nexec sleep 30\n"), 0700); err != nil {
				t.Fatal(err)
			}
			controller, err := agentcontrol.New(context.Background(), agentcontrol.Options{Project: project, Runner: runner})
			if err != nil {
				t.Fatal(err)
			}
			defer controller.Close()
			oldBaseline := filepath.Join(project, "Library/UnityMicroEditor/accepted/current.json")
			if err := agentImmutableJSON(oldBaseline, map[string]any{"project": project}); err != nil {
				t.Fatal(err)
			}
			if err := controller.ConfigurePrepared(oldBaseline, "Assets/Main.unity"); err != nil {
				t.Fatal(err)
			}
			if _, err := controller.Start(); err != nil {
				t.Fatal(err)
			}
			service := &agentProjectService{project: project, editor: "/missing-editor", baseline: oldBaseline, controller: controller, lifetime: context.Background()}
			defer service.Close()
			before := controller.Status()
			raw, _ := json.Marshal(map[string]any{"scene": "Assets/Main.unity", "requestId": id})
			_, err = service.HandleSync(context.Background(), "project_prepare", raw)
			if changed == "valid" {
				if err != nil || service.baseline != baseline {
					t.Fatal("valid cached preparation required Editor rebuild or failed activation", err)
				}
			} else {
				if err == nil {
					t.Fatal("invalid cached preparation reached activation")
				}
				if service.baseline != oldBaseline || controller.Status().SessionID != before.SessionID || controller.Status().Phase != before.Phase || controller.Status().LastSequence != before.LastSequence {
					t.Fatal("invalid cached preparation mutated controller selection")
				}
			}
		})
	}
}

func TestAgentProjectBuildBackendArgumentsDefaultToRelease(t *testing.T) {
	for _, raw := range []string{`{"scenes":["Assets/Main.unity"]}`, `{"scenes":["Assets/Main.unity"],"development":false}`, `{"scenes":["Assets/Main.unity"],"development":true}`} {
		var input map[string]any
		if err := json.Unmarshal([]byte(raw), &input); err != nil {
			t.Fatal(err)
		}
		encoded, err := agentProjectBuildArguments(input)
		if err != nil {
			t.Fatal(err)
		}
		var actual map[string]json.RawMessage
		if err := json.Unmarshal(encoded, &actual); err != nil {
			t.Fatal(err)
		}
		want := "false"
		if input["development"] == true {
			want = "true"
		}
		if string(actual["development"]) != want || string(actual["scenes"]) != `["Assets/Main.unity"]` {
			t.Fatalf("public backend build arguments %s", encoded)
		}
	}
	for _, value := range []any{nil, "false", float64(0)} {
		if _, err := agentProjectBuildArguments(map[string]any{"development": value}); err == nil {
			t.Fatalf("invalid build boolean accepted: %v", value)
		}
	}
}
