// Package mcpserver exposes project-bound authoring and optional Player control
// using the official MCP Go SDK over stdio or Streamable HTTP.
package mcpserver

import (
	"context"
	"crypto/subtle"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"strings"
	"time"
	"unicode/utf8"

	"github.com/modelcontextprotocol/go-sdk/mcp"
	"unitymicroeditor/internal/project/authoring"
)

type RuntimeHandler func(context.Context, string, json.RawMessage) (any, error)
type Options struct {
	Project        string
	Transport      string
	Listen         string
	TokenFile      string
	Runtime        RuntimeHandler
	ProjectActions RuntimeHandler
	Backends       any
}

func ParseOptions(args []string) (Options, error) {
	var options Options
	flags := flag.NewFlagSet("mcp", flag.ContinueOnError)
	flags.SetOutput(os.Stderr)
	flags.StringVar(&options.Project, "project", "", "explicit target Unity project root")
	flags.StringVar(&options.Transport, "transport", "stdio", "stdio or http")
	flags.StringVar(&options.Listen, "listen", "127.0.0.1:8090", "loopback HTTP listen address")
	flags.StringVar(&options.TokenFile, "token-file", "", "optional file containing an HTTP bearer token of at least 32 characters")
	if err := flags.Parse(args); err != nil {
		return options, err
	}
	if flags.NArg() != 0 || options.Project == "" {
		return options, errors.New("use mcp --project /absolute/UnityProject [--transport stdio|http]")
	}
	if options.Transport != "stdio" && options.Transport != "http" {
		return options, errors.New("transport must be stdio or http")
	}
	return options, nil
}
func Run(ctx context.Context, args []string) error { return RunWithRuntime(ctx, args, nil) }
func RunWithRuntime(ctx context.Context, args []string, runtime RuntimeHandler) error {
	options, err := ParseOptions(args)
	if err != nil {
		return err
	}
	options.Runtime = runtime
	return RunOptions(ctx, options)
}
func RunOptions(ctx context.Context, options Options) error {
	server, err := New(options)
	if err != nil {
		return err
	}
	if options.Transport == "" || options.Transport == "stdio" {
		return server.Run(ctx, &mcp.StdioTransport{})
	}
	if options.Transport != "http" {
		return errors.New("transport must be stdio or http")
	}
	if options.Listen == "" {
		options.Listen = "127.0.0.1:8090"
	}
	host, _, err := net.SplitHostPort(options.Listen)
	if err != nil {
		return err
	}
	ip := net.ParseIP(host)
	if ip == nil || !ip.IsLoopback() {
		return errors.New("HTTP MCP must listen on a literal loopback IP address")
	}
	token := ""
	if options.TokenFile != "" {
		bytes, err := os.ReadFile(options.TokenFile)
		if err != nil {
			return err
		}
		token = strings.TrimSpace(string(bytes))
		if len(token) < 32 {
			return errors.New("HTTP bearer token must contain at least 32 characters")
		}
	}
	listener, err := net.Listen("tcp", options.Listen)
	if err != nil {
		return err
	}
	defer listener.Close()
	handler := HTTPHandler(server, token)
	httpServer := &http.Server{Handler: handler, ReadHeaderTimeout: 5 * time.Second, IdleTimeout: 90 * time.Second, MaxHeaderBytes: 16 << 10}
	fmt.Fprintf(os.Stderr, "Unity Micro Editor MCP: http://%s/mcp\n", listener.Addr())
	done := make(chan struct{})
	defer close(done)
	go func() {
		select {
		case <-ctx.Done():
			shutdownCtx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
			defer cancel()
			_ = httpServer.Shutdown(shutdownCtx)
		case <-done:
		}
	}()
	err = httpServer.Serve(listener)
	if errors.Is(err, http.ErrServerClosed) {
		return nil
	}
	return err
}

