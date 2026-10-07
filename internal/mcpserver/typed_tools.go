package mcpserver

import (
	"bytes"
	"context"
	"encoding/json"
	"reflect"

	"github.com/google/jsonschema-go/jsonschema"
	"github.com/modelcontextprotocol/go-sdk/mcp"
	"unitymicroeditor/internal/project/authoring"
)

// addTool keeps original JSON numbers and RawMessage values intact. The SDK's
// generic AddTool applies schema defaults through map[string]any and marshals
// that map again, which rounds integers beyond the float64 precision limit.
// These schemas have no defaults; validate a separate copy, then decode the
// original arguments and return the already bounded reply directly.
func addTool[In any](server *mcp.Server, tool *mcp.Tool, handler func(context.Context, *mcp.CallToolRequest, In) (*mcp.CallToolResult, Reply, error)) {
	schema, err := jsonschema.For[In](&jsonschema.ForOptions{TypeSchemas: map[reflect.Type]*jsonschema.Schema{
		reflect.TypeFor[json.RawMessage](): {},
	}})
	if err != nil {
		panic(err) // Registration uses fixed package types, as SDK AddTool does.
	}
	if provided, ok := tool.InputSchema.(*jsonschema.Schema); ok {
		schema = provided
	}
	resolved, err := schema.Resolve(nil)
	if err != nil {
		panic(err)
	}
	tool.InputSchema = schema
	tool.OutputSchema, err = jsonschema.For[Reply](nil)
	if err != nil {
		panic(err)
	}
	server.AddTool(tool, func(ctx context.Context, request *mcp.CallToolRequest) (*mcp.CallToolResult, error) {
		raw := request.Params.Arguments
		if len(raw) == 0 {
			raw = json.RawMessage(`{}`)
		}
		var validation any
		err := json.Unmarshal(raw, &validation)
		if err == nil {
			err = resolved.Validate(validation)
		}
		if err != nil {
			result, _, marshalErr := reply(nil, &authoring.Error{Code: "invalid_request", Message: err.Error()})
			return result, marshalErr
		}
		var input In
		decoder := json.NewDecoder(bytes.NewReader(raw))
		decoder.UseNumber()
		decoder.DisallowUnknownFields()
		if err := decoder.Decode(&input); err != nil {
			result, _, marshalErr := reply(nil, &authoring.Error{Code: "invalid_request", Message: err.Error()})
			return result, marshalErr
		}
		result, _, err := handler(ctx, request, input)
		return result, err
	})
}
