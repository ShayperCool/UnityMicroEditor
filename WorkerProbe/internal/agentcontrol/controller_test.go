package agentcontrol

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"testing"
	"time"
	"unicode/utf8"
)

func helperWrite(path string, value any) error {
	if err := os.MkdirAll(filepath.Dir(path), 0700); err != nil {
		return err
	}
	data, err := json.Marshal(value)
	if err != nil {
		return err
	}
	temporary := path + ".temporary"
	if err := os.WriteFile(temporary, data, 0600); err != nil {
		return err
	}
	return os.Rename(temporary, path)
}

// This subprocess implements the runner/file protocol without Unity. Each
// fixture has its own project, output, process group, and request artifacts.
func TestControllerHelperProcess(t *testing.T) {
	if os.Getenv("UNITY_AGENT_CONTROL_HELPER") != "1" {
		return
	}
	project, output := "", ""
	for i, arg := range os.Args {
		if arg == "--project" && i+1 < len(os.Args) {
			project = os.Args[i+1]
		}
		if arg == "--output" && i+1 < len(os.Args) {
			output = os.Args[i+1]
		}
	}
	modeBytes, _ := os.ReadFile(filepath.Join(project, "helper-mode"))
	mode := string(modeBytes)
	signals := make(chan os.Signal, 4)
	signal.Notify(signals, syscall.SIGINT, syscall.SIGTERM)
	if mode == "child" {
		for {
			<-signals
			os.Exit(0)
		}
	}
	if mode == "overflow" {
		fmt.Print(strings.Repeat("x", 4*1024*1024+1))
		for {
			<-signals
		}
	}
	if mode == "truncated" {
		fmt.Print(`{"event":"ready"`)
		os.Exit(0)
	}
	if mode == "empty-exit" {
		os.Exit(0)
	}
	if mode == "stderr-exit" {
		fmt.Fprint(os.Stderr, "discarded-prefix-marker"+strings.Repeat("x", 40000)+"\n")
		os.Stderr.Write([]byte("native loader actual failure reason\x00 invalidUTF8:\xff\n"))
		os.Exit(1)
	}
	manifest := filepath.Join(project, "Temp/manifests/rev-1/scene.json")
	if mode == "library-manifest" {
		manifest = filepath.Join(project, "Library/UnityMicroEditor/manifests/rev-1/scene.json")
	} else if mode == "foreign-library-manifest" {
		manifest = filepath.Join(project, "Library/ScriptAssemblies/rev-1/scene.json")
	}
	report := filepath.Join(output, "runs/first/report.json")
	control := filepath.Join(output, "runs/first/control")
	if err := os.MkdirAll(control, 0700); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(4)
	}
	if err := helperWrite(manifest, map[string]any{"revision": "rev-1"}); err != nil {
		os.Exit(4)
	}
	pid := os.Getpid()
	var child *exec.Cmd
	if mode == "spawn-child" {
		childProject := filepath.Join(project, "child")
		os.MkdirAll(childProject, 0700)
		os.WriteFile(filepath.Join(childProject, "helper-mode"), []byte("child"), 0600)
		child = exec.Command(os.Args[0], "-test.run=^TestControllerHelperProcess$", "--", "--project", childProject)
		child.Env = os.Environ()
		child.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
		if err := child.Start(); err != nil {
			os.Exit(4)
		}
		pid = child.Process.Pid
		defer func() { child.Process.Kill(); child.Wait() }()
	}
	ready := map[string]any{"schemaVersion": 1, "pid": pid, "controlDirectory": control, "scene": map[string]any{"isLoaded": true}, "errors": 0, "exceptions": 0}
	if mode == "bad-pid" {
		ready["pid"] = pid + 100000
	}
	if mode == "symlink-control" {
		escaped := filepath.Join(project, "outside")
		os.MkdirAll(escaped, 0700)
		os.Remove(control)
		os.Symlink(escaped, control)
	}
	if err := helperWrite(report+".ready.json", ready); err != nil {
		os.Exit(4)
	}
	encoder := json.NewEncoder(os.Stdout)
	encoder.Encode(map[string]any{"event": "ready", "pid": pid, "manifest": manifest, "report": report})
	seen := map[string]bool{}
	pending := map[string]time.Time{}
	executions := 0
	lastEvent := ""
	for {
		select {
		case <-signals:
			if mode != "ignore-signals" {
				os.Exit(0)
			}
		default:
		}
		if data, err := os.ReadFile(filepath.Join(output, "helper-event.json")); err == nil && string(data) != lastEvent {
			lastEvent = string(data)
			var event map[string]any
			if json.Unmarshal(data, &event) == nil {
				if event["event"] == "crash" {
					os.Exit(7)
				}
				encoder.Encode(event)
			}
		}
		requests, _ := filepath.Glob(filepath.Join(control, "*.request.json"))
		for _, file := range requests {
			payload, err := os.ReadFile(file)
			if err != nil {
				continue
			}
			var request struct {
				ID        string         `json:"requestId"`
				PID       int            `json:"expectedPid"`
				Revision  string         `json:"expectedRevision"`
				Operation string         `json:"operation"`
				Arguments map[string]any `json:"arguments"`
			}
			if json.Unmarshal(payload, &request) != nil {
				continue
			}
			response := filepath.Join(control, request.ID+".response.json")
			if _, err := os.Stat(response); err == nil {
				continue
			}
			if !seen[request.ID] {
				seen[request.ID] = true
				executions++
				helperWrite(filepath.Join(output, "executions.json"), executions)
				if delay, _ := request.Arguments["delayMs"].(float64); delay > 0 {
					pending[request.ID] = time.Now().Add(time.Duration(delay) * time.Millisecond)
				}
			}
			if due, ok := pending[request.ID]; ok && time.Now().Before(due) {
				continue
			}
			if request.Arguments["dropResponse"] == true {
				continue
			}
			hash := sha256.Sum256(payload)
			hashText := hex.EncodeToString(hash[:])
			if request.Arguments["badHash"] == true {
				hashText = strings.Repeat("0", 64)
			}
			success := true
			var failure any
			if request.PID != pid {
				success = false
				failure = map[string]any{"code": "stale_process"}
			}
			if request.Operation != "status" && request.Revision != "rev-1" {
				success = false
				failure = map[string]any{"code": "stale_revision"}
			}
			revision := "rev-1"
			if value, ok := request.Arguments["responseRevision"].(string); ok {
				revision = value
			}
			helperWrite(response, map[string]any{"schemaVersion": 1, "requestId": request.ID, "requestSha256": hashText, "pid": pid, "revision": revision, "success": success, "result": map[string]any{"executions": executions}, "error": failure})
		}
		time.Sleep(5 * time.Millisecond)
	}
}

