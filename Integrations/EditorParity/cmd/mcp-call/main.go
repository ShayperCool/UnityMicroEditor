// mcp-call runs one MCP request against a loopback Streamable HTTP server.
package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"strings"
	"time"
)

type client struct {
	endpoint string
	session  string
	http     *http.Client
}

func decodeResponse(body []byte, contentType string) (json.RawMessage, error) {
	if strings.Contains(contentType, "text/event-stream") {
		scanner := bufio.NewScanner(bytes.NewReader(body))
		scanner.Buffer(make([]byte, 4096), 16*1024*1024)
		for scanner.Scan() {
			line := strings.TrimSpace(scanner.Text())
			if strings.HasPrefix(line, "data:") {
				data := []byte(strings.TrimSpace(strings.TrimPrefix(line, "data:")))
				var envelope struct {
					ID json.RawMessage `json:"id"`
				}
				if json.Unmarshal(data, &envelope) == nil && len(envelope.ID) != 0 {
					return data, nil
				}
			}
		}
		if err := scanner.Err(); err != nil {
			return nil, err
		}
		return nil, errors.New("SSE response has no JSON-RPC reply")
	}
	if !json.Valid(body) {
		return nil, errors.New("server returned invalid JSON")
	}
	return json.RawMessage(body), nil
}

func (c *client) request(ctx context.Context, method string, id int, params any) (json.RawMessage, error) {
	payload := map[string]any{"jsonrpc": "2.0", "method": method, "params": params}
	if id != 0 {
		payload["id"] = id
	}
	data, err := json.Marshal(payload)
	if err != nil {
		return nil, err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, c.endpoint, bytes.NewReader(data))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Accept", "application/json, text/event-stream")
	if c.session != "" {
		req.Header.Set("Mcp-Session-Id", c.session)
		req.Header.Set("MCP-Protocol-Version", "2025-03-26")
	}
	resp, err := c.http.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if session := resp.Header.Get("Mcp-Session-Id"); session != "" {
		c.session = session
	}
	body, err := io.ReadAll(io.LimitReader(resp.Body, 16*1024*1024+1))
	if err != nil {
		return nil, err
	}
	if len(body) > 16*1024*1024 {
		return nil, errors.New("MCP response exceeds 16 MiB")
	}
	if resp.StatusCode >= 400 {
		return nil, fmt.Errorf("HTTP %s: %s", resp.Status, body)
	}
	if id == 0 {
		return nil, nil
	}
	return decodeResponse(body, resp.Header.Get("Content-Type"))
}

func operationError(data json.RawMessage) error {
	var envelope map[string]json.RawMessage
	if err := json.Unmarshal(data, &envelope); err != nil {
		return err
	}
	if errorValue := envelope["error"]; len(errorValue) != 0 && string(errorValue) != "null" {
		return fmt.Errorf("operation error: %s", errorValue)
	}
	var isError bool
	json.Unmarshal(envelope["isError"], &isError)
	if isError {
		return errors.New("MCP tool returned isError=true; see the saved reply")
	}
	var success bool
	if value := envelope["success"]; len(value) != 0 && json.Unmarshal(value, &success) == nil && !success {
		return errors.New("operation returned success=false; see the saved reply")
	}
	for _, key := range []string{"result", "structuredContent"} {
		if nested := envelope[key]; len(nested) != 0 && string(nested) != "null" {
			if err := operationError(nested); err != nil {
				return err
			}
		}
	}
	return nil
}

func (c *client) close() {
	if c.session == "" {
		return
	}
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	req, err := http.NewRequestWithContext(ctx, http.MethodDelete, c.endpoint, nil)
	if err != nil {
		return
	}
	req.Header.Set("Mcp-Session-Id", c.session)
	req.Header.Set("MCP-Protocol-Version", "2025-03-26")
	resp, err := c.http.Do(req)
	if err == nil {
		resp.Body.Close()
	}
}

func run() error {
	endpoint := flag.String("url", "http://127.0.0.1:8087/mcp", "loopback MCP endpoint")
	method := flag.String("method", "tools/list", "JSON-RPC method")
	params := flag.String("params", "{}", "JSON-RPC params JSON")
	paramsFile := flag.String("params-file", "", "read JSON-RPC params from this file")
	output := flag.String("output", "", "optional result file")
	deadline := flag.Duration("timeout", 45*time.Second, "total request timeout")
	flag.Parse()
	if *paramsFile != "" {
		data, err := os.ReadFile(*paramsFile)
		if err != nil {
			return err
		}
		*params = string(data)
	}
	u, err := url.Parse(*endpoint)
	if err != nil || u.Scheme != "http" || (u.Hostname() != "127.0.0.1" && u.Hostname() != "localhost" && u.Hostname() != "::1") {
		return errors.New("only loopback HTTP MCP URLs are supported")
	}
	if !json.Valid([]byte(*params)) {
		return errors.New("params must be valid JSON")
	}
	ctx, cancel := context.WithTimeout(context.Background(), *deadline)
	defer cancel()
	c := &client{endpoint: *endpoint, http: &http.Client{}}
	defer c.close()
	initialized, err := c.request(ctx, "initialize", 1, map[string]any{"protocolVersion": "2025-03-26", "capabilities": map[string]any{}, "clientInfo": map[string]string{"name": "UnityMicroEditorParity", "version": "0.1.0"}})
	if err != nil {
		return err
	}
	if err := operationError(initialized); err != nil {
		return err
	}
	if _, err := c.request(ctx, "notifications/initialized", 0, map[string]any{}); err != nil {
		return err
	}
	result, err := c.request(ctx, *method, 2, json.RawMessage(*params))
	if err != nil {
		return err
	}
	var pretty bytes.Buffer
	if err := json.Indent(&pretty, result, "", "  "); err != nil {
		return err
	}
	pretty.WriteByte('\n')
	if *output != "" {
		if err := os.WriteFile(*output, pretty.Bytes(), 0600); err != nil {
			return err
		}
	}
	_, err = os.Stdout.Write(pretty.Bytes())
	if err != nil {
		return err
	}
	return operationError(result)
}

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
