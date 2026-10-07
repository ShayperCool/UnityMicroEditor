package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"

	"unitymicroeditor/internal/project/authoring"
)

// Only revision-checked project coordinators may run as detached jobs. Player
// requests retain the controller's receipt protocol and are never replayed here.
func agentAsyncAction(action string) bool {
	switch action {
	case "project_prepare", "project_build", "prefab_edit", "component_add", "component_schema":
		return true
	default:
		return false
	}
}

type agentActionJobRequest struct {
	SchemaVersion int             `json:"schemaVersion"`
	Project       string          `json:"project"`
	Action        string          `json:"action"`
	Identity      string          `json:"identity"`
	Arguments     json.RawMessage `json:"arguments"`
}

type agentActionJobReceipt struct {
	SchemaVersion      int              `json:"schemaVersion"`
	RequestFingerprint string           `json:"requestFingerprint"`
	Result             json.RawMessage  `json:"result,omitempty"`
	Error              *authoring.Error `json:"error,omitempty"`
}

type agentActionJob struct {
	active  bool
	receipt *agentActionJobReceipt
}

func actionJobPending(action, id, directory string, args map[string]json.RawMessage) map[string]any {
	result := map[string]any{"success": false, "status": "result_pending", "backend": "project_action_job", "action": action, "requestId": id, "jobStatusPath": filepath.Join(directory, "result.json")}
	if len(args["operationId"]) > 0 {
		result["operationId"] = id
	}
	return result
}

func (s *agentProjectService) enqueueAction(ctx context.Context, action string, raw json.RawMessage, run func(context.Context, string, json.RawMessage) (any, error)) (any, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	if !agentAsyncAction(action) || len(raw) > 1<<20 {
		return nil, agentFailure("invalid_request", "unsupported or oversized detached project action")
	}
	var args map[string]json.RawMessage
	if err := json.Unmarshal(raw, &args); err != nil || args == nil {
		return nil, agentFailure("invalid_request", "project arguments must be an object")
	}
	idField := "requestId"
	if action == "prefab_edit" || action == "component_add" {
		idField = "operationId"
	}
	var id string
	if err := json.Unmarshal(args[idField], &id); err != nil || !agentOperationID.MatchString(id) {
		return nil, agentFailure("invalid_request", idField+" must be at most 64 ASCII letters/digits/hyphens for a detached project action")
	}
	normalized, err := json.Marshal(args)
	if err != nil {
		return nil, err
	}
	directory := filepath.Join(s.project, "Library/UnityMicroEditor/action-jobs", bytesSHA([]byte(id)))
	request := agentActionJobRequest{SchemaVersion: 1, Project: s.project, Action: action, Identity: id, Arguments: normalized}
	requestBytes, err := json.Marshal(request)
	if err != nil {
		return nil, err
	}
	fingerprint := bytesSHA(requestBytes)

	s.jobMu.Lock()
	defer s.jobMu.Unlock()
	if s.jobClosed {
		return nil, context.Canceled
	}
	if err := agentImmutableJSON(filepath.Join(directory, "request.json"), request); err != nil {
		return nil, err
	}
	var persisted agentActionJobReceipt
	if err := agentReadJSON(filepath.Join(directory, "result.json"), 80<<20, &persisted); err == nil {
		if persisted.SchemaVersion != 1 || persisted.RequestFingerprint != fingerprint {
			return nil, agentFailure("invalid_job_receipt", "project job receipt has an unsupported schema")
		}
		return persisted.value()
	} else if !errors.Is(err, os.ErrNotExist) {
		return nil, err
	}
	if s.jobs == nil {
		s.jobs = map[string]*agentActionJob{}
	}
	if job := s.jobs[id]; job != nil {
		if job.receipt != nil {
			return job.receipt.value()
		}
		if job.active {
			return actionJobPending(action, id, directory, args), nil
		}
	}
	if len(s.jobs) >= 128 {
		for key, job := range s.jobs {
			if !job.active {
				delete(s.jobs, key)
			}
		}
		if len(s.jobs) >= 128 {
			return nil, agentFailure("too_many_action_jobs", "at most 128 project action jobs may be active in this service")
		}
	}
	if s.jobContext == nil {
		lifetime := s.lifetime
		if lifetime == nil {
			lifetime = context.Background()
		}
		s.jobContext, s.jobCancel = context.WithCancel(lifetime)
	}
	if err := s.jobContext.Err(); err != nil {
		return nil, err
	}
	job := &agentActionJob{active: true}
	s.jobs[id] = job
	s.jobWait.Add(1)
	go func() {
		defer s.jobWait.Done()
		value, runErr := run(s.jobContext, action, normalized)
		receipt, pending := makeActionJobReceipt(value, runErr)
		receipt.RequestFingerprint = fingerprint
		if !pending {
			if writeErr := agentImmutableJSON(filepath.Join(directory, "result.json"), receipt); writeErr != nil {
				var winner agentActionJobReceipt
				if readErr := agentReadJSON(filepath.Join(directory, "result.json"), 80<<20, &winner); readErr == nil && winner.SchemaVersion == 1 && winner.RequestFingerprint == fingerprint {
					receipt = winner
				} else {
					receipt.Error = &authoring.Error{Code: "outcome_unknown", Message: "project action completed but its durable receipt could not be published", Details: map[string]any{"requestId": id, "jobStatusPath": filepath.Join(directory, "result.json"), "cause": writeErr.Error()}}
					receipt.Result = nil
				}
			}
		}
		s.jobMu.Lock()
		job.active = false
		if !pending {
			job.receipt = &receipt
		}
		s.jobMu.Unlock()
	}()
	return actionJobPending(action, id, directory, args), nil
}

