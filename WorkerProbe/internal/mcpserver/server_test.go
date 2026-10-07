package mcpserver

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"
	"unicode/utf8"

	"github.com/modelcontextprotocol/go-sdk/mcp"
)

func projectFixture(t *testing.T) string {
	t.Helper()
	root := t.TempDir()
	for _, dir := range []string{"Assets/Scripts", "ProjectSettings"} {
		if err := os.MkdirAll(filepath.Join(root, dir), 0755); err != nil {
			t.Fatal(err)
		}
	}
	if err := os.WriteFile(filepath.Join(root, "ProjectSettings/ProjectVersion.txt"), []byte("m_EditorVersion: 6000.6.3f1\n"), 0644); err != nil {
		t.Fatal(err)
	}
	return root
}
func connectMemory(t *testing.T, server *mcp.Server) (*mcp.ClientSession, context.Context) {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	t.Cleanup(cancel)
	clientTransport, serverTransport := mcp.NewInMemoryTransports()
	serverSession, err := server.Connect(ctx, serverTransport, nil)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = serverSession.Close() })
	client := mcp.NewClient(&mcp.Implementation{Name: "test-client", Version: "1"}, nil)
	session, err := client.Connect(ctx, clientTransport, nil)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = session.Close() })
	return session, ctx
}
func call(t *testing.T, ctx context.Context, session *mcp.ClientSession, name string, args any) (*mcp.CallToolResult, Reply) {
	t.Helper()
	result, err := session.CallTool(ctx, &mcp.CallToolParams{Name: name, Arguments: args})
	if err != nil {
		t.Fatal(err)
	}
	if len(result.Content) == 0 {
		t.Fatal("missing content")
	}
	text, ok := result.Content[0].(*mcp.TextContent)
	if !ok {
		t.Fatal("missing JSON text result")
	}
	var reply Reply
	if err := json.Unmarshal([]byte(text.Text), &reply); err != nil {
		t.Fatal(err)
	}
	return result, reply
}
func TestToolDiscoveryResourcesAndDurableScriptOperations(t *testing.T) {
	project := projectFixture(t)
	server, err := New(Options{Project: project})
	if err != nil {
		t.Fatal(err)
	}
	session, ctx := connectMemory(t, server)
	tools, err := session.ListTools(ctx, nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(tools.Tools) != 11 {
		t.Fatalf("got %d tools", len(tools.Tools))
	}
	for _, tool := range tools.Tools {
		if tool.InputSchema == nil {
			t.Fatalf("%s has no schema", tool.Name)
		}
	}
	resources, err := session.ListResources(ctx, nil)
	if err != nil || len(resources.Resources) != 2 {
		t.Fatalf("resources %v %v", resources, err)
	}
	resource, err := session.ReadResource(ctx, &mcp.ReadResourceParams{URI: "unitymicroeditor://project"})
	if err != nil || len(resource.Contents) != 1 || !strings.Contains(resource.Contents[0].Text, "6000.6.3f1") {
		t.Fatalf("read resource %v %v", resource, err)
	}
	create := map[string]any{"path": "Assets/Scripts/Agent.cs", "expectedRevision": "absent", "operationId": "mcp-create", "content": "public class Agent {}\n"}
	result, reply := call(t, ctx, session, "script_create", create)
	if result.IsError || !reply.Success {
		t.Fatalf("create %+v", reply)
	}
	data := reply.Data.(map[string]any)
	if len(data["guid"].(string)) != 32 || data["revision"] == "" {
		t.Fatal("missing identity/revision")
	}
	_, replay := call(t, ctx, session, "script_create", create)
	if !replay.Success || replay.Data.(map[string]any)["replayed"] != true {
		t.Fatalf("retry %+v", replay)
	}
	_, source := call(t, ctx, session, "script_read", map[string]any{"path": "Assets/Scripts/Agent.cs"})
	metadata := source.Data.(map[string]any)["metadata"]
	edit := map[string]any{"path": "Assets/Scripts/Agent.cs", "expectedRevision": data["revision"], "operationId": "mcp-edit", "content": "public class Agent { public int value = 3; }\n"}
	_, edited := call(t, ctx, session, "script_edit", edit)
	if !edited.Success {
		t.Fatalf("edit %+v", edited)
	}
	_, after := call(t, ctx, session, "script_read", map[string]any{"path": "Assets/Scripts/Agent.cs"})
	if after.Data.(map[string]any)["metadata"] != metadata {
		t.Fatal("metadata changed")
	}
	edit["operationId"] = "mcp-stale"
	result, stale := call(t, ctx, session, "script_edit", edit)
	if !result.IsError || stale.Error == nil || stale.Error.Code != "revision_conflict" {
		t.Fatalf("stale reply %+v %+v", result, stale)
	}
	result, invalid := call(t, ctx, session, "asset_read", map[string]any{"path": "Assets/../ProjectSettings/ProjectVersion.txt"})
	if !result.IsError || invalid.Error.Code != "invalid_path" {
		t.Fatalf("path escape %+v", invalid)
	}
}
func TestRuntimeBoundaryAllowsOnlyHonestReadOperations(t *testing.T) {
	var receivedName string
	var received RuntimeInput
	handler := func(_ context.Context, name string, raw json.RawMessage) (any, error) {
		receivedName = name
		if err := json.Unmarshal(raw, &received); err != nil {
			return nil, err
		}
		return map[string]any{"operation": received.Operation}, nil
	}
	server, err := New(Options{Project: projectFixture(t), Runtime: handler})
	if err != nil {
		t.Fatal(err)
	}
	session, ctx := connectMemory(t, server)
	_, good := call(t, ctx, session, "runtime_query", map[string]any{"operation": "inspect", "arguments": map[string]any{"fileId": "9223372036854775807"}, "expectedPid": 123, "expectedRevision": "runtime-revision"})
	if !good.Success || receivedName != "runtime_query" || received.ExpectedPID != 123 || received.Arguments["fileId"] != "9223372036854775807" {
		t.Fatalf("forwarding %+v %+v", good, received)
	}
	receivedName = ""
	result, bad := call(t, ctx, session, "runtime_query", map[string]any{"operation": "pause"})
	if !result.IsError || bad.Error.Code != "unsupported_operation" || receivedName != "" {
		t.Fatal("query permitted mutation")
	}
	_, good = call(t, ctx, session, "runtime_command", map[string]any{"operation": "pause", "requestId": "command-id"})
	if !good.Success || receivedName != "runtime_command" || received.RequestID != "command-id" {
		t.Fatal("command forwarding")
	}
}

type bearerTransport struct {
	base  http.RoundTripper
	token string
}

func (b bearerTransport) RoundTrip(request *http.Request) (*http.Response, error) {
	clone := request.Clone(request.Context())
	clone.Header.Set("Authorization", "Bearer "+b.token)
	return b.base.RoundTrip(clone)
}
func TestStreamableHTTPProtocolAuthenticationAndOriginProtection(t *testing.T) {
	server, err := New(Options{Project: projectFixture(t)})
	if err != nil {
		t.Fatal(err)
	}
	token := strings.Repeat("a", 40)
	httpServer := httptest.NewServer(HTTPHandler(server, token))
	defer httpServer.Close()
	response, err := http.Get(httpServer.URL + "/health")
	if err != nil {
		t.Fatal(err)
	}
	_ = response.Body.Close()
	if response.StatusCode != http.StatusUnauthorized {
		t.Fatalf("unauthorized status=%d", response.StatusCode)
	}
	client := mcp.NewClient(&mcp.Implementation{Name: "http-test"}, nil)
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()
	transport := &mcp.StreamableClientTransport{Endpoint: httpServer.URL + "/mcp", HTTPClient: &http.Client{Transport: bearerTransport{http.DefaultTransport, token}}}
	session, err := client.Connect(ctx, transport, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer session.Close()
	result, reply := call(t, ctx, session, "project_inspect", map[string]any{})
	if result.IsError || !reply.Success {
		t.Fatalf("HTTP tool call %+v", reply)
	}
	body := `{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"origin-test","version":"1"}}}`
	request, err := http.NewRequest(http.MethodPost, httpServer.URL+"/mcp", strings.NewReader(body))
	if err != nil {
		t.Fatal(err)
	}
	request.Header.Set("Authorization", "Bearer "+token)
	request.Header.Set("Content-Type", "application/json")
	request.Header.Set("Accept", "application/json, text/event-stream")
	request.Header.Set("Origin", "https://evil.example")
	response, err = http.DefaultClient.Do(request)
	if err != nil {
		t.Fatal(err)
	}
	bytes, _ := io.ReadAll(response.Body)
	_ = response.Body.Close()
	if response.StatusCode != http.StatusForbidden {
		t.Fatalf("cross-origin status=%d body=%s", response.StatusCode, bytes)
	}
	request.Header.Del("Origin")
	request.Host = "evil.example"
	request.Body = io.NopCloser(strings.NewReader(body))
	response, err = http.DefaultClient.Do(request)
	if err != nil {
		t.Fatal(err)
	}
	_ = response.Body.Close()
	if response.StatusCode != http.StatusForbidden {
		t.Fatalf("DNS rebinding status=%d", response.StatusCode)
	}
}

// The child test process runs the actual stdio entry point. CommandTransport
// proves protocol framing is clean and no informational output reaches stdout.
func TestStdioHelper(t *testing.T) {
	project := os.Getenv("UME_MCP_STDIO_TEST_PROJECT")
	if project == "" {
		return
	}
	if err := Run(context.Background(), []string{"--project", project}); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	os.Exit(0)
}
func TestStdioTransportStartsAndServesTools(t *testing.T) {
	project := projectFixture(t)
	command := exec.Command(os.Args[0], "-test.run=^TestStdioHelper$")
	command.Env = append(os.Environ(), "UME_MCP_STDIO_TEST_PROJECT="+project)
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()
	client := mcp.NewClient(&mcp.Implementation{Name: "stdio-test"}, nil)
	session, err := client.Connect(ctx, &mcp.CommandTransport{Command: command}, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer session.Close()
	result, reply := call(t, ctx, session, "project_capabilities", map[string]any{})
	if result.IsError || !reply.Success {
		t.Fatalf("stdio tool call %+v", reply)
	}
}
func TestCLIRequiresProjectAndLoopbackListener(t *testing.T) {
	if _, err := ParseOptions(nil); err == nil {
		t.Fatal("accepted inferred project")
	}
	err := RunOptions(context.Background(), Options{Project: projectFixture(t), Transport: "http", Listen: "0.0.0.0:0"})
	if err == nil || !strings.Contains(err.Error(), "loopback") {
		t.Fatalf("broad bind accepted: %v", err)
	}
}

func TestRuntimeRejectedAndPendingResultsDoNotClaimAppliedSuccess(t *testing.T) {
	response := map[string]any{"success": false, "error": "stale object", "requestId": "receipt-id", "revision": "revision"}
	server, err := New(Options{Project: projectFixture(t), Runtime: func(context.Context, string, json.RawMessage) (any, error) { return response, nil }})
	if err != nil {
		t.Fatal(err)
	}
	session, ctx := connectMemory(t, server)
	result, rejected := call(t, ctx, session, "runtime_command", map[string]any{"operation": "pause", "requestId": "receipt-id"})
	if !result.IsError || rejected.Success || rejected.Error == nil || rejected.Error.Code != "runtime_rejected" || rejected.Error.Details["requestId"] != "receipt-id" || rejected.Error.Details["revision"] != "revision" {
		t.Fatalf("rejection %+v", rejected)
	}
	response = map[string]any{"status": "result_pending", "requestId": "receipt-id"}
	result, pending := call(t, ctx, session, "runtime_command", map[string]any{"operation": "pause", "requestId": "receipt-id"})
	if result.IsError || pending.Success || pending.Status != "pending" || pending.Data.(map[string]any)["requestId"] != "receipt-id" {
		t.Fatalf("pending %+v", pending)
	}
}

func TestSourcePagingRetainsRevisionAndUTF8Boundaries(t *testing.T) {
	project := projectFixture(t)
	path := "Assets/Scripts/Large.cs"
	prefix := "public class Large {}\n// "
	content := prefix + strings.Repeat("Я", 180000) + "\n"
	if err := os.WriteFile(filepath.Join(project, path), []byte(content), 0644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(project, path)+".meta", []byte("guid: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n"), 0644); err != nil {
		t.Fatal(err)
	}
	server, err := New(Options{Project: project})
	if err != nil {
		t.Fatal(err)
	}
	session, ctx := connectMemory(t, server)
	_, first := call(t, ctx, session, "script_read", map[string]any{"path": path})
	data := first.Data.(map[string]any)
	revision := data["revision"].(string)
	if data["truncated"] != true || int(data["fullLength"].(float64)) != len(content) || data["nextOffset"] == nil {
		t.Fatalf("paging metadata %+v", data)
	}
	reconstructed := data["content"].(string)
	offset := int(data["nextOffset"].(float64))
	if !utf8.ValidString(reconstructed) {
		t.Fatal("first page split a UTF8 rune")
	}
	for {
		_, page := call(t, ctx, session, "script_read", map[string]any{"path": path, "offset": offset, "expectedRevision": revision})
		if !page.Success {
			t.Fatalf("page %+v", page)
		}
		values := page.Data.(map[string]any)
		chunk := values["content"].(string)
		if !utf8.ValidString(chunk) || values["revision"] != revision {
			t.Fatal("invalid chunk/revision")
		}
		reconstructed += chunk
		if values["nextOffset"] == nil {
			break
		}
		offset = int(values["nextOffset"].(float64))
	}
	if reconstructed != content {
		t.Fatal("pages did not reproduce exact source")
	}
	result, invalid := call(t, ctx, session, "script_read", map[string]any{"path": path, "offset": len(prefix) + 1})
	if !result.IsError || invalid.Error.Code != "invalid_request" {
		t.Fatal("accepted offset inside a rune")
	}
	if err := os.WriteFile(filepath.Join(project, path), []byte(content+"// changed\n"), 0644); err != nil {
		t.Fatal(err)
	}
	result, stale := call(t, ctx, session, "script_read", map[string]any{"path": path, "offset": 0, "expectedRevision": revision})
	if !result.IsError || stale.Error.Code != "revision_conflict" {
		t.Fatal("mixed revisions accepted")
	}
}
func TestAuthoredHierarchyPagingAndFieldOmission(t *testing.T) {
	project := projectFixture(t)
	var scene strings.Builder
	scene.WriteString("%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n")
	for i := 0; i < 120; i++ {
		goID, transformID := i*2+1, i*2+2
		fmt.Fprintf(&scene, "--- !u!1 &%d\nGameObject:\n  m_Name: Object%d\n  m_IsActive: 1\n  m_Component:\n  - component: {fileID: %d}\n--- !u!4 &%d\nTransform:\n  m_GameObject: {fileID: %d}\n  m_LocalPosition: {x: 0, y: 0, z: 0}\n  m_Children: []\n  m_Father: {fileID: 0}\n", goID, i, transformID, transformID, goID)
	}
	path := "Assets/Large.unity"
	if err := os.WriteFile(filepath.Join(project, path), []byte(scene.String()), 0644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(project, path)+".meta", []byte("guid: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n"), 0644); err != nil {
		t.Fatal(err)
	}
	server, err := New(Options{Project: project})
	if err != nil {
		t.Fatal(err)
	}
	session, ctx := connectMemory(t, server)
	_, first := call(t, ctx, session, "scene_hierarchy", map[string]any{"path": path})
	data := first.Data.(map[string]any)
	if len(data["objects"].([]any)) != 100 || data["total"] != float64(120) || data["nextOffset"] != float64(100) {
		t.Fatalf("first hierarchy page %+v", data)
	}
	_, second := call(t, ctx, session, "scene_hierarchy", map[string]any{"path": path, "offset": 100, "includeFields": false, "expectedRevision": data["revision"]})
	values := second.Data.(map[string]any)
	if len(values["objects"].([]any)) != 20 || values["nextOffset"] != nil {
		t.Fatal("second page wrong")
	}
	component := values["objects"].([]any)[0].(map[string]any)["components"].([]any)[0].(map[string]any)
	if component["fields"] != nil {
		t.Fatal("fields were not omitted")
	}
}
func TestOversizedReplyReturnsStructuredError(t *testing.T) {
	result, response, err := reply(strings.Repeat("x", 5<<20), nil)
	if err != nil || !result.IsError || response.Error == nil || response.Error.Code != "response_too_large" {
		t.Fatalf("size guard %+v %v", response, err)
	}
}

func TestOversizedResourceReturnsErrorAndPreservesConnection(t *testing.T) {
	server, err := New(Options{Project: projectFixture(t)})
	if err != nil {
		t.Fatal(err)
	}
	const uri = "unitymicroeditor://oversized-test"
	server.AddResource(&mcp.Resource{URI: uri, Name: "Oversized test"}, func(context.Context, *mcp.ReadResourceRequest) (*mcp.ReadResourceResult, error) {
		// NUL needs six JSON bytes. The resource bound must measure the encoded
		// response, rather than just the original string's byte length.
		return resourceReply(uri, strings.Repeat("\x00", 2<<20))
	})
	session, ctx := connectMemory(t, server)
	if _, err := session.ReadResource(ctx, &mcp.ReadResourceParams{URI: uri}); err == nil || !strings.Contains(err.Error(), "response_too_large") {
		t.Fatalf("oversized resource was not refused: %v", err)
	}
	result, response := call(t, ctx, session, "project_capabilities", map[string]any{})
	if result.IsError || !response.Success {
		t.Fatalf("connection failed after bounded resource error: %+v", response)
	}
}