// HTTPHandler also works with httptest. The SDK rejects DNS rebinding requests;
// CrossOriginProtection rejects browser cross-origin requests. Optional bearer
// authentication protects both MCP and health endpoints.
func HTTPHandler(server *mcp.Server, token string) http.Handler {
	mux := http.NewServeMux()
	streamable := mcp.NewStreamableHTTPHandler(func(*http.Request) *mcp.Server { return server }, &mcp.StreamableHTTPOptions{Stateless: true, JSONResponse: true, CrossOriginProtection: http.NewCrossOriginProtection()})
	mux.Handle("/mcp", streamable)
	mux.HandleFunc("GET /health", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		_, _ = io.WriteString(w, `{"status":"ready","service":"unity-micro-editor-mcp"}`)
	})
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if token != "" {
			header := r.Header.Get("Authorization")
			supplied := strings.TrimPrefix(header, "Bearer ")
			if !strings.HasPrefix(header, "Bearer ") || subtle.ConstantTimeCompare([]byte(supplied), []byte(token)) != 1 {
				w.Header().Set("WWW-Authenticate", `Bearer realm="unity-micro-editor"`)
				http.Error(w, "unauthorized", http.StatusUnauthorized)
				return
			}
		}
		mux.ServeHTTP(w, r)
	})
}

type Reply struct {
	Success bool             `json:"success"`
	Status  string           `json:"status,omitempty"`
	Data    any              `json:"data,omitempty"`
	Error   *authoring.Error `json:"error,omitempty"`
}

const maxReplyBytes = 8 << 20

func reply(data any, err error) (*mcp.CallToolResult, Reply, error) {
	out := Reply{Success: err == nil, Data: data}
	if err == nil {
		if structured, ok := data.(map[string]any); ok {
			if status, _ := structured["status"].(string); status == "result_pending" {
				out.Success = false
				out.Status = "pending"
			} else if status == "result_unknown" || status == "outcome_unknown" {
				err = &authoring.Error{Code: "outcome_unknown", Message: "command outcome is unconfirmed; inspect the session and retry the same requestId", Details: structured}
				out.Success = false
				out.Status = "unknown"
			}
		}
	}
	if err != nil {
		out.Data = nil
		var typed *authoring.Error
		if errors.As(err, &typed) {
			out.Error = typed
		} else {
			code := "operation_failed"
			if errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded) {
				code = "cancelled"
			}
			out.Error = &authoring.Error{Code: code, Message: err.Error()}
		}
	}
	encoded, marshalErr := json.Marshal(out)
	if marshalErr != nil {
		return nil, Reply{}, marshalErr
	}
	result := &mcp.CallToolResult{Content: []mcp.Content{&mcp.TextContent{Text: string(encoded)}}, StructuredContent: out, IsError: err != nil}
	wire, marshalErr := json.Marshal(result)
	if marshalErr != nil {
		return nil, Reply{}, marshalErr
	}
	if len(wire) > maxReplyBytes {
		var details map[string]any
		// A size failure after a committed source write must retain the receipt
		// identity; clients can inspect/undo the confirmed revision safely.
		switch value := data.(type) {
		case authoring.EditResult:
			details = map[string]any{"operationId": value.OperationID, "path": value.Path, "revision": value.Revision, "preview": value.Preview}
		case authoring.MutationResult:
			details = map[string]any{"operationId": value.OperationID, "path": value.Path, "revision": value.Revision}
		case authoring.PublicationResult:
			details = map[string]any{"operationId": value.OperationID, "assets": value.Assets, "replayed": value.Replayed}
		case map[string]any:
			details = map[string]any{}
			for _, key := range []string{"requestId", "operationId", "backend", "pid", "revision", "status"} {
				if item, exists := value[key]; exists {
					switch item.(type) {
					case string, int, int64, float64, json.Number:
						details[key] = item
					}
				}
			}
		}
		return reply(nil, &authoring.Error{Code: "response_too_large", Message: "result exceeds the 8 MiB MCP response bound; request smaller source/hierarchy pages, filter objects or set includeFields false", Details: details})
	}
	return result, out, nil
}

