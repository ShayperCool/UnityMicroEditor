package main

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"sync/atomic"
	"testing"
	"time"

	"unitymicroeditor/internal/project/authoring"
	"unitymicroeditor/internal/agentcontrol"
)

func waitAgentJob(t *testing.T, service *agentProjectService, action string, raw json.RawMessage, run func(context.Context, string, json.RawMessage) (any, error)) (any, error) {
	t.Helper()
	deadline := time.Now().Add(3 * time.Second)
	for {
		result, err := service.enqueueAction(context.Background(), action, raw, run)
		if err != nil {
			return result, err
		}
		if _, pending := result.(map[string]any); !pending {
			return result, nil
		}
		if time.Now().After(deadline) {
			t.Fatal("job receipt did not become confirmed")
		}
		time.Sleep(5 * time.Millisecond)
	}
}

func TestAgentJobOutlivesRequestAndPersistsConfirmedReceipt(t *testing.T) {
	project := agentReviewProject(t)
	service := &agentProjectService{project: project, lifetime: context.Background()}
	defer service.Close()
	raw := json.RawMessage(`{"requestId":"detached-test","scene":"Assets/Main.unity"}`)
	started, release := make(chan struct{}), make(chan struct{})
	var executions atomic.Int32
	run := func(ctx context.Context, _ string, _ json.RawMessage) (any, error) {
		executions.Add(1)
		close(started)
		select {
		case <-release:
			return map[string]any{"baseline": "confirmed", "requestId": "detached-test"}, nil
		case <-ctx.Done():
			return nil, ctx.Err()
		}
	}
	request, cancel := context.WithTimeout(context.Background(), 50*time.Millisecond)
	before := time.Now()
	pending, err := service.enqueueAction(request, "project_prepare", raw, run)
	if err != nil || time.Since(before) > time.Second || pending.(map[string]any)["status"] != "result_pending" {
		t.Fatalf("enqueue blocked request: %+v %v", pending, err)
	}
	<-started
	cancel()
	stillPending, err := service.enqueueAction(context.Background(), "project_prepare", raw, run)
	if err != nil || stillPending.(map[string]any)["status"] != "result_pending" {
		t.Fatal("caller cancellation stopped detached action")
	}
	close(release)
	completed, err := waitAgentJob(t, service, "project_prepare", raw, run)
	if err != nil || executions.Load() != 1 {
		t.Fatalf("job ran %d times: %v", executions.Load(), err)
	}
	var data map[string]any
	if err := json.Unmarshal(completed.(json.RawMessage), &data); err != nil || data["baseline"] != "confirmed" {
		t.Fatal("receipt lost confirmed result")
	}
	restarted := &agentProjectService{project: project, lifetime: context.Background()}
	defer restarted.Close()
	completed, err = restarted.enqueueAction(context.Background(), "project_prepare", raw, func(context.Context, string, json.RawMessage) (any, error) {
		t.Error("confirmed job executed after restart")
		return nil, nil
	})
	if err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal(completed.(json.RawMessage), &data); err != nil || data["baseline"] != "confirmed" {
		t.Fatal("restart receipt changed")
	}
	var conflict *authoring.Error
	if _, err := restarted.enqueueAction(context.Background(), "project_prepare", json.RawMessage(`{"requestId":"detached-test","scene":"Assets/Changed.unity"}`), run); !errors.As(err, &conflict) || conflict.Code != "request_id_conflict" {
		t.Fatalf("changed arguments accepted: %v", err)
	}
}

func TestAgentJobKnownFailureAndUnknownOutcomeAreNeverReexecuted(t *testing.T) {
	for _, code := range []string{"compile_rejected", "result_unknown"} {
		t.Run(code, func(t *testing.T) {
			project := agentReviewProject(t)
			service := &agentProjectService{project: project, lifetime: context.Background()}
			defer service.Close()
			raw, _ := json.Marshal(map[string]any{"requestId": "failed-test", "scene": "Assets/Main.unity"})
			var executions atomic.Int32
			run := func(context.Context, string, json.RawMessage) (any, error) {
				executions.Add(1)
				return nil, &authoring.Error{Code: code, Message: "observed backend failure", Details: map[string]any{"requestId": "phase-proof"}}
			}
			_, err := waitAgentJob(t, service, "project_prepare", raw, run)
			var detail *authoring.Error
			if !errors.As(err, &detail) || detail.Code != code || executions.Load() != 1 {
				t.Fatalf("failure receipt lost: %v", err)
			}
			restarted := &agentProjectService{project: project, lifetime: context.Background()}
			defer restarted.Close()
			_, err = restarted.enqueueAction(context.Background(), "project_prepare", raw, run)
			if !errors.As(err, &detail) || detail.Code != code || executions.Load() != 1 || detail.Details["requestId"] != "phase-proof" {
				t.Fatalf("restart reran unknown/failure: %v executions %d", err, executions.Load())
			}
		})
	}
}