func controllerFixture(t *testing.T, mode string) *Controller {
	t.Helper()
	project := t.TempDir()
	if err := os.WriteFile(filepath.Join(project, "helper-mode"), []byte(mode), 0600); err != nil {
		t.Fatal(err)
	}
	binary, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	runner := filepath.Join(project, "runner")
	quoted := "'" + strings.ReplaceAll(binary, "'", "'\"'\"'") + "'"
	if err := os.WriteFile(runner, []byte("#!/bin/sh\nexec env UNITY_AGENT_CONTROL_HELPER=1 GORACE=atexit_sleep_ms=0 "+quoted+" '-test.run=^TestControllerHelperProcess$' -- \"$@\"\n"), 0700); err != nil {
		t.Fatal(err)
	}
	c, err := New(context.Background(), Options{Project: project, Runner: runner})
	if err != nil {
		t.Fatal(err)
	}
	c.stopGrace = 40 * time.Millisecond
	c.terminateGrace = 40 * time.Millisecond
	t.Cleanup(func() {
		if err := c.Close(); err != nil {
			t.Error(err)
		}
	})
	return c
}

func startReady(t *testing.T, c *Controller) State {
	t.Helper()
	if _, err := c.Start(); err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	state, err := c.Wait(ctx, 0, true)
	if err != nil {
		t.Fatalf("ready: %+v %v", state, err)
	}
	return state
}

