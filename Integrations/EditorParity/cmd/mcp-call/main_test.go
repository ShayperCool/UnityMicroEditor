package main

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func TestDecodeSSESkipsNotification(t *testing.T) {
	input := "event: message\r\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\r\n\r\nevent: message\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"ok\":true}}\r\n\r\n"
	result, err := decodeResponse([]byte(input), "text/event-stream; charset=utf-8")
	if err != nil || !strings.Contains(string(result), `"ok":true`) {
		t.Fatalf("result=%s err=%v", result, err)
	}
}

func TestSessionHeadersAndNotificationIdentity(t *testing.T) {
	requests := 0
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		requests++
		if requests > 1 && (r.Header.Get("Mcp-Session-Id") != "reference-session" || r.Header.Get("MCP-Protocol-Version") != "2025-03-26") {
			t.Error("session/protocol headers missing")
		}
		if r.Method == http.MethodDelete {
			w.WriteHeader(http.StatusNoContent)
			return
		}
		var payload map[string]any
		if err := json.NewDecoder(r.Body).Decode(&payload); err != nil {
			t.Error(err)
		}
		if payload["method"] == "notifications/initialized" {
			if payload["id"] != nil {
				t.Error("notification has request id")
			}
			w.WriteHeader(http.StatusAccepted)
			return
		}
		w.Header().Set("Mcp-Session-Id", "reference-session")
		w.Header().Set("Content-Type", "text/event-stream")
		fmt.Fprintf(w, "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":%v,\"result\":{}}\n\n", payload["id"])
	}))
	defer server.Close()
	c := &client{endpoint: server.URL, http: server.Client()}
	if _, err := c.request(context.Background(), "initialize", 1, map[string]any{}); err != nil {
		t.Fatal(err)
	}
	if _, err := c.request(context.Background(), "notifications/initialized", 0, map[string]any{}); err != nil {
		t.Fatal(err)
	}
	if _, err := c.request(context.Background(), "tools/list", 2, map[string]any{}); err != nil {
		t.Fatal(err)
	}
	c.close()
	if requests != 4 {
		t.Fatalf("requests=%d", requests)
	}
}

func TestOperationErrorsAreNotReportedAsSuccess(t *testing.T) {
	for _, input := range []string{`{"error":{"code":-32602}}`, `{"result":{"isError":true}}`, `{"result":{"structuredContent":{"success":false}}}`} {
		if err := operationError(json.RawMessage(input)); err == nil {
			t.Fatalf("accepted failure %s", input)
		}
	}
	if err := operationError(json.RawMessage(`{"result":{"structuredContent":{"success":true,"data":{"error":null}}}}`)); err != nil {
		t.Fatal(err)
	}
}

func TestDecodeRejectsIncompleteResponse(t *testing.T) {
	for _, input := range []struct{ body, kind string }{{`{"invalid"`, "application/json"}, {"event: message\ndata: {\"method\":\"notification\"}\n", "text/event-stream"}} {
		if _, err := decodeResponse([]byte(input.body), input.kind); err == nil {
			t.Fatal("invalid or incomplete response accepted")
		}
	}
}
