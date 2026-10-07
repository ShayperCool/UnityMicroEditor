package mcpserver

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"regexp"
	"sync"
	"time"

	"github.com/google/jsonschema-go/jsonschema"
	"github.com/modelcontextprotocol/go-sdk/mcp"
	"unitymicroeditor/project/authoring"
)

type ProjectToolsInput struct {
	RequestID        string `json:"requestId,omitempty"`
	ExpectedPID      int    `json:"expectedPid,omitempty"`
	ExpectedRevision string `json:"expectedRevision,omitempty"`
}

type projectToolInput struct {
	Parameters       json.RawMessage `json:"parameters"`
	Target           json.RawMessage `json:"target,omitempty"`
	RequestID        string          `json:"requestId,omitempty"`
	ExpectedPID      int             `json:"expectedPid,omitempty"`
	ExpectedRevision string          `json:"expectedRevision,omitempty"`
}

type projectToolCatalog struct {
	mu           sync.Mutex
	server       *mcp.Server
	handler      RuntimeHandler
	fingerprints map[string]string
}

var projectToolName = regexp.MustCompile(`^[A-Za-z0-9_.-]{1,120}$`)

func (catalog *projectToolCatalog) clear() {
	catalog.mu.Lock()
	defer catalog.mu.Unlock()
	catalog.clearLocked()
}

func (catalog *projectToolCatalog) clearLocked() {
	var names []string
	for name := range catalog.fingerprints {
		names = append(names, name)
	}
	catalog.server.RemoveTools(names...)
	catalog.fingerprints = map[string]string{}
}

func (catalog *projectToolCatalog) refresh(ctx context.Context, input ProjectToolsInput) (any, error) {
	catalog.mu.Lock()
	defer catalog.mu.Unlock()
	result, err := callBackend(ctx, "runtime_query", RuntimeInput{Operation: "tools", RequestID: input.RequestID, ExpectedPID: input.ExpectedPID, ExpectedRevision: input.ExpectedRevision}, catalog.handler, "runtime_rejected")
	if err != nil {
		catalog.clearLocked()
		return result, err
	}
	receipt, err := objectValue(result)
	if err != nil {
		catalog.clearLocked()
		return nil, err
	}
	if status, _ := receipt["status"].(string); status == "result_pending" || status == "result_unknown" || status == "outcome_unknown" {
		catalog.clearLocked()
		return receipt, nil
	}
	payload := receipt
	if nested, ok := receipt["result"].(map[string]any); ok {
		payload = nested
	}
	descriptors, ok := payload["tools"].([]any)
	if !ok {
		catalog.clearLocked()
		return nil, &authoring.Error{Code: "invalid_tool_catalog", Message: "runtime tools receipt requires a tools array"}
	}
	if len(descriptors) > 256 {
		catalog.clearLocked()
		return nil, &authoring.Error{Code: "tool_catalog_too_large", Message: "runtime tool catalog exceeds the 256-tool bound"}
	}
	pid := input.ExpectedPID
	if value, ok := receipt["pid"].(float64); ok {
		pid = int(value)
	}
	revision := input.ExpectedRevision
	if value, ok := receipt["revision"].(string); ok {
		revision = value
	}
	if pid <= 0 || revision == "" {
		catalog.clearLocked()
		return nil, &authoring.Error{Code: "invalid_tool_catalog", Message: "runtime tools receipt requires a Player PID and revision"}
	}
	counts := map[string]int{}
	for _, descriptor := range descriptors {
		if value, ok := descriptor.(map[string]any); ok {
			if name, ok := value["name"].(string); ok {
				counts[name]++
			}
		}
	}
	accepted := map[string]string{}
	errors := []any{}
	for _, descriptor := range descriptors {
		value, ok := descriptor.(map[string]any)
		if !ok {
			errors = append(errors, map[string]any{"code": "invalid_descriptor", "message": "tool descriptor must be an object"})
			continue
		}
		name, _ := value["name"].(string)
		if !projectToolName.MatchString(name) || counts[name] != 1 {
			errors = append(errors, map[string]any{"name": name, "code": "invalid_name", "message": "MCP project tool names require unique 1..120 ASCII letters/digits/dots/underscores/hyphens"})
			continue
		}
		parameterSchema, ok := value["inputSchema"].(map[string]any)
		if !ok || parameterSchema["type"] != "object" {
			errors = append(errors, map[string]any{"name": name, "code": "invalid_schema", "message": "DTO input schema must have object type"})
			continue
		}
		readOnly, _ := value["readOnly"].(bool)
		binding, _ := value["binding"].(string)
		requiresTarget := binding == "component"
		if value, ok := value["requiresTarget"].(bool); ok {
			requiresTarget = requiresTarget || value
		}
		if binding != "static" && binding != "component" && binding != "legacy" {
			errors = append(errors, map[string]any{"name": name, "code": "invalid_binding", "message": "tool binding must be static,component or legacy"})
			continue
		}
		properties := map[string]any{"parameters": parameterSchema, "requestId": map[string]any{"type": "string", "minLength": 1, "maxLength": 64, "pattern": "^[A-Za-z0-9-]+$"}, "expectedPid": map[string]any{"type": "integer", "minimum": 1}, "expectedRevision": map[string]any{"type": "string", "minLength": 1, "maxLength": 128}}
		required := []string{"parameters"}
		if !readOnly {
			required = append(required, "requestId")
		}
		if requiresTarget {
			properties["target"] = map[string]any{"type": "object", "additionalProperties": false, "properties": map[string]any{"path": map[string]any{"type": "string", "minLength": 1, "maxLength": 2048}, "fileId": map[string]any{"type": "string", "minLength": 1, "maxLength": 2048}}, "oneOf": []any{map[string]any{"required": []string{"path"}}, map[string]any{"required": []string{"fileId"}}}}
			required = append(required, "target")
		}
		inputSchema := map[string]any{"type": "object", "additionalProperties": false, "properties": properties, "required": required}
		encoded, err := json.Marshal(inputSchema)
		if err != nil {
			return nil, err
		}
		if len(encoded) > 1<<20 {
			errors = append(errors, map[string]any{"name": name, "code": "schema_too_large", "message": "DTO wrapper schema exceeds one MiB"})
			continue
		}
		var schema jsonschema.Schema
		if err := json.Unmarshal(encoded, &schema); err != nil {
			errors = append(errors, map[string]any{"name": name, "code": "invalid_schema", "message": err.Error()})
			continue
		}
		resolved, err := schema.Resolve(nil)
		if err != nil {
			errors = append(errors, map[string]any{"name": name, "code": "invalid_schema", "message": err.Error()})
			continue
		}
		mcpName := "project." + name
		encodedDescriptor, err := json.Marshal(struct {
			Descriptor any
			PID        int
			Revision   string
		}{value, pid, revision})
		if err != nil {
			return nil, err
		}
		digest := sha256.Sum256(encodedDescriptor)
		fingerprint := hex.EncodeToString(digest[:])
		accepted[mcpName] = fingerprint
		if catalog.fingerprints[mcpName] == fingerprint {
			continue
		}
		description, _ := value["description"].(string)
		if len(description) > 65536 {
			description = description[:65536]
		}
		tool := writeTool(mcpName, description+" Invoke a publicly attributed or registered project method. parameters follows its DTO schema; target is required for component methods. This wrapper is pinned to the catalog Player PID/revision when caller guards are omitted.", false)
		if readOnly {
			tool = readTool(mcpName, tool.Description)
		}
		tool.InputSchema = inputSchema
		catalog.server.AddTool(tool, func(ctx context.Context, request *mcp.CallToolRequest) (*mcp.CallToolResult, error) {
			raw := request.Params.Arguments
			var validation any
			if err := json.Unmarshal(raw, &validation); err != nil {
				result, _, _ := reply(nil, &authoring.Error{Code: "invalid_request", Message: err.Error()})
				return result, nil
			}
			if err := resolved.Validate(validation); err != nil {
				result, _, _ := reply(nil, &authoring.Error{Code: "invalid_request", Message: err.Error()})
				return result, nil
			}
			var input projectToolInput
			decoder := json.NewDecoder(bytes.NewReader(raw))
			decoder.DisallowUnknownFields()
			if err := decoder.Decode(&input); err != nil {
				result, _, _ := reply(nil, &authoring.Error{Code: "invalid_request", Message: err.Error()})
				return result, nil
			}
			if input.ExpectedPID == 0 {
				input.ExpectedPID = pid
			}
			if input.ExpectedRevision == "" {
				input.ExpectedRevision = revision
			}
			arguments := map[string]any{"name": name, "parameters": input.Parameters}
			if len(input.Target) > 0 {
				arguments["target"] = input.Target
			}
			data, err := callBackend(ctx, "runtime_command", RuntimeInput{Operation: "invoke", Arguments: arguments, RequestID: input.RequestID, ExpectedPID: input.ExpectedPID, ExpectedRevision: input.ExpectedRevision}, catalog.handler, "runtime_rejected")
			result, _, marshalErr := reply(data, err)
			return result, marshalErr
		})
	}
	var removed []string
	for name := range catalog.fingerprints {
		if _, exists := accepted[name]; !exists {
			removed = append(removed, name)
		}
	}
	catalog.server.RemoveTools(removed...)
	catalog.fingerprints = accepted
	if len(errors) > 0 {
		payload["mcpCatalogErrors"] = errors
	}
	payload["mcpRegisteredCount"] = len(accepted)
	return receipt, nil
}