func requestArgs(value map[string]any) map[string]json.RawMessage {
	result := map[string]json.RawMessage{}
	for key, item := range value {
		result[key], _ = json.Marshal(item)
	}
	return result
}

func emitFixtureEvent(t *testing.T, c *Controller, event map[string]any) {
	t.Helper()
	if err := helperWrite(filepath.Join(c.options.Output, "helper-event.json"), event); err != nil {
		t.Fatal(err)
	}
}

func TestControllerLifecycleAndLifetimeCancellation(t *testing.T) {
	c := controllerFixture(t, "")
	ready := startReady(t, c)
	if started, err := c.Start(); err != nil || started.SessionID != ready.SessionID || started.PID != ready.PID {
		t.Fatal("start duplicated an owned runner")
	}
	c.cancel()
	deadline := time.Now().Add(3 * time.Second)
	for c.Status().Phase != "stopped" && time.Now().Before(deadline) {
		time.Sleep(5 * time.Millisecond)
	}
	state := c.Status()
	if state.Phase != "stopped" || state.PID != 0 {
		t.Fatalf("cancelled controller retained process: %+v", state)
	}
	if err := syscall.Kill(ready.PID, 0); err != syscall.ESRCH {
		t.Fatalf("owned runner was not reaped: %v", err)
	}
	if _, err := c.Start(); err == nil {
		t.Fatal("closed lifetime allowed another process")
	}
}

