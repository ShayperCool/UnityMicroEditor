package main

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"testing"
	"time"

	"unitymicroeditor/worker/internal/agentcontrol"
)

func TestAgentConfigurationRevisionTracksCompilerInputs(t *testing.T) {
	project := agentReviewProject(t)
	original, err := agentConfigurationRevision(project)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(project, "Assets/Main.unity"), []byte("scene content"), 0600); err != nil {
		t.Fatal(err)
	}
	same, err := agentConfigurationRevision(project)
	if err != nil || same != original {
		t.Fatal("scene changes unexpectedly changed the managed configuration identity", err)
	}
	for _, path := range []string{"Assets/Sample.cs", "Assets/Sample.cs.meta", "Assets/Gameplay.asmdef", "Packages/manifest.json", "ProjectSettings/ProjectVersion.txt"} {
		before, err := agentConfigurationRevision(project)
		if err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(project, path), []byte("changed compiler input "+path), 0600); err != nil {
			t.Fatal(err)
		}
		after, err := agentConfigurationRevision(project)
		if err != nil || after == before {
			t.Fatalf("configuration ignored %s: %v", path, err)
		}
	}
	if err := os.Symlink(t.TempDir(), filepath.Join(project, "Assets/linked-sources")); err != nil {
		t.Fatal(err)
	}
	if _, err := agentConfigurationRevision(project); err == nil {
		t.Fatal("configuration silently skipped linked source directory")
	}
}

func TestAgentConfigurationRecoveryQueuesPreparationForRejectedGraph(t *testing.T) {
	project := agentReviewProject(t)
	if err := os.WriteFile(filepath.Join(project, "Assets/Main.unity"), []byte("selected scene"), 0600); err != nil {
		t.Fatal(err)
	}
	service := &agentProjectService{project: project, scene: "Assets/Main.unity", Async: true, lifetime: context.Background()}
	// Block the coordinator while checking the externally visible pending receipt.
	service.mu.Lock()
	defer func() { service.closeActionJobs(); service.mu.Unlock(); service.Close() }()
	rejected := agentcontrol.State{Phase: "ready", PID: 1234, Revision: "old-generation", UpdateError: "managed assembly configuration/GUID changed; regenerate source graph: Assets/Gameplay.asmdef"}
	result, err := service.recoverConfiguration(context.Background(), "session_control", json.RawMessage(`{"action":"wait_update"}`), rejected, errors.New(rejected.UpdateError))
	if err != nil {
		t.Fatal(err)
	}
	pending := result.(map[string]any)
	if pending["status"] != "result_pending" || pending["phase"] != "preparing_configuration" || pending["currentPlayer"].(agentcontrol.State).PID != rejected.PID {
		t.Fatal("recovery lost the running Player or pending state", pending)
	}
	job := pending["preparation"].(map[string]any)
	if job["status"] != "result_pending" {
		t.Fatal("recovery did not enqueue preparation")
	}
	var request agentActionJobRequest
	if err := agentReadJSON(filepath.Join(filepath.Dir(job["jobStatusPath"].(string)), "request.json"), 1<<20, &request); err != nil {
		t.Fatal(err)
	}
	var args map[string]any
	if json.Unmarshal(request.Arguments, &args) != nil || args["scene"] != "Assets/Main.unity" || args["activate"] != true || request.Action != "project_prepare" {
		t.Fatal("automatic preparation changed the scene or did not request activation")
	}
	repeated, err := service.recoverConfiguration(context.Background(), "session_control", json.RawMessage(`{"action":"wait_update"}`), rejected, errors.New(rejected.UpdateError))
	if err != nil || repeated.(map[string]any)["preparation"].(map[string]any)["requestId"] != job["requestId"] {
		t.Fatal("poll changed preparation identity", err)
	}
}

