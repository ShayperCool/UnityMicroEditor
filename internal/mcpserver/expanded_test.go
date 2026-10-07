package mcpserver

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"

	"github.com/modelcontextprotocol/go-sdk/mcp"
)

func TestProjectActionsExposeTypedSchemasAndPreserveNullReferenceValue(t *testing.T) {
	var operation string
	var arguments map[string]json.RawMessage
	server, err := New(Options{Project: projectFixture(t), Backends: map[string]any{"project_build": "unity_editor"}, ProjectActions: func(_ context.Context, name string, raw json.RawMessage) (any, error) {
		operation = name
		arguments = nil
		if err := json.Unmarshal(raw, &arguments); err != nil {
			return nil, err
		}
		return map[string]any{"success": true, "backend": "unity_editor"}, nil
	}})
	if err != nil {
		t.Fatal(err)
	}
	session, ctx := connectMemory(t, server)
	tools, _ := session.ListTools(ctx, nil)
	if len(tools.Tools) != 16 {
		t.Fatalf("project action count %d", len(tools.Tools))
	}
	_, result := call(t, ctx, session, "prefab_edit", map[string]any{"action": "set_override", "path": "Assets/Scene.unity", "expectedRevision": "source-revision", "operationId": "edit-prefab", "fileId": "123", "property": "linkedAsset", "value": nil})
	if !result.Success || operation != "prefab_edit" || string(arguments["value"]) != "null" {
		t.Fatalf("forwarding %s %s %+v", operation, arguments["value"], result)
	}
	for _, value := range []any{json.Number("9223372036854775807"), 7, true, "label", map[string]any{"fileId": "9223372036854775807"}, []any{1, 2}} {
		_, result = call(t, ctx, session, "prefab_edit", map[string]any{"action": "set_override", "path": "Assets/Scene.unity", "expectedRevision": "source-revision", "operationId": "edit-prefab", "property": "counter", "value": value})
		want, _ := json.Marshal(value)
		if !result.Success || string(arguments["value"]) != string(want) {
			t.Fatalf("JSON value changed: want %s got %s %+v", want, arguments["value"], result)
		}
	}
	_, result = call(t, ctx, session, "prefab_edit", map[string]any{"action": "apply", "path": "Assets/Scene.unity", "expectedRevision": "source-revision", "operationId": "edit-prefab"})
	if !result.Success || arguments["value"] != nil {
		t.Fatal("absent optional value became an explicit null")
	}
	_, result = call(t, ctx, session, "project_capabilities", map[string]any{})
	data := result.Data.(map[string]any)
	if data["projectActionsConfigured"] != true || data["backends"].(map[string]any)["project_build"] != "unity_editor" {
		t.Fatal("configured backend capability omitted")
	}
}

func TestTypedToolPreservesLargeNumbersInArgumentsAndReplies(t *testing.T) {
	server, err := New(Options{Project: projectFixture(t), ProjectActions: func(_ context.Context, _ string, raw json.RawMessage) (any, error) {
		var input struct {
			Fields map[string]json.RawMessage `json:"fields"`
		}
		if err := json.Unmarshal(raw, &input); err != nil {
			t.Fatal(err)
		}
		if string(input.Fields["counter"]) != "9223372036854775807" {
			t.Fatalf("serialized integer input rounded: %s", input.Fields["counter"])
		}
		return map[string]any{"counter": json.Number("9223372036854775807")}, nil
	}})
	if err != nil {
		t.Fatal(err)
	}
	session, ctx := connectMemory(t, server)
	result, receipt := call(t, ctx, session, "component_add", map[string]any{"path": "Assets/Scene.unity", "fileId": "1", "componentType": "BoxCollider", "expectedRevision": "revision", "operationId": "counter", "fields": map[string]any{"counter": json.Number("9223372036854775807")}})
	if !receipt.Success {
		t.Fatalf("typed action failed %+v", receipt)
	}
	text := result.Content[0].(*mcp.TextContent).Text
	if !strings.Contains(text, `"counter":9223372036854775807`) {
		t.Fatalf("serialized integer reply rounded: %s", text)
	}
}

