package agentcontrol

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
	"time"
)

func TestControllerPersistentReceiptSurvivesServerAndControlRemoval(t *testing.T) {
	c := controllerFixture(t, "")
	state := startReady(t, c)
	args := requestArgs(map[string]any{"requestId": "durable-once", "arguments": map[string]any{"value": 23}})
	first, err := c.Request(context.Background(), "custom", args)
	if err != nil {
		t.Fatal(err)
	}
	if err := c.Close(); err != nil {
		t.Fatal(err)
	}
	if err := os.RemoveAll(state.ControlDirectory); err != nil {
		t.Fatal(err)
	}
	restarted, err := New(context.Background(), c.options)
	if err != nil {
		t.Fatal(err)
	}
	defer restarted.Close()
	replay, err := restarted.Request(context.Background(), "custom", args)
	if err != nil {
		t.Fatal(err)
	}
	a, _ := json.Marshal(first)
	b, _ := json.Marshal(replay)
	if string(a) != string(b) {
		t.Fatalf("persistent receipt changed: %s != %s", a, b)
	}
	args["arguments"] = json.RawMessage(`{"value":24}`)
	if _, err := restarted.Request(context.Background(), "custom", args); err == nil {
		t.Fatal("restart accepted conflicting request ID")
	}
	data, _ := os.ReadFile(filepath.Join(c.options.Output, "executions.json"))
	if string(data) != "1" {
		t.Fatalf("replay executed command again: %s", data)
	}
}

func TestControllerPersistentPendingCannotExecuteOnReplacementPlayer(t *testing.T) {
	c := controllerFixture(t, "")
	startReady(t, c)
	args := requestArgs(map[string]any{"requestId": "durable-pending", "arguments": map[string]any{"dropResponse": true}})
	ctx, cancel := context.WithTimeout(context.Background(), 25*time.Millisecond)
	if _, err := c.Request(ctx, "custom", args); err != nil {
		t.Fatal(err)
	}
	cancel()
	if err := c.Close(); err != nil {
		t.Fatal(err)
	}
	restarted, err := New(context.Background(), c.options)
	if err != nil {
		t.Fatal(err)
	}
	restarted.stopGrace = 40 * time.Millisecond
	restarted.terminateGrace = 40 * time.Millisecond
	defer restarted.Close()
	startReady(t, restarted)
	result, err := restarted.Request(context.Background(), "custom", args)
	if err != nil || result.(map[string]any)["status"] != "result_unknown" {
		t.Fatalf("pending command replayed: %v %v", result, err)
	}
	data, _ := os.ReadFile(filepath.Join(c.options.Output, "executions.json"))
	if string(data) != "1" {
		t.Fatalf("restarted Player accepted old command: %s", data)
	}
}

func TestControllerRejectsCorruptPersistentRequest(t *testing.T) {
	c := controllerFixture(t, "")
	startReady(t, c)
	if _, err := c.Request(context.Background(), "status", requestArgs(map[string]any{"requestId": "corrupt"})); err != nil {
		t.Fatal(err)
	}
	if err := c.Close(); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(c.options.Output, ".agent-requests", "corrupt.request-record.json")
	data, _ := os.ReadFile(path)
	var record requestRecord
	if err := json.Unmarshal(data, &record); err != nil {
		t.Fatal(err)
	}
	record.Directory = filepath.Join(c.options.Project, "Assets")
	data, _ = json.Marshal(record)
	if err := os.WriteFile(path, data, 0600); err != nil {
		t.Fatal(err)
	}
	if _, err := New(context.Background(), c.options); err == nil {
		t.Fatal("foreign control path accepted from persistent journal")
	}
}