func TestPreparedReplacementReadinessAcknowledgesOnlyConfigurationUpdate(t *testing.T) {
	c := controllerFixture(t, "")
	baseline := filepath.Join(c.options.Project, "Library/UnityMicroEditor/preparations/first/current.json")
	if err := os.MkdirAll(filepath.Dir(baseline), 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(baseline, []byte(`{}`), 0600); err != nil {
		t.Fatal(err)
	}
	if err := c.ConfigurePrepared(baseline, "Assets/Main.unity"); err != nil {
		t.Fatal(err)
	}
	initial := startReady(t, c)
	if initial.LastUpdateSequence != 0 {
		t.Fatal("initial readiness was reported as a source update")
	}
	emitFixtureEvent(t, c, map[string]any{"event": "update-rejected", "message": "regenerate source graph"})
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	rejected, err := c.Wait(ctx, initial.LastSequence, false)
	if err == nil {
		t.Fatal("expected graph rejection")
	}
	if _, err := c.Stop(ctx); err != nil {
		t.Fatal(err)
	}
	deleteEvent := filepath.Join(c.options.Output, "helper-event.json")
	if err := os.Remove(deleteEvent); err != nil && !errors.Is(err, os.ErrNotExist) {
		t.Fatal(err)
	}
	replacement := filepath.Join(c.options.Project, "Library/UnityMicroEditor/preparations/second/current.json")
	if err := os.MkdirAll(filepath.Dir(replacement), 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(replacement, []byte(`{}`), 0600); err != nil {
		t.Fatal(err)
	}
	if err := c.ConfigurePrepared(replacement, "Assets/Main.unity"); err != nil {
		t.Fatal(err)
	}
	if _, err := c.Start(); err != nil {
		t.Fatal(err)
	}
	confirmed, err := c.Wait(ctx, rejected.LastSequence, false)
	if err != nil || confirmed.Phase != "ready" || confirmed.PID == initial.PID || confirmed.LastUpdateSequence <= rejected.LastSequence || confirmed.UpdateError != "" {
		t.Fatalf("prepared replacement did not confirm an update: %+v %v", confirmed, err)
	}
	if confirmed.Events[len(confirmed.Events)-1].Data["preparationActivated"] != true {
		t.Fatal("configuration activation was not identified in the readiness event")
	}
	if _, err := c.Stop(ctx); err != nil {
		t.Fatal(err)
	}
	if _, err := c.Start(); err != nil {
		t.Fatal(err)
	}
	restarted, err := c.Wait(ctx, confirmed.LastSequence, true)
	if err != nil || restarted.LastUpdateSequence != 0 {
		t.Fatalf("ordinary restart manufactured a source update: %+v %v", restarted, err)
	}
}

func TestControllerStopContinuesCleanupAfterCallerTimeout(t *testing.T) {
	c := controllerFixture(t, "ignore-signals")
	ready := startReady(t, c)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Millisecond)
	defer cancel()
	if _, err := c.Stop(ctx); err != context.DeadlineExceeded {
		t.Fatalf("stop should return caller deadline while background cleanup continues: %v", err)
	}
	ctx2, cancel2 := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel2()
	if state, err := c.Stop(ctx2); err != nil || state.Phase != "stopped" {
		t.Fatalf("bounded force stop not reaped: %+v %v", state, err)
	}
	if err := syscall.Kill(ready.PID, 0); err != syscall.ESRCH {
		t.Fatalf("force-stopped runner remains: %v", err)
	}
}

func TestControllerWaitDistinguishesProgressCompletionAndRejection(t *testing.T) {
	c := controllerFixture(t, "")
	ready := startReady(t, c)
	emitFixtureEvent(t, c, map[string]any{"event": "compiling", "stage": "Roslyn"})
	ctx, cancel := context.WithTimeout(context.Background(), 80*time.Millisecond)
	if _, err := c.Wait(ctx, ready.LastSequence, false); err != context.DeadlineExceeded {
		t.Fatalf("progress falsely acknowledged update: %v", err)
	}
	cancel()
	emitFixtureEvent(t, c, map[string]any{"event": "update-rejected", "error": "CS1002: expected semicolon"})
	ctx, cancel = context.WithTimeout(context.Background(), time.Second)
	state, err := c.Wait(ctx, ready.LastSequence, false)
	cancel()
	if err == nil || !strings.Contains(err.Error(), "CS1002") || state.Phase != "ready" || state.PID != ready.PID {
		t.Fatalf("rejection lost accepted Player identity: %+v %v", state, err)
	}
	after := state.LastSequence
	emitFixtureEvent(t, c, map[string]any{"event": "updated", "ack": map[string]any{"success": true, "restartRequired": false, "loadedRevision": "rev-2", "pid": ready.PID}})
	ctx, cancel = context.WithTimeout(context.Background(), time.Second)
	state, err = c.Wait(ctx, after, false)
	cancel()
	if err != nil || state.Revision != "rev-2" || state.LastUpdateSequence <= after {
		t.Fatalf("confirmed update was not published: %+v %v", state, err)
	}
}

func TestControllerSourceUndoAcknowledgesAcceptedRevision(t *testing.T) {
	c := controllerFixture(t, "")
	ready := startReady(t, c)
	emitFixtureEvent(t, c, map[string]any{"event": "update-rejected", "error": "CS1002: expected semicolon"})
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	rejected, err := c.Wait(ctx, ready.LastSequence, false)
	cancel()
	if err == nil || rejected.UpdateError == "" {
		t.Fatalf("compile rejection was not observable: %+v %v", rejected, err)
	}
	// Restoring the accepted source bytes needs no reload or new readiness
	// artifact. The runner must still confirm which live revision it retained.
	emitFixtureEvent(t, c, map[string]any{"event": "unchanged", "pid": ready.PID, "revision": ready.Revision})
	ctx, cancel = context.WithTimeout(context.Background(), time.Second)
	recovered, err := c.Wait(ctx, rejected.LastSequence, false)
	cancel()
	if err != nil || recovered.Phase != "ready" || recovered.PID != ready.PID || recovered.Revision != ready.Revision || recovered.LastUpdateSequence <= rejected.LastSequence || recovered.UpdateError != "" || recovered.LastError != "" {
		t.Fatalf("source undo did not acknowledge retained Player: %+v %v", recovered, err)
	}
}

func TestControllerRejectsUnchangedWithWrongPlayerIdentity(t *testing.T) {
	for _, field := range []string{"pid", "revision"} {
		t.Run(field, func(t *testing.T) {
			c := controllerFixture(t, "")
			ready := startReady(t, c)
			event := map[string]any{"event": "unchanged", "pid": ready.PID, "revision": ready.Revision}
			if field == "pid" {
				event[field] = ready.PID + 1
			} else {
				event[field] = "rev-unaccepted"
			}
			emitFixtureEvent(t, c, event)
			ctx, cancel := context.WithTimeout(context.Background(), time.Second)
			state, err := c.Wait(ctx, ready.LastSequence, false)
			cancel()
			if err == nil || state.Phase != "failed" || state.LastUpdateSequence > ready.LastSequence {
				t.Fatalf("incorrect unchanged identity acknowledged: %+v %v", state, err)
			}
		})
	}
}

func TestControllerRetriesImmutableCommandAndExposesPendingResults(t *testing.T) {
	c := controllerFixture(t, "")
	ready := startReady(t, c)
	args := requestArgs(map[string]any{"requestId": "once", "arguments": map[string]any{"delayMs": 100}})
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Millisecond)
	result, err := c.Request(ctx, "custom", args)
	cancel()
	if err != nil || result.(map[string]any)["status"] != "result_pending" {
		t.Fatalf("accepted canceled request was hidden: %v %v", result, err)
	}
	ctx, cancel = context.WithTimeout(context.Background(), time.Second)
	result, err = c.Request(ctx, "custom", args)
	cancel()
	if err != nil || result.(map[string]any)["success"] != true {
		t.Fatalf("same request retry did not retrieve original result: %v %v", result, err)
	}
	emitFixtureEvent(t, c, map[string]any{"event": "updated", "ack": map[string]any{"success": true, "restartRequired": false, "loadedRevision": "rev-2", "pid": ready.PID}})
	ctx, cancel = context.WithTimeout(context.Background(), time.Second)
	_, err = c.Wait(ctx, ready.LastSequence, false)
	cancel()
	if err != nil {
		t.Fatal(err)
	}
	if replay, err := c.Request(context.Background(), "custom", args); err != nil || replay.(map[string]any)["revision"] != "rev-1" {
		t.Fatalf("retry after scene update rebuilt envelope or lost historic result: %v %v", replay, err)
	}
	args["arguments"] = json.RawMessage(`{"delayMs":101}`)
	if _, err := c.Request(context.Background(), "custom", args); err == nil {
		t.Fatal("request ID reused for another operation")
	}
	data, _ := os.ReadFile(filepath.Join(c.options.Output, "executions.json"))
	if string(data) != "1" {
		t.Fatalf("retry executed twice: %s", data)
	}
}