func TestAgentPendingJobCanPollSameBackendRequestAfterRestart(t *testing.T) {
	project := agentReviewProject(t)
	service := &agentProjectService{project: project, lifetime: context.Background()}
	raw := json.RawMessage(`{"operationId":"pending-prefab","path":"Assets/Main.unity","expectedRevision":"original"}`)
	invoked := make(chan struct{})
	_, err := service.enqueueAction(context.Background(), "prefab_edit", raw, func(context.Context, string, json.RawMessage) (any, error) {
		close(invoked)
		return nil, &authoring.Error{Code: "result_pending", Message: "accepted Editor receipt is still pending"}
	})
	if err != nil {
		t.Fatal(err)
	}
	<-invoked
	service.Close()
	resultPath := filepath.Join(project, "Library/UnityMicroEditor/action-jobs", bytesSHA([]byte("pending-prefab")), "result.json")
	if _, err := os.Stat(resultPath); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("unconfirmed outcome was persisted as final failure")
	}
	restarted := &agentProjectService{project: project, lifetime: context.Background()}
	defer restarted.Close()
	var request map[string]json.RawMessage
	json.Unmarshal(raw, &request)
	normalized, _ := json.Marshal(request)
	var executions atomic.Int32
	completed, err := waitAgentJob(t, restarted, "prefab_edit", raw, func(_ context.Context, _ string, retried json.RawMessage) (any, error) {
		executions.Add(1)
		if string(retried) != string(normalized) {
			t.Error("recovery changed operation identity or guards")
		}
		return map[string]any{"operationId": "pending-prefab", "confirmed": true}, nil
	})
	if err != nil || executions.Load() != 1 || len(completed.(json.RawMessage)) == 0 {
		t.Fatal("same request did not recover confirmed receipt")
	}
}

func TestAgentCloseCancelsDetachedWorkBeforeTakingServiceLock(t *testing.T) {
	service := &agentProjectService{project: agentReviewProject(t), lifetime: context.Background()}
	entered := make(chan struct{})
	_, err := service.enqueueAction(context.Background(), "project_build", json.RawMessage(`{"requestId":"close-test","scenes":["Assets/Main.unity"]}`), func(ctx context.Context, _ string, _ json.RawMessage) (any, error) {
		return service.locked(ctx, func() (any, error) { close(entered); <-ctx.Done(); return nil, ctx.Err() })
	})
	if err != nil {
		t.Fatal(err)
	}
	<-entered
	done := make(chan struct{})
	go func() { service.Close(); close(done) }()
	select {
	case <-done:
	case <-time.After(time.Second):
		t.Fatal("Close waited on the action lock before canceling its job")
	}
	if _, err := service.enqueueAction(context.Background(), "project_build", json.RawMessage(`{"requestId":"closed","scenes":[]}`), nil); !errors.Is(err, context.Canceled) {
		t.Fatalf("closed service accepted job: %v", err)
	}
}

func TestAgentConfirmedPreparationRestoresActivationAfterRestart(t *testing.T) {
	project := agentReviewProject(t)
	preparation, _, _ := agentJobPreparationFixture(t, project, "restore-test")
	original := &agentProjectService{project: project, lifetime: context.Background()}
	raw := json.RawMessage(`{"requestId":"restore-test","scene":"Assets/Main.unity","activate":true}`)
	_, err := waitAgentJob(t, original, "project_prepare", raw, func(context.Context, string, json.RawMessage) (any, error) {
		return preparation, nil
	})
	original.Close()
	if err != nil {
		t.Fatal(err)
	}
	lifetime, cancel := context.WithCancel(context.Background())
	defer cancel()
	controller, err := agentcontrol.New(lifetime, agentcontrol.Options{Project: project, Runner: "/bin/true"})
	if err != nil {
		t.Fatal(err)
	}
	defer controller.Close()
	restarted := &agentProjectService{project: project, lifetime: lifetime, controller: controller, Async: true}
	defer restarted.Close()
	if _, err := restarted.Handle(context.Background(), "project_prepare", raw); err != nil {
		t.Fatal(err)
	}
	restarted.stateMu.RLock()
	active, scene := restarted.baseline, restarted.scene
	restarted.stateMu.RUnlock()
	if active != preparation.Baseline || scene != "Assets/Main.unity" {
		t.Fatal("confirmed preparation did not restore controller selection")
	}
}