func TestProjectBuildForwardsExplicitAndDefaultReleaseBoolean(t *testing.T) {
	var received map[string]json.RawMessage
	server, err := New(Options{Project: projectFixture(t), ProjectActions: func(_ context.Context, name string, raw json.RawMessage) (any, error) {
		if name != "project_build" {
			t.Fatal("unexpected project action", name)
		}
		if err := json.Unmarshal(raw, &received); err != nil {
			t.Fatal(err)
		}
		return map[string]any{"success": true}, nil
	}})
	if err != nil {
		t.Fatal(err)
	}
	session, ctx := connectMemory(t, server)
	for _, development := range []any{nil, false, true} {
		input := map[string]any{"scenes": []string{"Assets/Scene.unity"}, "requestId": "release-build"}
		if development != nil {
			input["development"] = development
		}
		_, receipt := call(t, ctx, session, "project_build", input)
		want := "false"
		if development == true {
			want = "true"
		}
		if !receipt.Success || string(received["development"]) != want {
			t.Fatalf("build mode %v forwarded %s: %+v", development, received["development"], receipt)
		}
	}
}

func TestSourcePropertyLongSurvivesMCPAndYAMLPublication(t *testing.T) {
	project := projectFixture(t)
	path := filepath.Join(project, "Assets/Counter.asset")
	content := "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!114 &11400000\nMonoBehaviour:\n  m_ObjectHideFlags: 0\n  m_Name: Counter\n  counter: 0\n"
	if err := os.WriteFile(path, []byte(content), 0600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path+".meta", []byte("fileFormatVersion: 2\nguid: 11111111111111111111111111111111\n"), 0600); err != nil {
		t.Fatal(err)
	}
	server, err := New(Options{Project: project})
	if err != nil {
		t.Fatal(err)
	}
	session, ctx := connectMemory(t, server)
	_, source := call(t, ctx, session, "asset_read", map[string]any{"path": "Assets/Counter.asset"})
	_, result := call(t, ctx, session, "scene_edit", map[string]any{"path": "Assets/Counter.asset", "expectedRevision": source.Data.(map[string]any)["revision"], "operationId": "long-property", "operations": []any{map[string]any{"action": "set_property", "fileId": "11400000", "property": "counter", "value": json.Number("9223372036854775807")}}})
	if !result.Success {
		t.Fatalf("long property source mutation failed %+v", result)
	}
	actual, _ := os.ReadFile(path)
	if !strings.Contains(string(actual), "counter: 9223372036854775807\n") {
		t.Fatalf("authored long property rounded: %s", actual)
	}
}