func (catalog *projectToolCatalog) refreshReady(ctx context.Context, result any) any {
	state, err := objectValue(result)
	if err != nil || state["phase"] != "ready" {
		return result
	}
	pid, _ := state["pid"].(float64)
	revision, _ := state["revision"].(string)
	refreshContext, cancel := context.WithTimeout(ctx, 5*time.Second)
	defer cancel()
	data, err := catalog.refresh(refreshContext, ProjectToolsInput{ExpectedPID: int(pid), ExpectedRevision: revision})
	if err != nil {
		catalog.clear()
		state["projectToolsRefreshError"] = err.Error()
	} else {
		receipt, _ := objectValue(data)
		if status, _ := receipt["status"].(string); status != "" {
			catalog.clear()
			state["projectToolsRefreshStatus"] = status
		} else {
			state["projectToolsRefreshed"] = true
		}
	}
	return state
}

func addProjectTools(server *mcp.Server, handler RuntimeHandler) *projectToolCatalog {
	catalog := &projectToolCatalog{server: server, handler: handler, fingerprints: map[string]string{}}
	mcp.AddTool(server, readTool("project_tools", "Discover public attributed/registered project tools from the current Player, publish project.<name> MCP tools using exact DTO parameter schemas and readonly hints, and remove stale catalog entries. Discovery errors remain visible in the runtime receipt."), func(ctx context.Context, _ *mcp.CallToolRequest, input ProjectToolsInput) (*mcp.CallToolResult, Reply, error) {
		data, err := catalog.refresh(ctx, input)
		return reply(data, err)
	})
	return catalog
}
