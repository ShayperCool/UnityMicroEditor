package mcpserver

import (
	"context"
	"encoding/json"

	"unitymicroeditor/project/authoring"
)

func objectValue(value any) (map[string]any, error) {
	encoded, err := json.Marshal(value)
	if err != nil {
		return nil, err
	}
	var result map[string]any
	if err := json.Unmarshal(encoded, &result); err != nil {
		return nil, err
	}
	return result, nil
}

func callBackend(ctx context.Context, name string, input any, handler RuntimeHandler, failureCode string) (any, error) {
	encoded, err := json.Marshal(input)
	if err != nil {
		return nil, err
	}
	result, err := handler(ctx, name, encoded)
	if err != nil {
		return result, err
	}
	structured, objectErr := objectValue(result)
	if objectErr != nil {
		return result, nil
	}
	status, _ := structured["status"].(string)
	if status == "result_pending" || status == "result_unknown" || status == "outcome_unknown" {
		return structured, nil
	}
	if success, ok := structured["success"].(bool); ok && !success {
		message := "Backend rejected the command"
		if text, ok := structured["error"].(string); ok && text != "" {
			message = text
		}
		if rejection, ok := structured["error"].(map[string]any); ok {
			if text, ok := rejection["message"].(string); ok && text != "" {
				message = text
			}
		}
		return result, &authoring.Error{Code: failureCode, Message: message, Details: structured}
	}
	return result, nil
}
