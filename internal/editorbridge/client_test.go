package editorbridge

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"strings"
	"syscall"
	"testing"
	"time"
)

func fixtureJSON(path string, value any) error {
	data, err := json.Marshal(value)
	if err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(path), 0700); err != nil {
		return err
	}
	temporary := path + ".temp"
	if err := os.WriteFile(temporary, data, 0600); err != nil {
		return err
	}
	return os.Rename(temporary, path)
}

// A real subprocess implements the mailbox without Unity. Actual Editor tests
// are opt-in and never share fixture output with these lifecycle/receipt tests.
func TestEditorBridgeHelper(t *testing.T) {
	if os.Getenv("UNITY_EDITOR_BRIDGE_TEST_HELPER") != "1" {
		return
	}
	project := argument(os.Args, "-projectPath")
	editor := os.Getenv("UNITY_EDITOR_BRIDGE_TEST_EDITOR")
	mailbox := filepath.Join(project, "Library/UnityMicroEditor/editor-bridge")
	signals := make(chan os.Signal, 2)
	signal.Notify(signals, syscall.SIGINT, syscall.SIGTERM)
	pid := os.Getpid()
	mode, _ := os.ReadFile(filepath.Join(project, "helper-mode"))
	if string(mode) == "early-exit" {
		os.Exit(7)
	}
	h := heartbeat{SchemaVersion: 1, Backend: "unity_editor", Project: project, Editor: editor, UnityVersion: Profile, PID: pid, OwnerToken: os.Getenv("UNITY_MICRO_EDITOR_BRIDGE_OWNER"), BatchMode: true}
	if string(mode) == "wrong-project" {
		h.Project += "-wrong"
	}
	if err := fixtureJSON(filepath.Join(mailbox, "ready.json"), h); err != nil {
		os.Exit(8)
	}
	type job struct {
		request envelope
		data    []byte
		due     time.Time
		drop    bool
	}
	pending := map[string]job{}
	executed := 0
	for {
		stopPath := filepath.Join(mailbox, "owner-"+h.OwnerToken+".stop.json")
		if _, err := os.Stat(stopPath); err == nil {
			os.Remove(stopPath)
			os.Exit(0)
		}
		select {
		case <-signals:
			os.Exit(0)
		default:
		}
		for id, item := range pending {
			if item.drop || time.Now().Before(item.due) {
				continue
			}
			result := json.RawMessage(`{"fixture":true}`)
			switch item.request.Action {
			case "export_graph":
				result, _ = json.Marshal(map[string]any{"schemaVersion": 1, "project": project, "unity": editor, "assemblies": []any{}})
			case "scene_proof":
				result, _ = json.Marshal(map[string]any{"schemaVersion": 1, "scenePath": "Assets/Test.unity", "sceneDirty": false})
			}
			response := Response{SchemaVersion: 1, Backend: "unity_editor", RequestID: id, RequestSHA256: digest(item.data), Project: project, UnityVersion: Profile, Editor: editor, PID: pid, Status: "completed", Success: true, Result: result}
			var args map[string]any
			json.Unmarshal(item.request.Arguments, &args)
			if args["badHash"] == true {
				response.RequestSHA256 = strings.Repeat("0", 64)
			}
			if err := fixtureJSON(filepath.Join(mailbox, id+".response.json"), response); err != nil {
				os.Exit(8)
			}
			delete(pending, id)
		}
		requests, _ := filepath.Glob(filepath.Join(mailbox, "*.request.json"))
		for _, path := range requests {
			data, err := os.ReadFile(path)
			if err != nil {
				continue
			}
			var request envelope
			if json.Unmarshal(data, &request) != nil {
				continue
			}
			id := request.ID
			responsePath := filepath.Join(mailbox, id+".response.json")
			if _, err := os.Stat(responsePath); err == nil {
				os.Remove(path)
				continue
			}
			if _, exists := pending[id]; exists {
				continue
			}
			claim := filepath.Join(mailbox, id+".accepted.json")
			if _, err := os.Stat(claim); err == nil {
				response := Response{SchemaVersion: 1, Backend: "unity_editor", RequestID: id, RequestSHA256: digest(data), Project: project, UnityVersion: Profile, Editor: editor, PID: pid, Status: "completed", Success: false, Error: &Failure{Code: "outcome_unknown", Message: "Accepted receipt is unavailable."}}
				fixtureJSON(responsePath, response)
				os.Remove(path)
				continue
			}
			if os.Rename(path, claim) != nil {
				continue
			}
			executed++
			fixtureJSON(filepath.Join(mailbox, "executions.json"), executed)
			var args map[string]any
			json.Unmarshal(request.Arguments, &args)
			delay, _ := args["delayMs"].(float64)
			pending[id] = job{request: request, data: data, due: time.Now().Add(time.Duration(delay) * time.Millisecond), drop: args["dropResponse"] == true}
		}
		time.Sleep(5 * time.Millisecond)
	}
}