func resourceReply(uri string, data any) (*mcp.ReadResourceResult, error) {
	encoded, err := json.Marshal(data)
	if err != nil {
		return nil, err
	}
	result := &mcp.ReadResourceResult{Contents: []*mcp.ResourceContents{{URI: uri, MIMEType: "application/json", Text: string(encoded)}}}
	wire, err := json.Marshal(result)
	if err != nil {
		return nil, err
	}
	if len(wire) > maxReplyBytes {
		return nil, &authoring.Error{Code: "response_too_large", Message: "resource exceeds the 8 MiB MCP response bound; use project_inventory for paged inventory or asset_read/scene_hierarchy for known paths"}
	}
	return result, nil
}

func readTool(name, description string) *mcp.Tool {
	closed := false
	return &mcp.Tool{Name: name, Description: description, Annotations: &mcp.ToolAnnotations{ReadOnlyHint: true, OpenWorldHint: &closed}}
}
func writeTool(name, description string, idempotent bool) *mcp.Tool {
	closed := false
	return &mcp.Tool{Name: name, Description: description, Annotations: &mcp.ToolAnnotations{IdempotentHint: idempotent, OpenWorldHint: &closed}}
}

func New(options Options) (*mcp.Server, error) {
	store, err := authoring.NewStore(options.Project)
	if err != nil {
		return nil, err
	}
	server := mcp.NewServer(&mcp.Implementation{Name: "unity-micro-editor", Version: "0.2.0"}, &mcp.ServerOptions{Instructions: "This service edits authored Unity project files. Read sourceRevision or asset_read revision before each write and send a unique operationId. Resolved hierarchy revision binds the entire prefab dependency graph; composite IDs are read-only and provenance identifies editable local/source IDs. Repeating identical arguments with the same operationId safely replays the original result. Source writes preserve .meta GUIDs. Source write success does not imply compilation, import or Player success. Use explicit source prefab override operations or prefab_edit for source prefab authoring. Read project_capabilities to inspect supported behavior."})
	capabilities := func() map[string]any {
		return map[string]any{"authoring": store.Capabilities(), "nativeComponentTypes": authoring.NativeComponentTypes(), "runtimeConfigured": options.Runtime != nil, "projectActionsConfigured": options.ProjectActions != nil, "backends": options.Backends}
	}
	addTool(server, readTool("project_inspect", "Inspect the explicit target project, Unity profile, scenes, prefabs, scripts, assembly definitions and identity issues."), func(ctx context.Context, _ *mcp.CallToolRequest, _ struct{}) (*mcp.CallToolResult, Reply, error) {
		info, err := store.Inspect(ctx)
		return reply(info, err)
	})
	addTool(server, readTool("project_capabilities", "Read supported authoring operations, guarantees, limitations, and whether runtime control is configured."), func(ctx context.Context, _ *mcp.CallToolRequest, _ struct{}) (*mcp.CallToolResult, Reply, error) {
		return reply(capabilities(), nil)
	})
	addTool(server, readTool("project_inventory", "Read a filtered page of ordinary Assets files, including native inputs, GUIDs, metadata hashes and identity issues. Default100, maximum500. Inventory revision binds paths/stat/metadata; includeSourceRevision provides exact editable digests for supported text assets."), func(ctx context.Context, _ *mcp.CallToolRequest, input authoring.InventoryRequest) (*mcp.CallToolResult, Reply, error) {
		page, err := store.Inventory(ctx, input)
		return reply(page, err)
	})
	addTool(server, readTool("asset_read", "Read exact pages of authored source and metadata with their full revision. Byte offset/limit default 0/262144; metadataOffset/metadataLimit default 0/65536. Follow nextOffset/metadataNextOffset; expectedRevision can bind later pages to the first."), func(ctx context.Context, _ *mcp.CallToolRequest, input ReadInput) (*mcp.CallToolResult, Reply, error) {
		source, err := store.Read(ctx, input.Path)
		if err != nil {
			return reply(nil, err)
		}
		page, err := readPage(source, input)
		return reply(page, err)
	})
	type hierarchyInput struct {
		Path             string `json:"path"`
		Query            string `json:"query,omitempty" jsonschema:"optional case-insensitive name substring or exact GameObject fileID"`
		Offset           int    `json:"offset,omitempty"`
		Limit            int    `json:"limit,omitempty" jsonschema:"object page length, default 100, maximum 500"`
		IncludeFields    *bool  `json:"includeFields,omitempty" jsonschema:"default true; false omits serialized component field payloads"`
		ExpectedRevision string `json:"expectedRevision,omitempty"`
		ResolvePrefabs   *bool  `json:"resolvePrefabs,omitempty" jsonschema:"default true; false returns only authored records and unresolved prefab links"`
	}
	addTool(server, readTool("scene_hierarchy", "Read a page of GameObjects/components, resolving nested authored prefabs and variants by default. Default100, maximum500. Resolved composite IDs are read-only; provenance reports original GUID/fileIDs and editable local IDs. Use sourceRevision for writes, dependencyRevisions for prefab operations and graph revision for subsequent pages. resolvePrefabs:false returns raw authored records."), func(ctx context.Context, _ *mcp.CallToolRequest, input hierarchyInput) (*mcp.CallToolResult, Reply, error) {
		if input.ResolvePrefabs == nil || *input.ResolvePrefabs {
			page, err := store.ResolvedHierarchy(ctx, authoring.HierarchyRequest{Path: input.Path, Query: input.Query, Offset: input.Offset, Limit: input.Limit, ExpectedRevision: input.ExpectedRevision, IncludeFields: input.IncludeFields})
			return reply(page, err)
		}
		if input.Offset < 0 || input.Limit < 0 || input.Limit > 500 {
			return reply(nil, &authoring.Error{Code: "invalid_request", Message: "hierarchy offset must be nonnegative and limit at most 500"})
		}
		if input.Limit == 0 {
			input.Limit = 100
		}
		hierarchy, err := store.Hierarchy(ctx, input.Path, input.Query)
		if err != nil {
			return reply(nil, err)
		}
		if input.ExpectedRevision != "" && hierarchy.Revision != input.ExpectedRevision {
			return reply(nil, &authoring.Error{Code: "revision_conflict", Message: "authored scene or metadata changed between hierarchy pages", Details: map[string]any{"actualRevision": hierarchy.Revision, "expectedRevision": input.ExpectedRevision}})
		}
		total := len(hierarchy.Objects)
		if input.Offset > total {
			return reply(nil, &authoring.Error{Code: "invalid_request", Message: "hierarchy offset exceeds filtered object count"})
		}
		end := input.Offset + input.Limit
		if end > total {
			end = total
		}
		hierarchy.Objects = hierarchy.Objects[input.Offset:end]
		if input.IncludeFields != nil && !*input.IncludeFields {
			for i := range hierarchy.Objects {
				for j := range hierarchy.Objects[i].Components {
					hierarchy.Objects[i].Components[j].Fields = nil
				}
			}
		}
		var next *int
		if end < total {
			next = &end
		}
		return reply(struct {
			authoring.Hierarchy
			Total      int  `json:"total"`
			Offset     int  `json:"offset"`
			NextOffset *int `json:"nextOffset"`
		}{hierarchy, total, input.Offset, next}, nil)
	})
	addTool(server, writeTool("scene_edit", "Apply a revision-checked source transaction or preview. Actions: create_gameobject (optional explicit rectTransform fields), delete_gameobject, reparent, set_property (scalar/reference/collection), add_component (scriptPath or supported componentType/classId with explicit fields/template), remove_component, instantiate_prefab, set_prefab_override, revert_prefab_override. Creation keys use $key/$key.transform/$key.instance. Reparent keeps local transform; prefab operations bind dependencyRevisions. Complex prefab operations use prefab_edit. Unrelated YAML spans and metadata remain intact.", true), func(ctx context.Context, _ *mcp.CallToolRequest, input authoring.EditRequest) (*mcp.CallToolResult, Reply, error) {
		result, err := store.Edit(ctx, input)
		return reply(result, err)
	})
	addTool(server, writeTool("asset_create", "Create an absent .mat, ScriptableObject .asset or authored .prefab from explicit Unity YAML plus a new GUID-preserving metadata identity. expectedRevision must be absent; parent directory exists. Does not invent shader/script defaults or confirm runtime loading.", true), func(ctx context.Context, _ *mcp.CallToolRequest, input authoring.AssetCreateRequest) (*mcp.CallToolResult, Reply, error) {
		result, err := store.CreateAsset(ctx, input)
		return reply(result, err)
	})
	addTool(server, readTool("script_read", "Read a C# source page, existing metadata GUID and full revision. Paging is identical to asset_read."), func(ctx context.Context, _ *mcp.CallToolRequest, input ReadInput) (*mcp.CallToolResult, Reply, error) {
		source, err := store.ReadScript(ctx, input.Path)
		if err != nil {
			return reply(nil, err)
		}
		page, err := readPage(source, input)
		return reply(page, err)
	})
	addTool(server, writeTool("script_create", "Create a new C# file and MonoImporter .meta transactionally. expectedRevision must be absent; parent directory must exist. Does not run the compiler.", true), func(ctx context.Context, _ *mcp.CallToolRequest, input authoring.ScriptRequest) (*mcp.CallToolResult, Reply, error) {
		result, err := store.CreateScript(ctx, input)
		return reply(result, err)
	})
	addTool(server, writeTool("script_edit", "Replace C# source using expectedRevision and operationId; preserve existing .meta byte-for-byte. Does not run the compiler.", true), func(ctx context.Context, _ *mcp.CallToolRequest, input authoring.ScriptRequest) (*mcp.CallToolResult, Reply, error) {
		result, err := store.EditScript(ctx, input)
		return reply(result, err)
	})
	type undoInput struct {
		Path                string            `json:"path,omitempty"`
		ExpectedRevision    string            `json:"expectedRevision,omitempty"`
		OperationID         string            `json:"operationId"`
		OriginalOperationID string            `json:"originalOperationId"`
		ExpectedRevisions   map[string]string `json:"expectedRevisions,omitempty" jsonschema:"for a staged multi-file publication; one exact resulting revision per changed asset"`
	}
	addTool(server, writeTool("source_undo", "Undo an existing-file edit with path/expectedRevision, or a staged multi-file publication with expectedRevisions. Supply originalOperationId and new operationId. Refuses intervening changes, referenced-object deletion and erasing newly created files.", true), func(ctx context.Context, _ *mcp.CallToolRequest, input undoInput) (*mcp.CallToolResult, Reply, error) {
		if input.Path == "" || len(input.ExpectedRevisions) > 0 {
			result, err := store.UndoPublication(ctx, authoring.PublicationUndoRequest{OperationID: input.OperationID, OriginalOperationID: input.OriginalOperationID, ExpectedRevisions: input.ExpectedRevisions})
			return reply(result, err)
		}
		result, err := store.Undo(ctx, authoring.UndoRequest{Path: input.Path, ExpectedRevision: input.ExpectedRevision, OperationID: input.OperationID, OriginalOperationID: input.OriginalOperationID})
		return reply(result, err)
	})
	server.AddResource(&mcp.Resource{URI: "unitymicroeditor://project", Name: "Bound Unity project", MIMEType: "application/json"}, func(ctx context.Context, _ *mcp.ReadResourceRequest) (*mcp.ReadResourceResult, error) {
		info, err := store.Inspect(ctx)
		if err != nil {
			return nil, err
		}
		return resourceReply("unitymicroeditor://project", info)
	})
	server.AddResource(&mcp.Resource{URI: "unitymicroeditor://capabilities", Name: "Authoring capabilities", MIMEType: "application/json"}, func(ctx context.Context, _ *mcp.ReadResourceRequest) (*mcp.ReadResourceResult, error) {
		return resourceReply("unitymicroeditor://capabilities", capabilities())
	})
	if options.Runtime != nil {
		addRuntimeTools(server, options.Runtime)
	}
	if options.ProjectActions != nil {
		addProjectActions(server, options.ProjectActions)
	}
	return server, nil
}