func agentConfigurationFixture(t *testing.T) (string, agentConfigurationSelection, string, string) {
	t.Helper()
	project := agentReviewProject(t)
	packageRoot := t.TempDir()
	input := filepath.Join(t.TempDir(), "public-plugin.dll")
	for path, contents := range map[string]string{
		filepath.Join(project, "Assets/Main.unity"):        "selected scene",
		filepath.Join(project, "Assets/Main.unity.meta"):   "guid: 33333333333333333333333333333333\n",
		filepath.Join(packageRoot, "package.json"):         `{"name":"com.example.external"}`,
		filepath.Join(packageRoot, "External.cs"):          "public class External {}",
		filepath.Join(packageRoot, "External.asmdef"):      `{"name":"External"}`,
		filepath.Join(packageRoot, "External.asmdef.meta"): "guid: 44444444444444444444444444444444\n",
		input: "public compiled plugin",
	} {
		if err := os.WriteFile(path, []byte(contents), 0600); err != nil {
			t.Fatal(err)
		}
	}
	baseline := filepath.Join(project, "Library/UnityMicroEditor/preparations/initial/current.json")
	inputs := map[string]string{input: "previous", filepath.Join(packageRoot, "External.asmdef"): "previous"}
	if err := agentImmutableJSON(baseline, map[string]any{"project": project, "compiler": goCompileManifest{Project: project, CompilerInputs: inputs}}); err != nil {
		t.Fatal(err)
	}
	graph := goCompilerGraphExport{SchemaVersion: 1, Project: project, CompilerInputs: inputs,
		SourceRoots: []goCompilerSourceRoot{{Path: "Packages/com.example.external", Source: packageRoot}}}
	if err := agentImmutableJSON(filepath.Join(filepath.Dir(baseline), "graph.json"), graph); err != nil {
		t.Fatal(err)
	}
	return project, agentConfigurationSelection{Scene: "Assets/Main.unity", Baseline: baseline}, packageRoot, input
}