func clientFixture(t *testing.T, mode string) *Client {
	t.Helper()
	project := t.TempDir()
	os.MkdirAll(filepath.Join(project, "ProjectSettings"), 0700)
	os.WriteFile(filepath.Join(project, "ProjectSettings/ProjectVersion.txt"), []byte("m_EditorVersion: "+Profile+"\n"), 0600)
	os.WriteFile(filepath.Join(project, "helper-mode"), []byte(mode), 0600)
	binary, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	editor := filepath.Join(project, "Unity")
	quote := func(value string) string { return "'" + strings.ReplaceAll(value, "'", "'\"'\"'") + "'" }
	script := "#!/bin/sh\nif [ \"$1\" = '-version' ]; then printf '" + Profile + "\\n'; exit 0; fi\nexec env UNITY_EDITOR_BRIDGE_TEST_HELPER=1 UNITY_EDITOR_BRIDGE_TEST_EDITOR=" + quote(editor) + " GORACE=atexit_sleep_ms=0 " + quote(binary) + " '-test.run=^TestEditorBridgeHelper$' -- \"$@\"\n"
	if err := os.WriteFile(editor, []byte(script), 0700); err != nil {
		t.Fatal(err)
	}
	client, err := New(context.Background(), Options{Project: project, Editor: editor, StartupTimeout: 5 * time.Second, PollInterval: 5 * time.Millisecond})
	if err != nil {
		t.Fatal(err)
	}
	client.stopGrace = 200 * time.Millisecond
	t.Cleanup(func() {
		if err := client.Close(); err != nil {
			t.Error(err)
		}
	})
	return client
}
func requestJSON(value any) json.RawMessage { data, _ := json.Marshal(value); return data }

func TestEditorBridgeProjectVersionParsing(t *testing.T) {
	for _, test := range []struct {
		name, text string
		valid      bool
	}{
		{"lf", "m_EditorVersion: " + Profile + "\n", true},
		{"crlf", "m_EditorVersion: " + Profile + "\r\nm_EditorVersionWithRevision: " + Profile + " (hash)\r\n", true},
		{"no-final-newline", "m_EditorVersion: " + Profile, true},
		{"line-whitespace", "  m_EditorVersion: " + Profile + " \r\n", true},
		{"wrong-version", "m_EditorVersion: 6000.6.4f1\n", false},
		{"suffix", "m_EditorVersion: " + Profile + "-extra\n", false},
		{"duplicate", "m_EditorVersion: " + Profile + "\nm_EditorVersion: " + Profile + "\n", false},
		{"conflicting", "m_EditorVersion: " + Profile + "\nm_EditorVersion: 6000.6.4f1\n", false},
		{"embedded", "unrelated: m_EditorVersion: " + Profile + "\n", false},
		{"missing", "m_EditorVersionWithRevision: " + Profile + " (hash)\n", false},
	} {
		t.Run(test.name, func(t *testing.T) {
			if err := ValidateProjectVersion([]byte(test.text)); (err == nil) != test.valid {
				t.Fatalf("version validity=%v wanted=%v err=%v", err == nil, test.valid, err)
			}
		})
	}
	c := clientFixture(t, "")
	if err := os.WriteFile(filepath.Join(c.options.Project, "ProjectSettings/ProjectVersion.txt"), []byte("m_EditorVersion: "+Profile+"\r\n"), 0600); err != nil {
		t.Fatal(err)
	}
	second, err := New(context.Background(), c.options)
	if err != nil {
		t.Fatal("CRLF project rejected before bridge creation:", err)
	}
	if err := second.Close(); err != nil {
		t.Fatal(err)
	}
}