func TestControllerPendingRequestCannotReplayOnReplacementPlayer(t *testing.T) {
	c := controllerFixture(t, "")
	startReady(t, c)
	args := requestArgs(map[string]any{"requestId": "pending", "arguments": map[string]any{"dropResponse": true}})
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Millisecond)
	_, err := c.Request(ctx, "custom", args)
	cancel()
	if err != nil {
		t.Fatal(err)
	}
	stopCtx, stopCancel := context.WithTimeout(context.Background(), time.Second)
	if _, err := c.Stop(stopCtx); err != nil {
		t.Fatal(err)
	}
	stopCancel()
	startReady(t, c)
	result, err := c.Request(context.Background(), "custom", args)
	if err != nil || result.(map[string]any)["status"] != "result_unknown" {
		t.Fatalf("old request ID reexecuted on new Player: %v %v", result, err)
	}
}

func TestControllerGuardsStaleRevisionsResponseIdentityAndBounds(t *testing.T) {
	c := controllerFixture(t, "")
	ready := startReady(t, c)
	result, err := c.Request(context.Background(), "pause", requestArgs(map[string]any{"expectedRevision": "old-revision", "requestId": "stale"}))
	if err != nil || result.(map[string]any)["success"] != false {
		t.Fatalf("stale revision accepted: %v %v", result, err)
	}
	if _, err := os.Stat(filepath.Join(ready.ControlDirectory, "stale.request.json")); !os.IsNotExist(err) {
		t.Fatal("stale command reached Player")
	}
	for _, arguments := range []map[string]any{{"badHash": true}, {"responseRevision": "unrelated"}} {
		ctx, cancel := context.WithTimeout(context.Background(), time.Second)
		_, err := c.Request(ctx, "custom", requestArgs(map[string]any{"arguments": arguments}))
		cancel()
		if err == nil || !strings.Contains(err.Error(), "identity") {
			t.Fatalf("invalid response proof accepted: %v", err)
		}
	}
	if _, err := c.Handle(context.Background(), "runtime_query", json.RawMessage(`null`)); err == nil {
		t.Fatal("nonobject command accepted")
	}
	if _, err := c.Request(context.Background(), "custom", requestArgs(map[string]any{"arguments": map[string]any{"text": strings.Repeat("x", 65536)}})); err == nil {
		t.Fatal("oversized request accepted")
	}
}