func agentJobPreparationFixture(t *testing.T, project, id string) (agentPreparation, string, string) {
	t.Helper()
	directory := filepath.Join(project, "Library/UnityMicroEditor/preparations", id)
	preparation := agentPreparation{SchemaVersion: 1, Project: project, RequestID: id, Scene: "Assets/Main.unity", Baseline: filepath.Join(directory, "current.json")}
	manifest, player := filepath.Join(directory, "source/scene.json"), filepath.Join(directory, "Player/Project.x86_64")
	if err := agentImmutableJSON(manifest, map[string]any{"schemaVersion": 1, "projectRoot": project, "scenePath": preparation.Scene, "supported": true, "unityVersion": fixtureUnityVersion}); err != nil {
		t.Fatal(err)
	}
	if err := os.MkdirAll(filepath.Dir(player), 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(player, []byte("native Player"), 0700); err != nil {
		t.Fatal(err)
	}
	compiler := goCompileManifest{Schema: 1, Project: project, CompilerInputs: map[string]string{"public-input": "verified"}}
	if err := agentImmutableJSON(preparation.Baseline, map[string]any{"schemaVersion": 1, "project": project, "manifest": manifest, "player": player, "compiler": compiler}); err != nil {
		t.Fatal(err)
	}
	return preparation, manifest, player
}

func TestAgentPersistedPreparationRejectsCorruptFilesBeforeRestore(t *testing.T) {
	for _, corrupt := range []string{"baseline-json", "manifest-missing", "player-symlink"} {
		t.Run(corrupt, func(t *testing.T) {
			project := agentReviewProject(t)
			preparation, manifest, player := agentJobPreparationFixture(t, project, "restore-corrupt")
			raw := json.RawMessage(`{"requestId":"restore-corrupt","scene":"Assets/Main.unity","activate":true}`)
			original := &agentProjectService{project: project, lifetime: context.Background()}
			var executions atomic.Int32
			_, err := waitAgentJob(t, original, "project_prepare", raw, func(context.Context, string, json.RawMessage) (any, error) {
				executions.Add(1)
				return preparation, nil
			})
			original.Close()
			if err != nil {
				t.Fatal(err)
			}
			switch corrupt {
			case "baseline-json":
				if err := os.WriteFile(preparation.Baseline, []byte("{bad"), 0600); err != nil {
					t.Fatal(err)
				}
			case "manifest-missing":
				if err := os.Remove(manifest); err != nil {
					t.Fatal(err)
				}
			case "player-symlink":
				if err := os.Remove(player); err != nil {
					t.Fatal(err)
				}
				if err := os.Symlink("/bin/true", player); err != nil {
					t.Fatal(err)
				}
			}
			controller, err := agentcontrol.New(context.Background(), agentcontrol.Options{Project: project, Runner: "/bin/true"})
			if err != nil {
				t.Fatal(err)
			}
			defer controller.Close()
			restarted := &agentProjectService{project: project, lifetime: context.Background(), controller: controller, Async: true}
			defer restarted.Close()
			if _, err := restarted.Handle(context.Background(), "project_prepare", raw); err == nil {
				t.Fatal("corrupt confirmed preparation was activated")
			}
			if restarted.baseline != "" || controller.Status().Phase != "stopped" || executions.Load() != 1 {
				t.Fatal("restore mutated controller or reran confirmed work")
			}
		})
	}
}

func TestAgentStartDoesNotIgnoreDifferentExplicitScene(t *testing.T) {
	service := &agentProjectService{project: agentReviewProject(t), baseline: "/prepared/current.json", scene: "Assets/Main.unity"}
	for _, action := range []string{"start", "reset"} {
		raw, _ := json.Marshal(map[string]any{"action": action, "arguments": map[string]any{"scene": "Assets/Other.unity"}})
		_, err := service.runtimeWithActionJobs(context.Background(), "session_control", raw)
		var detail *authoring.Error
		if !errors.As(err, &detail) || detail.Code != "scene_requires_preparation" {
			t.Fatalf("%s silently ignored the selected scene: %v", action, err)
		}
	}
}