func TestEditorBridgeRejectsExecutableProfileBeforeCacheMutation(t *testing.T) {
	for _, test := range []struct{ name, script string }{
		{"wrong-profile", "#!/bin/sh\nprintf '6000.6.4f1\\n'\n"},
		{"excess-output", "#!/bin/sh\nprintf '" + Profile + "\\n'\ni=0\nwhile [ $i -lt 1000 ]; do printf 'unexpected-output'; i=$((i+1)); done\n"},
	} {
		t.Run(test.name, func(t *testing.T) {
			project := t.TempDir()
			if err := os.Mkdir(filepath.Join(project, "ProjectSettings"), 0700); err != nil {
				t.Fatal(err)
			}
			if err := os.WriteFile(filepath.Join(project, "ProjectSettings/ProjectVersion.txt"), []byte("m_EditorVersion: "+Profile+"\r\n"), 0600); err != nil {
				t.Fatal(err)
			}
			editor := filepath.Join(project, "Unity")
			if err := os.WriteFile(editor, []byte(test.script), 0700); err != nil {
				t.Fatal(err)
			}
			if client, err := New(context.Background(), Options{Project: project, Editor: editor}); err == nil {
				client.Close()
				t.Fatal("wrong executable was accepted before authored installation")
			}
			if _, err := os.Stat(filepath.Join(project, "Library")); !errors.Is(err, os.ErrNotExist) {
				t.Fatal("failed executable preflight created a cache directory")
			}
		})
	}
}

func TestEditorBridgeOwnedProcessAndPersistentReceipt(t *testing.T) {
	c := clientFixture(t, "")
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	connection, err := c.OwnProcessOrAttach(ctx)
	if err != nil || !connection.Owned || connection.PID < 1 {
		t.Fatalf("owned startup: %+v %v", connection, err)
	}
	response, err := c.Do(ctx, Request{ID: "immutable-once", Action: "status"})
	if err != nil || !response.Success {
		t.Fatalf("first command: %+v %v", response, err)
	}
	if err := c.Close(); err != nil {
		t.Fatal(err)
	}
	fresh, err := New(context.Background(), c.options)
	if err != nil {
		t.Fatal(err)
	}
	defer fresh.Close()
	replay, err := fresh.Do(ctx, Request{ID: "immutable-once", Action: "status"})
	if err != nil || !replay.Success || replay.PID != connection.PID {
		t.Fatalf("restart lost persisted receipt: %+v %v", replay, err)
	}
	if fresh.command != nil {
		t.Fatal("reading historic receipt started another Editor")
	}
	if _, err := fresh.Do(ctx, Request{ID: "immutable-once", Action: "refresh"}); err == nil {
		t.Fatal("request ID reused for another action")
	}
	data, _ := os.ReadFile(filepath.Join(c.mailbox, "executions.json"))
	if string(data) != "1" {
		t.Fatalf("receipt retry executed twice: %s", data)
	}
}

func TestEditorBridgePendingRetryAndResponseLossNeverReexecutes(t *testing.T) {
	c := clientFixture(t, "")
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	connection, err := c.OwnProcessOrAttach(ctx)
	cancel()
	if err != nil {
		t.Fatal(err)
	}
	request := Request{ID: "slow-once", Action: "build_project", Arguments: requestJSON(map[string]any{"delayMs": 100})}
	ctx, cancel = context.WithTimeout(context.Background(), 30*time.Millisecond)
	pending, err := c.Do(ctx, request)
	cancel()
	if err != nil || pending.Status != "result_pending" || !pending.Accepted {
		t.Fatalf("accepted cancellation hidden: %+v %v", pending, err)
	}
	ctx, cancel = context.WithTimeout(context.Background(), 5*time.Second)
	reply, err := c.Do(ctx, request)
	cancel()
	if err != nil || !reply.Success {
		t.Fatalf("retry did not obtain original completion: %+v %v", reply, err)
	}
	os.Remove(reply.Receipt)
	ctx, cancel = context.WithTimeout(context.Background(), 5*time.Second)
	unknown, err := c.Do(ctx, request)
	cancel()
	if err != nil || unknown.Success || unknown.Error == nil || unknown.Error.Code != "outcome_unknown" {
		t.Fatalf("response loss reexecuted or claimed success: %+v %v", unknown, err)
	}
	data, _ := os.ReadFile(filepath.Join(c.mailbox, "executions.json"))
	if string(data) != "1" {
		t.Fatalf("receipt loss executed twice: %s", data)
	}
	if c.command.Process.Pid != connection.PID {
		t.Fatal("command retry replaced owned Editor")
	}
}