func TestControllerStatusCopiesNestedMapsAndConcurrentRetry(t *testing.T) {
	c := controllerFixture(t, "")
	startReady(t, c)
	state := c.Status()
	state.Ready["scene"].(map[string]any)["isLoaded"] = false
	state.Events[0].Data["event"] = "changed-by-client"
	if current := c.Status(); current.Ready["scene"].(map[string]any)["isLoaded"] != true || current.Events[0].Data["event"] != "ready" {
		t.Fatal("client mutated controller state")
	}
	var wait sync.WaitGroup
	for i := 0; i < 12; i++ {
		wait.Add(1)
		go func() {
			defer wait.Done()
			ctx, cancel := context.WithTimeout(context.Background(), time.Second)
			defer cancel()
			if _, err := c.Request(ctx, "pause", requestArgs(map[string]any{"requestId": "parallel"})); err != nil {
				t.Error(err)
			}
		}()
	}
	wait.Wait()
	data, _ := os.ReadFile(filepath.Join(c.options.Output, "executions.json"))
	if string(data) != "1" {
		t.Fatalf("concurrent identical command executed more than once: %s", data)
	}
}

func TestControllerProtocolFailuresAreBounded(t *testing.T) {
	for _, mode := range []string{"overflow", "truncated", "empty-exit", "bad-pid", "symlink-control", "foreign-library-manifest"} {
		t.Run(mode, func(t *testing.T) {
			c := controllerFixture(t, mode)
			if _, err := c.Start(); err != nil {
				t.Fatal(err)
			}
			ctx, cancel := context.WithTimeout(context.Background(), time.Second)
			defer cancel()
			if state, err := c.Wait(ctx, 0, true); err == nil || state.Phase != "failed" {
				t.Fatalf("bad runner was accepted or hung: %+v %v", state, err)
			}
			stopCtx, stopCancel := context.WithTimeout(context.Background(), time.Second)
			defer stopCancel()
			if _, err := c.Stop(stopCtx); err != nil {
				t.Fatal(err)
			}
		})
	}
}