func TestDynamicProjectToolsSchemasRefreshGuardsAndRawIntegerPreservation(t *testing.T) {
	var mu sync.Mutex
	name, revision := "game.value", "catalog-one"
	readOnly := true
	pid := 77
	invocations := 0
	var lastRequest map[string]json.RawMessage
	inputSchema := map[string]any{"type": "object", "properties": map[string]any{"amount": map[string]any{"type": "integer", "minimum": 1}}, "required": []string{"amount"}, "additionalProperties": false}
	handler := func(_ context.Context, operation string, raw json.RawMessage) (any, error) {
		mu.Lock()
		defer mu.Unlock()
		var request map[string]json.RawMessage
		if err := json.Unmarshal(raw, &request); err != nil {
			return nil, err
		}
		if operation == "session_control" {
			return map[string]any{"phase": "ready", "pid": pid, "revision": revision}, nil
		}
		var command string
		if err := json.Unmarshal(request["operation"], &command); err != nil {
			return nil, err
		}
		if command == "tools" {
			return map[string]any{"success": true, "pid": pid, "revision": revision, "result": map[string]any{"tools": []any{map[string]any{"name": name, "description": "Public method", "readOnly": readOnly, "binding": "static", "inputSchema": inputSchema, "outputSchema": map[string]any{"type": "integer"}}}, "errors": []any{}}}, nil
		}
		if command == "invoke" {
			invocations++
			lastRequest = request
			return map[string]any{"success": true, "pid": pid, "revision": revision, "result": 1}, nil
		}
		return nil, nil
	}
	server, err := New(Options{Project: projectFixture(t), Runtime: handler})
	if err != nil {
		t.Fatal(err)
	}
	session, ctx := connectMemory(t, server)
	_, state := call(t, ctx, session, "session_control", map[string]any{"action": "wait"})
	if state.Data.(map[string]any)["projectToolsRefreshed"] != true {
		t.Fatalf("catalog was not refreshed %+v", state)
	}
	tools, _ := session.ListTools(ctx, nil)
	var dynamic *mcp.Tool
	for _, tool := range tools.Tools {
		if tool.Name == "project.game.value" {
			dynamic = tool
		}
	}
	if dynamic == nil || !dynamic.Annotations.ReadOnlyHint || dynamic.OutputSchema != nil {
		t.Fatal("dynamic annotation/output envelope incorrect")
	}
	schema := dynamic.InputSchema.(map[string]any)["properties"].(map[string]any)["parameters"].(map[string]any)
	if schema["properties"].(map[string]any)["amount"].(map[string]any)["minimum"] != float64(1) {
		t.Fatal("DTO schema was not preserved")
	}
	result, invalid := call(t, ctx, session, dynamic.Name, map[string]any{"parameters": map[string]any{"amount": 0}})
	if !result.IsError || invalid.Error.Code != "invalid_request" {
		t.Fatalf("DTO constraints were not validated %+v", invalid)
	}
	mu.Lock()
	before := invocations
	mu.Unlock()
	if before != 0 {
		t.Fatal("invalid DTO executed project code")
	}
	_, invoked := call(t, ctx, session, dynamic.Name, map[string]any{"parameters": map[string]any{"amount": json.Number("9223372036854775807")}})
	if !invoked.Success {
		t.Fatalf("invoke %+v", invoked)
	}
	mu.Lock()
	if string(lastRequest["expectedPid"]) != "77" || string(lastRequest["expectedRevision"]) != `"catalog-one"` || !strings.Contains(string(lastRequest["arguments"]), "9223372036854775807") {
		t.Fatalf("identity/integer changed: %s", lastRequest)
	}
	name, revision, pid, readOnly = "game.changed", "catalog-two", 88, false
	mu.Unlock()
	_, refreshed := call(t, ctx, session, "session_control", map[string]any{"action": "wait_update"})
	if !refreshed.Success {
		t.Fatal("update catalog refresh failed")
	}
	tools, _ = session.ListTools(ctx, nil)
	found := false
	for _, tool := range tools.Tools {
		if tool.Name == "project.game.value" {
			t.Fatal("stale project tool survived update")
		}
		if tool.Name == "project.game.changed" {
			found = true
			if tool.Annotations.ReadOnlyHint {
				t.Fatal("mutating method retains readonly hint")
			}
		}
	}
	if !found {
		t.Fatal("new project tool missing")
	}
	result, invalid = call(t, ctx, session, "project.game.changed", map[string]any{"parameters": map[string]any{"amount": 1}})
	if !result.IsError || invalid.Error.Code != "invalid_request" {
		t.Fatal("mutating project method accepted missing requestId")
	}
	_, _ = call(t, ctx, session, "session_control", map[string]any{"action": "stop"})
	tools, _ = session.ListTools(ctx, nil)
	for _, tool := range tools.Tools {
		if strings.HasPrefix(tool.Name, "project.") {
			t.Fatal("stopped session retained project tool")
		}
	}
}