func TestEditorBridgeAcceptedRequestUnknownAfterProcessExit(t *testing.T) {
	c := clientFixture(t, "")
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	_, err := c.OwnProcessOrAttach(ctx)
	cancel()
	if err != nil {
		t.Fatal(err)
	}
	request := Request{ID: "unknown-once", Action: "build_host", Arguments: requestJSON(map[string]any{"dropResponse": true})}
	ctx, cancel = context.WithTimeout(context.Background(), 30*time.Millisecond)
	pending, err := c.Do(ctx, request)
	cancel()
	if err != nil || !pending.Accepted {
		t.Fatal(pending, err)
	}
	c.Close()
	fresh, err := New(context.Background(), c.options)
	if err != nil {
		t.Fatal(err)
	}
	defer fresh.Close()
	response, err := fresh.Do(context.Background(), request)
	if err != nil || response.Status != "result_unknown" || fresh.command != nil {
		t.Fatalf("old accepted ID replayed in new Editor: %+v %v", response, err)
	}
}

func TestEditorBridgeRejectsProtocolDriftAndUnsafePaths(t *testing.T) {
	c := clientFixture(t, "")
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if _, err := c.Do(ctx, Request{ID: "bad-hash", Action: "status", Arguments: requestJSON(map[string]bool{"badHash": true})}); err == nil {
		t.Fatal("wrong receipt digest accepted")
	}
	for _, args := range []json.RawMessage{json.RawMessage(`{"x":1,"x":2}`), json.RawMessage(`[]`), json.RawMessage(`{} {}`), json.RawMessage(strings.Repeat(" ", requestLimit+1))} {
		if _, err := c.Do(ctx, Request{Action: "status", Arguments: args}); err == nil {
			t.Fatal("malformed/oversized arguments accepted")
		}
	}
	if _, err := c.Do(ctx, Request{Action: "execute_code"}); err == nil {
		t.Fatal("generic code evaluation allowed")
	}
	if _, err := c.Do(ctx, Request{Action: "prefab_prepare"}); err == nil {
		t.Fatal("prefab preparation allowed on authored target")
	}
	target := filepath.Join(t.TempDir(), "private")
	os.WriteFile(target, []byte("private"), 0600)
	link := filepath.Join(c.mailbox, "link.response.json")
	os.Symlink(target, link)
	root, err := c.openRoot()
	if err != nil {
		t.Fatal(err)
	}
	defer root.Close()
	if _, err := read(root, "link.response.json", responseLimit); err == nil {
		t.Fatal("response symlink followed")
	}
	if err := publish(root, "link.response.json", []byte("replace")); err == nil {
		t.Fatal("response symlink overwritten")
	}
	for _, rootName := range []string{"Library", "ProjectSettings"} {
		project := t.TempDir()
		os.MkdirAll(filepath.Join(project, "ProjectSettings"), 0700)
		os.WriteFile(filepath.Join(project, "ProjectSettings/ProjectVersion.txt"), []byte("m_EditorVersion: "+Profile+"\n"), 0600)
		if rootName == "ProjectSettings" {
			os.RemoveAll(filepath.Join(project, rootName))
		}
		os.Symlink(t.TempDir(), filepath.Join(project, rootName))
		if _, err := New(context.Background(), Options{Project: project, Editor: c.options.Editor}); err == nil {
			t.Fatal("symlink project path accepted")
		}
	}
}

func TestEditorBridgeStartupFailureAndClientLifetimeAreBounded(t *testing.T) {
	c := clientFixture(t, "early-exit")
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if _, err := c.OwnProcessOrAttach(ctx); err == nil {
		t.Fatal("failed batch startup marked ready")
	}
	live := clientFixture(t, "")
	connection, err := live.OwnProcessOrAttach(ctx)
	if err != nil {
		t.Fatal(err)
	}
	live.cancel()
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		if _, _, _, err := process(connection.PID); err != nil {
			break
		}
		time.Sleep(5 * time.Millisecond)
	}
	if _, _, _, err := process(connection.PID); err == nil {
		t.Fatal("cancelled client retained its owned Editor")
	}
	if _, err := live.OwnProcessOrAttach(ctx); !errors.Is(err, context.Canceled) {
		t.Fatalf("closed lifetime restarted Editor: %v", err)
	}
}