func TestControllerFailedRunnerReturnsBoundedStderrReason(t *testing.T) {
	c := controllerFixture(t, "stderr-exit")
	if _, err := c.Start(); err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	state, err := c.Wait(ctx, 0, true)
	if err == nil || state.Phase != "failed" || !strings.Contains(err.Error(), "native loader actual failure reason") {
		t.Fatalf("runner failure reason missing from wait: %+v %v", state, err)
	}
	status := c.Status()
	if status.RunnerStderrPath != filepath.Join(c.options.Output, "agent-runner-stderr.log") || status.RunnerStderrTail != state.RunnerStderrTail || !strings.Contains(status.LastError, "native loader actual failure reason") {
		t.Fatal("status lost owned stderr diagnostics", status)
	}
	if strings.Contains(state.RunnerStderrTail, "discarded-prefix-marker") || strings.Contains(state.RunnerStderrTail, strings.Repeat("x", 100)) {
		t.Fatal("oversized first-line fragment leaked into useful tail")
	}
	if len(state.RunnerStderrTail) > runnerStderrMaximum || len(state.LastError) > runnerStderrMaximum || strings.ContainsRune(state.RunnerStderrTail, 0) || !utf8.ValidString(state.RunnerStderrTail) {
		t.Fatal("stderr failure diagnostics are not bounded sanitized UTF8")
	}
	stopCtx, stopCancel := context.WithTimeout(context.Background(), time.Second)
	defer stopCancel()
	if _, err := c.Stop(stopCtx); err != nil {
		t.Fatal(err)
	}
}

func TestRunnerStderrTailRejectsReplacementSymlinkAndSpecialFiles(t *testing.T) {
	for _, mode := range []string{"replacement", "symlink", "fifo"} {
		t.Run(mode, func(t *testing.T) {
			output := t.TempDir()
			path := filepath.Join(output, "agent-runner-stderr.log")
			writer, err := os.OpenFile(path, os.O_CREATE|os.O_WRONLY, 0600)
			if err != nil {
				t.Fatal(err)
			}
			defer writer.Close()
			if _, err := writer.WriteString("owned original reason\n"); err != nil {
				t.Fatal(err)
			}
			if err := os.Rename(path, path+".original"); err != nil {
				t.Fatal(err)
			}
			switch mode {
			case "replacement":
				if err := os.WriteFile(path, []byte("foreign replacement content"), 0600); err != nil {
					t.Fatal(err)
				}
			case "symlink":
				foreign := filepath.Join(t.TempDir(), "foreign.log")
				if err := os.WriteFile(foreign, []byte("foreign replacement content"), 0600); err != nil {
					t.Fatal(err)
				}
				if err := os.Symlink(foreign, path); err != nil {
					t.Fatal(err)
				}
			case "fifo":
				if err := syscall.Mkfifo(path, 0600); err != nil {
					t.Fatal(err)
				}
			}
			if tail, err := readRunnerStderrTail(output, writer); err == nil || tail != "" {
				t.Fatalf("unsafe log replacement was read: %q %v", tail, err)
			}
		})
	}
}

func TestRunnerFailureMessageRetainsEndWithinBound(t *testing.T) {
	tail := strings.Repeat("ж", 8192) + "actual final reason"
	message := runnerFailureMessage("exit status 1", tail)
	if len(message) > runnerStderrMaximum || !utf8.ValidString(message) || !strings.HasSuffix(message, "actual final reason") {
		t.Fatal("bounded failure message lost useful suffix or UTF8")
	}
}

func TestControllerRejectsSymlinkOutputAndArtifacts(t *testing.T) {
	for _, root := range []string{"Temp", "Library"} {
		t.Run(root, func(t *testing.T) {
			project := t.TempDir()
			runner, err := os.Executable()
			if err != nil {
				t.Fatal(err)
			}
			if err := os.Symlink(t.TempDir(), filepath.Join(project, root)); err != nil {
				t.Fatal(err)
			}
			output := filepath.Join(project, "Library/UnityMicroEditor/agent")
			if root == "Temp" {
				output = filepath.Join(project, "Temp/agent")
			}
			if _, err := New(context.Background(), Options{Project: project, Runner: runner, Output: output}); err == nil {
				t.Fatal("symlink output ancestor accepted")
			}
		})
	}
	directory := t.TempDir()
	target := filepath.Join(t.TempDir(), "secret")
	os.WriteFile(target, []byte("private"), 0600)
	link := filepath.Join(directory, "response.json")
	os.Symlink(target, link)
	if _, err := readBounded(link, 65536); err == nil {
		t.Fatal("response symlink read")
	}
	if err := publishImmutable(link, []byte("command")); err == nil {
		t.Fatal("request symlink overwritten")
	}
}