func (receipt agentActionJobReceipt) value() (any, error) {
	if receipt.Error != nil {
		return nil, receipt.Error
	}
	if len(receipt.Result) == 0 {
		return nil, agentFailure("invalid_job_receipt", "completed project job has no result")
	}
	return json.RawMessage(receipt.Result), nil
}

func makeActionJobReceipt(value any, err error) (agentActionJobReceipt, bool) {
	receipt := agentActionJobReceipt{SchemaVersion: 1}
	if err != nil {
		var detail *authoring.Error
		if errors.As(err, &detail) {
			if detail.Code == "result_pending" {
				return receipt, true
			}
			receipt.Error = detail
		} else if errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded) {
			return receipt, true
		} else {
			receipt.Error = &authoring.Error{Code: "project_action_failed", Message: err.Error()}
		}
		return receipt, false
	}
	encoded, marshalErr := json.Marshal(value)
	if marshalErr != nil {
		receipt.Error = &authoring.Error{Code: "invalid_job_receipt", Message: marshalErr.Error()}
		return receipt, false
	}
	var result map[string]json.RawMessage
	if json.Unmarshal(encoded, &result) == nil {
		var status string
		json.Unmarshal(result["status"], &status)
		if status == "result_pending" {
			return receipt, true
		}
	}
	receipt.Result = encoded
	return receipt, false
}

func (s *agentProjectService) closeActionJobs() {
	s.jobMu.Lock()
	s.jobClosed = true
	if s.jobCancel != nil {
		s.jobCancel()
	}
	s.jobMu.Unlock()
	s.jobWait.Wait()
}