func TestEditorBridgeStartupLockAndImportWorkerDiscovery(t *testing.T) {
	c := clientFixture(t, "")
	unlock, err := c.startupLock(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	other, err := New(context.Background(), c.options)
	if err != nil {
		t.Fatal(err)
	}
	defer other.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Millisecond)
	_, err = other.OwnProcessOrAttach(ctx)
	cancel()
	if !errors.Is(err, context.DeadlineExceeded) {
		unlock()
		t.Fatalf("another client ignored startup ownership: %v", err)
	}
	unlock()
	binary, _ := os.Executable()
	worker := exec.Command(binary, "-test.run=^TestEditorBridgeHelper$", "--", "-projectPath", c.options.Project, "-name", "AssetImportWorkerHW0", "-parentPid", "123")
	worker.Env = append(os.Environ(), "UNITY_EDITOR_BRIDGE_TEST_HELPER=1", "UNITY_EDITOR_BRIDGE_TEST_EDITOR="+c.options.Editor, "GORACE=atexit_sleep_ms=0")
	if err := worker.Start(); err != nil {
		t.Fatal(err)
	}
	defer func() { worker.Process.Signal(syscall.SIGTERM); worker.Wait() }()
	deadline := time.Now().Add(5 * time.Second)
	for {
		data, err := readPath(filepath.Join(c.mailbox, "ready.json"), 65536)
		var ready heartbeat
		if err == nil && json.Unmarshal(data, &ready) == nil && ready.PID == worker.Process.Pid {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("import worker helper did not become live")
		}
		time.Sleep(5 * time.Millisecond)
	}
	if pid, err := c.findEditor(); err != nil || pid != 0 {
		t.Fatalf("Unity import worker mistaken for an Editor: %d %v", pid, err)
	}
	if _, err := c.readConnection(); err == nil {
		t.Fatal("import worker heartbeat accepted as Editor")
	}
}

func TestEditorBridgeStagingMarkerCannotAliasSourceProject(t *testing.T) {
	c := clientFixture(t, "")
	path := filepath.Join(c.options.Project, "ProjectSettings/UnityMicroEditorStaging.json")
	for _, source := range []string{c.options.Project, c.options.Project + "/../" + filepath.Base(c.options.Project), "/absent-unity-editor-source"} {
		fixtureJSON(path, map[string]any{"schemaVersion": 1, "sourceProject": source})
		if err := c.validateStaging(); err == nil {
			t.Fatalf("invalid staging source accepted: %s", source)
		}
	}
	fixtureJSON(path, map[string]any{"schemaVersion": 1, "sourceProject": t.TempDir()})
	if err := c.validateStaging(); err != nil {
		t.Fatal(err)
	}
}

func TestEditorBridgePreparationAndNoOverwriteResult(t *testing.T) {
	c := clientFixture(t, "")
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	prepared, err := c.Prepare(ctx, "Assets/Test.unity")
	if err != nil || prepared.Backend != "unity_editor" || !json.Valid(prepared.Graph) || !json.Valid(prepared.SceneProof) {
		t.Fatalf("preparation export: %+v %v", prepared, err)
	}
	response, err := c.Do(ctx, Request{Action: "status"})
	if err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(c.options.Project, "Library/UnityMicroEditor/exports/status.json")
	if err := c.SaveResult(response, path); err != nil {
		t.Fatal(err)
	}
	if err := c.SaveResult(response, path); err == nil {
		t.Fatal("Editor result replaced accepted output")
	}
	if err := c.SaveResult(response, filepath.Join(c.options.Project, "Assets/status.json")); err == nil {
		t.Fatal("Editor result published into authored assets")
	}
}