func TestAgentConfigurationRevisionBindsSceneAndRegisteredPublicInputs(t *testing.T) {
	project, selection, packageRoot, input := agentConfigurationFixture(t)
	revision := func() string {
		t.Helper()
		value, err := agentConfigurationRevision(project, selection)
		if err != nil {
			t.Fatal(err)
		}
		return value
	}
	original := revision()
	private := filepath.Join(project, "Library/UnityMicroEditor/compiler-cache/unregistered.dll")
	if err := os.MkdirAll(filepath.Dir(private), 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(private, []byte("cache output"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(project, "Assets/Other.unity"), []byte("unselected scene"), 0600); err != nil {
		t.Fatal(err)
	}
	if revision() != original {
		t.Fatal("unrelated scene or private cache changed recovery identity")
	}
	for _, path := range []string{
		filepath.Join(project, selection.Scene), filepath.Join(project, selection.Scene+".meta"),
		filepath.Join(packageRoot, "External.cs"), filepath.Join(packageRoot, "External.asmdef"), filepath.Join(packageRoot, "External.asmdef.meta"),
		filepath.Join(packageRoot, "new.rsp.meta"), filepath.Join(packageRoot, "new.cs"), input,
	} {
		before := revision()
		if err := os.WriteFile(path, []byte("changed public configuration input "+path), 0600); err != nil {
			t.Fatal(err)
		}
		if revision() == before {
			t.Fatal("recovery ignored public input", path)
		}
	}
	before := revision()
	if err := os.Remove(input); err != nil {
		t.Fatal(err)
	}
	if revision() == before {
		t.Fatal("removing a declared compile input did not change recovery identity")
	}
	before = revision()
	if err := os.RemoveAll(packageRoot); err != nil {
		t.Fatal(err)
	}
	if revision() == before {
		t.Fatal("removing a registered package did not change recovery identity")
	}
}

func TestAgentConfigurationRevisionRejectsSourceAliasesAndForeignGraph(t *testing.T) {
	for _, invalid := range []string{"package-symlink", "input-symlink", "private-root", "foreign-graph", "baseline-symlink"} {
		t.Run(invalid, func(t *testing.T) {
			project, selection, packageRoot, input := agentConfigurationFixture(t)
			switch invalid {
			case "package-symlink":
				if err := os.Symlink(t.TempDir(), filepath.Join(packageRoot, "alias")); err != nil {
					t.Fatal(err)
				}
			case "input-symlink":
				if err := os.Remove(input); err != nil {
					t.Fatal(err)
				}
				if err := os.Symlink(filepath.Join(packageRoot, "External.cs"), input); err != nil {
					t.Fatal(err)
				}
			case "private-root", "foreign-graph":
				graphPath := filepath.Join(filepath.Dir(selection.Baseline), "graph.json")
				var graph goCompilerGraphExport
				if err := agentReadJSON(graphPath, 1<<20, &graph); err != nil {
					t.Fatal(err)
				}
				if invalid == "private-root" {
					graph.SourceRoots[0].Source = filepath.Join(project, "Library/UnityMicroEditor/private")
				} else {
					graph.Project = t.TempDir()
				}
				data, _ := json.Marshal(graph)
				if err := os.WriteFile(graphPath, data, 0600); err != nil {
					t.Fatal(err)
				}
			case "baseline-symlink":
				alias := filepath.Join(filepath.Dir(selection.Baseline), "alias.json")
				if err := os.Symlink(selection.Baseline, alias); err != nil {
					t.Fatal(err)
				}
				selection.Baseline = alias
			}
			if _, err := agentConfigurationRevision(project, selection); err == nil {
				t.Fatal("unsafe recovery input accepted")
			}
		})
	}
}

func TestAgentConfigurationRecoveryLeavesFailedJobWhenSceneOrExternalPackageChanges(t *testing.T) {
	for _, changed := range []string{"scene", "external-package"} {
		t.Run(changed, func(t *testing.T) {
			project, selection, packageRoot, _ := agentConfigurationFixture(t)
			service := &agentProjectService{project: project, scene: selection.Scene, baseline: selection.Baseline, Async: true, lifetime: context.Background()}
			defer service.Close()
			revision, err := agentConfigurationRevision(project, selection)
			if err != nil {
				t.Fatal(err)
			}
			id := "config-" + bytesSHA([]byte(selection.Scene + "\n" + revision))[:40]
			raw, _ := json.Marshal(map[string]any{"scene": selection.Scene, "requestId": id, "activate": true})
			run := func(context.Context, string, json.RawMessage) (any, error) {
				return nil, agentFailure("revision_conflict", "source changed during the previous preparation")
			}
			if _, err := service.enqueueAction(context.Background(), "project_prepare", raw, run); err != nil {
				t.Fatal(err)
			}
			deadline := time.Now().Add(time.Second)
			for {
				_, err := service.enqueueAction(context.Background(), "project_prepare", raw, run)
				if err != nil {
					break
				}
				if time.Now().After(deadline) {
					t.Fatal("failed preparation was not persisted")
				}
				time.Sleep(time.Millisecond)
			}
			rejected := agentcontrol.State{Phase: "ready", PID: 1234, Revision: "old-player", UpdateError: "managed configuration requires a fresh managed graph"}
			if _, err := service.recoverConfiguration(context.Background(), "session_control", json.RawMessage(`{"action":"wait_update"}`), rejected, errors.New(rejected.UpdateError)); err == nil {
				t.Fatal("durable failed job was unexpectedly rerun")
			}
			path := filepath.Join(project, selection.Scene)
			if changed == "external-package" {
				path = filepath.Join(packageRoot, "External.asmdef")
			}
			if err := os.WriteFile(path, []byte("repaired configuration"), 0600); err != nil {
				t.Fatal(err)
			}
			service.mu.Lock()
			defer func() { service.closeActionJobs(); service.mu.Unlock() }()
			result, err := service.recoverConfiguration(context.Background(), "session_control", json.RawMessage(`{"action":"wait_update"}`), rejected, errors.New(rejected.UpdateError))
			if err != nil {
				t.Fatal("changed configuration reused failed durable preparation", err)
			}
			pending := result.(map[string]any)
			job := pending["preparation"].(map[string]any)
			if job["requestId"] == id || job["status"] != "result_pending" || pending["currentPlayer"].(agentcontrol.State).PID != rejected.PID {
				t.Fatal("new recovery did not preserve old Player with a new pending identity", pending)
			}
		})
	}
}

func TestAgentConfigurationRecoveryPreservesOrdinaryCompileAndCancellationErrors(t *testing.T) {
	service := &agentProjectService{}
	for _, failure := range []error{errors.New("CS1002: semicolon expected"), context.Canceled, context.DeadlineExceeded} {
		state := agentcontrol.State{Phase: "ready", PID: 1234, UpdateError: failure.Error()}
		if failure == context.Canceled || failure == context.DeadlineExceeded {
			state.UpdateError = "configuration requires a fresh managed graph"
		}
		result, err := service.recoverConfiguration(context.Background(), "session_control", json.RawMessage(`{"action":"wait_update"}`), state, failure)
		if err != failure || result.(agentcontrol.State).PID != state.PID {
			t.Fatal("ordinary failure or canceled request was replaced by automatic preparation")
		}
	}
	if agentRequiresNewGraph("CS0246: type could not be found") {
		t.Fatal("ordinary C# error was classified as managed configuration change")
	}
}