// A confirmed preparation may be read after the MCP process restarts. Restore
// its in-memory controller selection without rerunning Editor/SDK preparation.
func (s *agentProjectService) restorePreparedJobActivation(result any, raw json.RawMessage) (any, error) {
	if s.controller == nil {
		return result, nil
	}
	encoded, err := json.Marshal(result)
	if err != nil {
		return nil, err
	}
	var preparation agentPreparation
	if json.Unmarshal(encoded, &preparation) != nil || preparation.SchemaVersion != 1 || preparation.Baseline == "" {
		return result, nil
	}
	var args map[string]json.RawMessage
	if err := json.Unmarshal(raw, &args); err != nil {
		return nil, err
	}
	var activate = true
	if value := args["activate"]; value != nil {
		if err := json.Unmarshal(value, &activate); err != nil {
			return nil, err
		}
	}
	if !activate {
		return result, nil
	}
	s.stateMu.RLock()
	hasBaseline := s.baseline != ""
	s.stateMu.RUnlock()
	if hasBaseline {
		return result, nil
	}
	if !s.mu.TryLock() {
		return map[string]any{"success": false, "status": "result_pending", "requestId": preparation.RequestID, "backend": "project_action_job", "message": "preparation is confirmed; waiting to activate its controller selection"}, nil
	}
	defer s.mu.Unlock()
	if preparation.Project != s.project {
		return nil, agentFailure("invalid_job_receipt", "preparation receipt belongs to another project")
	}
	directory := filepath.Join(s.project, "Library/UnityMicroEditor/preparations", preparation.RequestID)
	if err := validateAgentCachedPreparation(s.project, s.editor, preparation.Scene, preparation.RequestID, directory, preparation); err != nil {
		return nil, err
	}
	if err := s.controller.ConfigurePrepared(preparation.Baseline, preparation.Scene); err != nil {
		return nil, err
	}
	s.stateMu.Lock()
	s.baseline, s.scene = preparation.Baseline, preparation.Scene
	s.stateMu.Unlock()
	return result, nil
}

func (s *agentProjectService) runtimeWithActionJobs(ctx context.Context, name string, raw json.RawMessage) (any, error) {
	if name != "session_control" {
		return s.controller.Handle(ctx, name, raw)
	}
	var input struct {
		Action    string         `json:"action"`
		RequestID string         `json:"requestId"`
		Arguments map[string]any `json:"arguments"`
	}
	if err := json.Unmarshal(raw, &input); err != nil {
		return nil, err
	}
	if input.Action == "stop" {
		return s.locked(ctx, func() (any, error) { return s.controller.Handle(ctx, name, raw) })
	}
	if input.Action != "start" && input.Action != "reset" {
		return s.controller.Handle(ctx, name, raw)
	}
	var scene string
	configured := false
	result, err := s.locked(ctx, func() (any, error) {
		s.stateMu.RLock()
		baseline := s.baseline
		scene = s.scene
		s.stateMu.RUnlock()
		if baseline == "" {
			return nil, nil
		}
		configured = true
		if selected := agentString(input.Arguments, "scene"); selected != "" && selected != scene {
			return nil, agentFailure("scene_requires_preparation", "prepare and activate the selected scene with project_prepare before starting or resetting this runtime")
		}
		return s.controller.Handle(ctx, name, raw)
	})
	if err != nil || configured {
		return result, err
	}
	if selected := agentString(input.Arguments, "scene"); selected != "" {
		scene = selected
	}
	id := input.RequestID
	if id == "" {
		s.jobMu.Lock()
		if s.autoPreparationID == "" {
			s.autoPreparationID = agentID()
		}
		id = s.autoPreparationID
		s.jobMu.Unlock()
	}
	args := map[string]any{"scene": scene, "requestId": id, "activate": true}
	encoded, _ := json.Marshal(args)
	result, err = s.Handle(ctx, "project_prepare", encoded)
	if err != nil {
		return result, err
	}
	var structured map[string]json.RawMessage
	if pending, ok := result.(map[string]any); ok {
		encoded, _ = json.Marshal(pending)
	} else {
		encoded, _ = json.Marshal(result)
	}
	if json.Unmarshal(encoded, &structured) == nil && bytes.Equal(structured["status"], []byte(`"result_pending"`)) {
		return result, nil
	}
	return s.locked(ctx, func() (any, error) {
		s.stateMu.RLock()
		activated, selectedScene := s.baseline != "", s.scene
		s.stateMu.RUnlock()
		if !activated {
			return nil, agentFailure("preparation_not_activated", "completed preparation did not activate a controller baseline")
		}
		if selected := agentString(input.Arguments, "scene"); selected != "" && selected != selectedScene {
			return nil, agentFailure("scene_requires_preparation", "prepare and activate the selected scene with project_prepare before starting or resetting this runtime")
		}
		return s.controller.Handle(ctx, name, raw)
	})
}