func TestEditorBridgeActualEditor(t *testing.T) {
	project, editor := os.Getenv("UNITY_EDITOR_BRIDGE_PROJECT"), os.Getenv("UNITY_EDITOR_BRIDGE_EDITOR")
	if project == "" || editor == "" {
		t.Skip("set explicit disposable project and matching Unity for actual public Editor bridge integration")
	}
	c, err := New(context.Background(), Options{Project: project, Editor: editor, StartupTimeout: 180 * time.Second})
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 4*time.Minute)
	defer cancel()
	connection, err := c.OwnProcessOrAttach(ctx)
	if err != nil {
		t.Fatal(err)
	}
	response, err := c.Do(ctx, Request{ID: "actual-" + newID(), Action: "export_graph"})
	if err != nil || !response.Success {
		t.Fatalf("actual graph: %+v %v", response, err)
	}
	var graph struct {
		Assemblies     []json.RawMessage `json:"assemblies"`
		HostAssemblies []json.RawMessage `json:"hostAssemblies"`
		Scripts        []struct {
			HostProvided bool   `json:"hostProvided"`
			TypeName     string `json:"typeName"`
		} `json:"scripts"`
	}
	nativeOnly := os.Getenv("UNITY_EDITOR_BRIDGE_NATIVE_ONLY") == "1"
	if json.Unmarshal(response.Result, &graph) != nil || len(graph.Assemblies)+len(graph.HostAssemblies) == 0 ||
		nativeOnly && len(graph.Assemblies) != 0 || !nativeOnly && len(graph.Assemblies) == 0 {
		t.Fatal("actual Editor did not export Player assembly graph")
	}
	for _, script := range graph.Scripts {
		if script.HostProvided && script.TypeName == "UnityEngine.InputSystem.Editor.RemoteInputPlayerConnectionEditor" {
			t.Fatal("Editor-only InputSystem ScriptableSingleton was exported as a Player MonoScript")
		}
	}
	scene := os.Getenv("UNITY_EDITOR_BRIDGE_SCENE")
	if scene != "" {
		proof, err := c.Do(ctx, Request{Action: "scene_proof", Arguments: requestJSON(map[string]string{"scene": scene})})
		if err != nil || !proof.Success {
			t.Fatalf("actual scene proof: %+v %v", proof, err)
		}
	}
	if os.Getenv("UNITY_EDITOR_BRIDGE_COMPONENTS") == "1" {
		for _, componentType := range []string{"UnityEngine.BoxCollider", "UnityEngine.Camera", "UnityEngine.ParticleSystem"} {
			schema, err := c.Do(ctx, Request{Action: "component_schema", Arguments: requestJSON(map[string]string{"componentType": componentType})})
			if err != nil || !schema.Success {
				t.Fatalf("actual component schema %s: %+v %v", componentType, schema, err)
			}
			var result struct {
				TemplatePath   string `json:"templatePath"`
				SerializedJSON string `json:"serializedJson"`
			}
			if err := json.Unmarshal(schema.Result, &result); err != nil {
				t.Fatal(err)
			}
			data, err := readPath(result.TemplatePath, responseLimit)
			if err != nil || !bytes.Contains(data, []byte("%YAML")) || !json.Valid([]byte(result.SerializedJSON)) {
				t.Fatalf("actual native component template invalid %s: %v", componentType, err)
			}
			t.Logf("actual native component schema %s receipt=%s", componentType, schema.Receipt)
		}
	}
	if action := os.Getenv("UNITY_EDITOR_BRIDGE_BUILD_ACTION"); action != "" {
		if action != "build_project" && action != "build_host" {
			t.Fatal("invalid explicit integration build action")
		}
		preloads := preloadedSettings(t, project)
		settingsPath := filepath.Join(project, "ProjectSettings/ProjectSettings.asset")
		settingsBefore, err := readPath(settingsPath, responseLimit)
		if err != nil {
			t.Fatal(err)
		}
		build, err := c.Do(ctx, Request{Action: action, Arguments: requestJSON(map[string]any{"scenes": []string{scene}, "development": action != "build_host"})})
		if err != nil || !build.Success {
			t.Fatalf("actual build: %+v %v", build, err)
		}
		if restored := preloadedSettings(t, project); restored != preloads {
			t.Fatalf("actual build changed authored preload identities: before=%s after=%s", preloads, restored)
		}
		if action == "build_host" {
			settingsAfter, err := readPath(settingsPath, responseLimit)
			if err != nil || !bytes.Equal(settingsBefore, settingsAfter) {
				t.Fatalf("host did not restore the exact authored settings bytes: before=%s after=%s err=%v", digest(settingsBefore), digest(settingsAfter), err)
			}
			var profile struct {
				Development     bool `json:"development"`
				HostDevelopment bool `json:"hostDevelopment"`
			}
			if json.Unmarshal(build.Result, &profile) != nil || !profile.Development || !profile.HostDevelopment {
				t.Fatal("native host must use the development agent profile even when release was requested")
			}
			postBuild, err := c.Do(ctx, Request{Action: "export_graph"})
			if err != nil || !postBuild.Success {
				t.Fatalf("post-host graph failed: %+v %v", postBuild, err)
			}
			var closure struct {
				Inputs map[string]string `json:"compilerInputs"`
			}
			if json.Unmarshal(postBuild.Result, &closure) != nil || closure.Inputs[settingsPath] != digest(settingsBefore) {
				t.Fatal("post-host graph hashed temporary settings instead of restored authored configuration")
			}
			t.Logf("host restored settings bytes and compiler proof sha256=%s receipt=%s", digest(settingsBefore), postBuild.Receipt)
		}
		t.Logf("actual %s receipt=%s result=%s", action, build.Receipt, build.Result)
	}
	if os.Getenv("UNITY_EDITOR_BRIDGE_NATIVE") == "1" {
		if !connection.BatchMode {
			t.Fatal("actual native export must use isolated batch fixture")
		}
		arguments := map[string]any{"scenes": []string{scene}}
		if assets := os.Getenv("UNITY_EDITOR_BRIDGE_NATIVE_ASSETS"); assets != "" {
			arguments["assetPaths"] = strings.Split(assets, ",")
		}
		native, err := c.Do(ctx, Request{Action: "native_prepare", Arguments: requestJSON(arguments)})
		if err != nil || !native.Success {
			t.Fatalf("actual native export: %+v %v", native, err)
		}
		var catalog struct {
			NativeAssetsPresent    bool              `json:"nativeAssetsPresent"`
			SourceRoots            []json.RawMessage `json:"sourceRoots"`
			ContainsScenes         bool              `json:"containsScenes"`
			CookedRegistryVerified bool              `json:"cookedRegistryVerified"`
			CatalogPath            string            `json:"catalogPath"`
			Packs                  []struct {
				BundlePath string `json:"bundlePath"`
				Bytes      int64  `json:"bytes"`
			} `json:"packs"`
		}
		if err := json.Unmarshal(native.Result, &catalog); err != nil {
			t.Fatal(err)
		}
		empty := os.Getenv("UNITY_EDITOR_BRIDGE_NATIVE_EMPTY") == "1"
		if catalog.ContainsScenes || !catalog.CookedRegistryVerified || empty && (catalog.NativeAssetsPresent || len(catalog.Packs) != 0 || len(catalog.SourceRoots) == 0) || !empty && (!catalog.NativeAssetsPresent || len(catalog.Packs) == 0) {
			t.Fatal("native export omitted cooked verification or embedded scenes")
		}
		for _, pack := range catalog.Packs {
			info, err := os.Stat(pack.BundlePath)
			if err != nil || info.Size() != pack.Bytes {
				t.Fatalf("native bundle mismatch: %v", err)
			}
		}
		data, err := readPath(catalog.CatalogPath, responseLimit)
		if err != nil || !json.Valid(data) {
			t.Fatalf("native catalog invalid: %v", err)
		}
		if _, err := os.Stat(filepath.Join(project, "Assets/__UnityPlayImports")); !errors.Is(err, os.ErrNotExist) {
			t.Fatal("native exporter did not remove owned scratch")
		}
		t.Logf("actual native export receipt=%s catalog=%s packs=%d", native.Receipt, catalog.CatalogPath, len(catalog.Packs))
	}
	if !connection.Owned {
		if err := c.Close(); err != nil {
			t.Fatal(err)
		}
		if _, _, _, err := process(connection.PID); err != nil {
			t.Fatal("closing attached client killed another Editor")
		}
	}
	t.Logf("actual public Editor backend PID=%d owned=%v assemblies=%d graphReceipt=%s", connection.PID, connection.Owned, len(graph.Assemblies), response.Receipt)
}

func preloadedSettings(t *testing.T, project string) string {
	t.Helper()
	data, err := readPath(filepath.Join(project, "ProjectSettings/ProjectSettings.asset"), responseLimit)
	if err != nil {
		t.Fatal(err)
	}
	var entries []string
	for _, line := range strings.Split(string(data), "\n") {
		if len(entries) == 0 {
			if strings.HasPrefix(line, "  preloadedAssets:") {
				entries = append(entries, line)
			}
			continue
		}
		if strings.HasPrefix(line, "  - ") || strings.HasPrefix(line, "    ") {
			entries = append(entries, line)
			continue
		}
		break
	}
	if len(entries) == 0 {
		t.Fatal("actual project settings omitted preloadedAssets")
	}
	return strings.Join(entries, "\n")
}
