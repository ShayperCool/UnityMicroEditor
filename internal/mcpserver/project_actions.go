package mcpserver

import (
	"context"
	"encoding/json"
	"slices"

	"github.com/google/jsonschema-go/jsonschema"
	"github.com/modelcontextprotocol/go-sdk/mcp"
)

type ProjectPrepareInput struct {
	Scene         string `json:"scene" jsonschema:"authored Assets/...unity scene to prepare"`
	RequestID     string `json:"requestId"`
	HostPlayer    string `json:"hostPlayer,omitempty" jsonschema:"optional explicitly compatible native Mono host Player"`
	NativeCatalog string `json:"nativeCatalog,omitempty" jsonschema:"optional verified native import catalog for this project/profile"`
	EngineProfile string `json:"engineProfile,omitempty" jsonschema:"optional cached pinned native engine/host profile; target source graph is built by Go"`
	Activate      *bool  `json:"activate,omitempty" jsonschema:"whether to activate the resulting baseline in this controller"`
}

type ProjectBuildInput struct {
	Scenes      []string `json:"scenes" jsonschema:"explicit authored scenes for the public Unity build backend"`
	Development bool     `json:"development"`
	RequestID   string   `json:"requestId"`
}

type PrefabEditInput struct {
	Action           string          `json:"action" jsonschema:"instantiate,create_prefab,create_variant,apply,revert,unpack,set_override,add_component"`
	Path             string          `json:"path" jsonschema:"authored Assets scene or prefab to edit"`
	ExpectedRevision string          `json:"expectedRevision"`
	OperationID      string          `json:"operationId"`
	RequestID        string          `json:"requestId,omitempty"`
	FileID           string          `json:"fileId,omitempty"`
	TargetPath       string          `json:"targetPath,omitempty"`
	PrefabPath       string          `json:"prefabPath,omitempty"`
	OutputPath       string          `json:"outputPath,omitempty"`
	ParentFileID     string          `json:"parentFileId,omitempty"`
	ParentPath       string          `json:"parentPath,omitempty"`
	Name             string          `json:"name,omitempty"`
	Completely       bool            `json:"completely,omitempty"`
	ComponentType    string          `json:"componentType,omitempty"`
	Property         string          `json:"property,omitempty"`
	Value            json.RawMessage `json:"value,omitempty"`
	Fields           map[string]any  `json:"fields,omitempty"`
}

type ComponentSchemaInput struct {
	ComponentType string `json:"componentType"`
	RequestID     string `json:"requestId"`
}

type ComponentAddInput struct {
	Path             string         `json:"path"`
	FileID           string         `json:"fileId"`
	ComponentType    string         `json:"componentType"`
	ExpectedRevision string         `json:"expectedRevision"`
	OperationID      string         `json:"operationId"`
	RequestID        string         `json:"requestId,omitempty"`
	Fields           map[string]any `json:"fields,omitempty" jsonschema:"explicit serialized field overrides applied to publicly captured native defaults"`
}

func addProjectActions(server *mcp.Server, handler RuntimeHandler) {
	forward := func(ctx context.Context, name string, input any) (*mcp.CallToolResult, Reply, error) {
		result, err := callBackend(ctx, name, input, handler, "project_action_rejected")
		return reply(result, err)
	}
	addTool(server, writeTool("project_prepare", "Prepare an explicit saved scene using our package resolver, authored Player assembly graph, source/native-resource scene validation and the SDK compiler. Reuses a verified native host/profile/catalog; initial native bootstrap or uncovered imports may require the explicit Editor backend. Poll identical arguments/requestId while pending. Preparation completion is separate from Player readiness; activate can select the result for this controller.", true), func(ctx context.Context, _ *mcp.CallToolRequest, input ProjectPrepareInput) (*mcp.CallToolResult, Reply, error) {
		return forward(ctx, "project_prepare", input)
	})
	buildTool := writeTool("project_build", "Build explicit authored scenes through the public Unity Editor build backend into a new owned output directory. development defaults to false for a release build. Poll identical arguments/requestId while status is pending. This invokes Unity BuildPipeline and reports its actual result.", true)
	buildSchema, err := jsonschema.For[ProjectBuildInput](nil)
	if err != nil {
		panic(err)
	}
	// Optional input with an explicit false on the backend wire. omitempty on
	// the DTO would erase release mode while forwarding typed arguments.
	buildSchema.Required = slices.DeleteFunc(buildSchema.Required, func(name string) bool { return name == "development" })
	buildTool.InputSchema = buildSchema
	addTool(server, buildTool, func(ctx context.Context, _ *mcp.CallToolRequest, input ProjectBuildInput) (*mcp.CallToolResult, Reply, error) {
		return forward(ctx, "project_build", input)
	})
	addTool(server, writeTool("prefab_edit", "Create, instantiate, edit, apply/revert overrides or unpack authored prefabs through our Go source resolver and multi-file CAS journal. add_component uses a version-bound cached native template. Backend raw_source; no Editor is started. expectedRevision/operationId are required; poll identical arguments/operationId while status is pending. Target by fileId or targetPath. Unsupported unresolved/model or structural changes fail explicitly; publication does not imply runtime import/compile success.", true), func(ctx context.Context, _ *mcp.CallToolRequest, input PrefabEditInput) (*mcp.CallToolResult, Reply, error) {
		return forward(ctx, "prefab_edit", input)
	})
	addTool(server, readTool("component_schema", "Read a supported native component's serialized defaults and required companions from the exact version/revision/native-context cache. Never starts Editor. A missing cache requires one explicit cache-component-defaults capture. Does not change authored target Assets."), func(ctx context.Context, _ *mcp.CallToolRequest, input ComponentSchemaInput) (*mcp.CallToolResult, Reply, error) {
		return forward(ctx, "component_schema", input)
	})
	addTool(server, writeTool("component_add", "Attach a supported native component using version-bound cached defaults and required companions in one Go source transaction. Never starts Editor; preserves existing GUIDs and Transform identity for RectTransform replacement. Poll identical arguments/operationId while pending. Prefab instance edits require prefab_edit. Does not imply runtime import/compile success.", true), func(ctx context.Context, _ *mcp.CallToolRequest, input ComponentAddInput) (*mcp.CallToolResult, Reply, error) {
		return forward(ctx, "component_add", input)
	})
}