func TestControllerOwnsOnlyStrictCacheChildren(t *testing.T) {
	project := t.TempDir()
	runner, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	for _, candidate := range []struct {
		path  string
		valid bool
	}{
		{"Temp/agent", true},
		{"Library/UnityMicroEditor/agent", true},
		{"Temp", false},
		{"Library/UnityMicroEditor", false},
		{"Library", false},
		{"Library/ScriptAssemblies/agent", false},
		{"Library/UnityMicroEditor-other/agent", false},
	} {
		t.Run(candidate.path, func(t *testing.T) {
			c, err := New(context.Background(), Options{Project: project, Runner: runner, Output: filepath.Join(project, candidate.path)})
			if (err == nil) != candidate.valid {
				t.Fatalf("unexpected cache ownership: %v", err)
			}
			if c != nil {
				if err := c.Close(); err != nil {
					t.Fatal(err)
				}
			}
		})
	}
}

func TestControllerLibrarySessionSurvivesEditorTempCleanup(t *testing.T) {
	c := controllerFixture(t, "library-manifest")
	if c.options.Output != filepath.Join(c.options.Project, "Library/UnityMicroEditor/agent") {
		t.Fatalf("default controller output is volatile: %s", c.options.Output)
	}
	ready := startReady(t, c)
	if err := os.MkdirAll(filepath.Join(c.options.Project, "Temp"), 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.RemoveAll(filepath.Join(c.options.Project, "Temp")); err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	response, err := c.Request(ctx, "status", requestArgs(map[string]any{"requestId": "after-editor-restart"}))
	if err != nil || response.(map[string]any)["success"] != true || c.Status().PID != ready.PID {
		t.Fatalf("Editor Temp cleanup broke Library endpoint: %v %v", response, err)
	}
}

func TestControllerCleansSeparateOwnedPlayerAfterRunnerCrash(t *testing.T) {
	c := controllerFixture(t, "spawn-child")
	ready := startReady(t, c)
	emitFixtureEvent(t, c, map[string]any{"event": "crash"})
	deadline := time.Now().Add(time.Second)
	for time.Now().Before(deadline) {
		state := c.Status()
		if state.Phase == "failed" && state.PID == 0 {
			break
		}
		time.Sleep(5 * time.Millisecond)
	}
	if state := c.Status(); state.Phase != "failed" || state.PID != 0 {
		t.Fatalf("crashed runner not reaped: %+v", state)
	}
	// SIGKILL is delivered asynchronously; the stopped runner can be reaped
	// before the kernel schedules its separate Player process to exit.
	deadline = time.Now().Add(time.Second)
	for {
		identity, _, err := readProcessIdentity(ready.PID)
		if err != nil {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("separate owned Player remains alive after runner crash: %+v", identity)
		}
		time.Sleep(5 * time.Millisecond)
	}
}

func TestNumberPIDKeepsExactIntegerIdentity(t *testing.T) {
	for _, value := range []any{json.Number("1.5"), json.Number("-1"), json.Number("999999999999999999999999"), float64(1.5), "123"} {
		if pid := numberPID(value); pid != 0 {
			t.Fatalf("invalid PID was coerced: %v -> %d", value, pid)
		}
	}
	if actual := numberPID(json.Number(strconv.Itoa(os.Getpid()))); actual != os.Getpid() {
		t.Fatal("integer PID did not survive JSON event decoding")
	}
}