type ReadInput struct {
	Path             string `json:"path" jsonschema:"normalized authored path beneath Assets"`
	Offset           int    `json:"offset,omitempty" jsonschema:"UTF-8 byte offset, default zero"`
	Limit            int    `json:"limit,omitempty" jsonschema:"source bytes per page, default262144, minimum4 maximum1048576"`
	MetadataOffset   int    `json:"metadataOffset,omitempty"`
	MetadataLimit    int    `json:"metadataLimit,omitempty" jsonschema:"metadata bytes per page, default65536, minimum4 maximum1048576"`
	ExpectedRevision string `json:"expectedRevision,omitempty"`
}
type SourcePage struct {
	authoring.Source
	Offset             int  `json:"offset"`
	FullLength         int  `json:"fullLength"`
	NextOffset         *int `json:"nextOffset"`
	Truncated          bool `json:"truncated"`
	MetadataOffset     int  `json:"metadataOffset"`
	MetadataFullLength int  `json:"metadataFullLength"`
	MetadataNextOffset *int `json:"metadataNextOffset"`
	MetadataTruncated  bool `json:"metadataTruncated"`
}

func sliceText(text string, offset, limit, defaultLimit int) (string, *int, bool, error) {
	if limit == 0 {
		limit = defaultLimit
	}
	if offset < 0 || offset > len(text) || limit < 4 || limit > 1<<20 {
		return "", nil, false, &authoring.Error{Code: "invalid_request", Message: "read offset must be within the text and page limit from4 to1048576 bytes"}
	}
	if offset < len(text) && !utf8.RuneStart(text[offset]) {
		return "", nil, false, &authoring.Error{Code: "invalid_request", Message: "read offset must be a UTF-8 character boundary"}
	}
	end := offset + limit
	if end > len(text) {
		end = len(text)
	}
	for end < len(text) && !utf8.RuneStart(text[end]) {
		end--
	}
	var next *int
	if end < len(text) {
		next = &end
	}
	return text[offset:end], next, offset != 0 || end < len(text), nil
}
func readPage(source authoring.Source, input ReadInput) (SourcePage, error) {
	if input.ExpectedRevision != "" && source.Revision != input.ExpectedRevision {
		return SourcePage{}, &authoring.Error{Code: "revision_conflict", Message: "authored source or metadata changed between pages", Details: map[string]any{"actualRevision": source.Revision, "expectedRevision": input.ExpectedRevision}}
	}
	page := SourcePage{Source: source, Offset: input.Offset, FullLength: len(source.Content), MetadataOffset: input.MetadataOffset, MetadataFullLength: len(source.Metadata)}
	var err error
	page.Content, page.NextOffset, page.Truncated, err = sliceText(source.Content, input.Offset, input.Limit, 262144)
	if err != nil {
		return SourcePage{}, err
	}
	page.Metadata, page.MetadataNextOffset, page.MetadataTruncated, err = sliceText(source.Metadata, input.MetadataOffset, input.MetadataLimit, 65536)
	return page, err
}