func TestDynamicComponentBindingRejectsAmbiguousTargetAndInvalidCatalog(t *testing.T) {
	invocations := 0
	handler := func(_ context.Context, name string, raw json.RawMessage) (any, error) {
		var input RuntimeInput
		if err := json.Unmarshal(raw, &input); err != nil {
			return nil, err
		}
		if input.Operation == "tools" {
			return map[string]any{"success": true, "pid": 123, "revision": "rev", "result": map[string]any{"tools": []any{
				map[string]any{"name": "component.run", "binding": "component", "readOnly": false, "inputSchema": map[string]any{"type": "object", "additionalProperties": false}},
				map[string]any{"name": "bad.schema", "binding": "static", "readOnly": true, "inputSchema": map[string]any{"type": "object", "$ref": "https://invalid.example/schema"}},
			}}}, nil
		}
		invocations++
		return map[string]any{"success": true}, nil
	}
	server, err := New(Options{Project: projectFixture(t), Runtime: handler})
	if err != nil {
		t.Fatal(err)
	}
	session, ctx := connectMemory(t, server)
	_, discovered := call(t, ctx, session, "project_tools", map[string]any{})
	data := discovered.Data.(map[string]any)["result"].(map[string]any)
	if data["mcpRegisteredCount"] != float64(1) || len(data["mcpCatalogErrors"].([]any)) != 1 {
		t.Fatalf("invalid schema was not reported %+v", data)
	}
	for _, target := range []any{nil, map[string]any{"path": "Root", "fileId": "1"}} {
		args := map[string]any{"parameters": map[string]any{}, "requestId": "run-component"}
		if target != nil {
			args["target"] = target
		}
		result, invalid := call(t, ctx, session, "project.component.run", args)
		if !result.IsError || invalid.Error.Code != "invalid_request" {
			t.Fatal("missing/ambiguous component target accepted")
		}
	}
	if invocations != 0 {
		t.Fatal("invalid component target invoked project code")
	}
	_, valid := call(t, ctx, session, "project.component.run", map[string]any{"parameters": map[string]any{}, "requestId": "run-component", "target": map[string]any{"path": "Root"}})
	if !valid.Success || invocations != 1 {
		t.Fatal("explicit component target did not invoke")
	}
}

func TestCatalogDiscoveryFailureDropsPreviouslyPublishedTools(t *testing.T) {
	var mu sync.Mutex
	mode := "ready"
	handler := func(_ context.Context, _ string, _ json.RawMessage) (any, error) {
		mu.Lock()
		defer mu.Unlock()
		if mode == "pending" {
			return map[string]any{"status": "result_pending", "success": false}, nil
		}
		result := map[string]any{"success": true, "pid": 23, "revision": "current", "result": map[string]any{"tools": []any{map[string]any{"name": "read", "binding": "static", "readOnly": true, "inputSchema": map[string]any{"type": "object"}}}}}
		if mode == "invalid" {
			delete(result, "revision")
		}
		return result, nil
	}
	server, err := New(Options{Project: projectFixture(t), Runtime: handler})
	if err != nil {
		t.Fatal(err)
	}
	session, ctx := connectMemory(t, server)
	for _, failure := range []string{"pending", "invalid"} {
		mu.Lock()
		mode = "ready"
		mu.Unlock()
		_, discovered := call(t, ctx, session, "project_tools", map[string]any{})
		if !discovered.Success {
			t.Fatal("valid catalog rejected")
		}
		mu.Lock()
		mode = failure
		mu.Unlock()
		_, discovered = call(t, ctx, session, "project_tools", map[string]any{})
		if discovered.Success {
			t.Fatalf("%s catalog reported success", failure)
		}
		tools, err := session.ListTools(ctx, nil)
		if err != nil {
			t.Fatal(err)
		}
		for _, tool := range tools.Tools {
			if strings.HasPrefix(tool.Name, "project.") {
				t.Fatalf("%s discovery retained stale tool %s", failure, tool.Name)
			}
		}
	}
}