type SessionInput struct {
	Action           string         `json:"action" jsonschema:"start,stop,status,reset,wait,wait_update"`
	AfterSequence    int64          `json:"afterSequence,omitempty"`
	RequestID        string         `json:"requestId,omitempty"`
	ExpectedPID      int            `json:"expectedPid,omitempty"`
	ExpectedRevision string         `json:"expectedRevision,omitempty"`
	Arguments        map[string]any `json:"arguments,omitempty"`
}
type RuntimeInput struct {
	Operation        string         `json:"operation" jsonschema:"status,hierarchy,inspect,logs,raycast,tools,screenshot,pause,resume,wait_frames,custom,invoke,simulation_pause,simulation_resume,simulation_step,input"`
	Arguments        map[string]any `json:"arguments,omitempty"`
	RequestID        string         `json:"requestId,omitempty"`
	ExpectedPID      int            `json:"expectedPid,omitempty"`
	ExpectedRevision string         `json:"expectedRevision,omitempty"`
}

func addRuntimeTools(server *mcp.Server, handler RuntimeHandler) {
	catalog := addProjectTools(server, handler)
	forward := func(ctx context.Context, name string, input any) (*mcp.CallToolResult, Reply, error) {
		result, err := callBackend(ctx, name, input, handler, "runtime_rejected")
		if session, ok := input.(SessionInput); ok {
			if session.Action == "stop" || session.Action == "reset" {
				catalog.clear()
			}
			if err == nil && (session.Action == "wait" || session.Action == "wait_update") {
				result = catalog.refreshReady(ctx, result)
			}
		}
		return reply(result, err)
	}
	addTool(server, writeTool("session_control", "Control the configured Player session: start, stop, status, reset, wait, wait_update. Start can be asynchronous; wait for confirmed readiness. expectedPid/revision guard stale runtime sessions.", false), func(ctx context.Context, _ *mcp.CallToolRequest, input SessionInput) (*mcp.CallToolResult, Reply, error) {
		switch input.Action {
		case "start", "stop", "status", "reset", "wait", "wait_update":
			return forward(ctx, "session_control", input)
		default:
			return reply(nil, &authoring.Error{Code: "unsupported_operation", Message: "unknown session action"})
		}
	})
	addTool(server, readTool("runtime_query", "Query configured Player via public Unity APIs. Read-only operations: status, hierarchy, inspect, logs, raycast, tools. tools refreshes the public project.<name> catalog. Object references must be validated against the current session PID/revision."), func(ctx context.Context, _ *mcp.CallToolRequest, input RuntimeInput) (*mcp.CallToolResult, Reply, error) {
		switch input.Operation {
		case "tools":
			data, err := catalog.refresh(ctx, ProjectToolsInput{RequestID: input.RequestID, ExpectedPID: input.ExpectedPID, ExpectedRevision: input.ExpectedRevision})
			return reply(data, err)
		case "status", "hierarchy", "inspect", "logs", "raycast":
			return forward(ctx, "runtime_query", input)
		default:
			return reply(nil, &authoring.Error{Code: "unsupported_operation", Message: "runtime_query permits only status, hierarchy, inspect, logs, raycast and tools"})
		}
	})
	addTool(server, writeTool("runtime_command", "Run configured Player operations: screenshot,pause,resume,wait_frames,custom,invoke,simulation_pause,simulation_resume,simulation_step,input. screenshot defaults to the full viewport with overlay UI; arguments.mode=camera captures a selected camera with optional width/height. pause affects scaled time; simulation_* uses the installed game-loop gate. invoke calls only public attributed/registered tools. input uses the configured synthetic device path. Supply requestId and current PID/revision; pending/unknown requires retry of the SAME requestId.", false), func(ctx context.Context, _ *mcp.CallToolRequest, input RuntimeInput) (*mcp.CallToolResult, Reply, error) {
		switch input.Operation {
		case "screenshot", "pause", "resume", "wait_frames", "custom", "invoke", "simulation_pause", "simulation_resume", "simulation_step", "input":
			return forward(ctx, "runtime_command", input)
		default:
			return reply(nil, &authoring.Error{Code: "unsupported_operation", Message: "unknown runtime command"})
		}
	})
}
