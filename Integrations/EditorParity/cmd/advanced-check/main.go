// advanced-check verifies actual game callbacks and input in the AdvancedParity Player.
package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"image/png"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"
)

type runner struct {
	client, url, output, prefix, revision string
	pid, sequence                         int
	checks                                []map[string]any
}

func (r *runner) check(name string, passed bool, detail any) {
	r.checks = append(r.checks, map[string]any{"name": name, "passed": passed, "detail": detail})
	fmt.Printf("%s: %v\n", name, passed)
}

func (r *runner) call(name string, params map[string]any, label string) (map[string]any, error) {
	r.sequence++
	if params["requestId"] == nil {
		params["requestId"] = fmt.Sprintf("%s-%03d", r.prefix, r.sequence)
	}
	if r.pid != 0 {
		params["expectedPid"] = r.pid
		params["expectedRevision"] = r.revision
	}
	data, err := json.Marshal(map[string]any{"name": name, "arguments": params})
	if err != nil {
		return nil, err
	}
	path := filepath.Join(r.output, fmt.Sprintf("%03d-%s.json", r.sequence, label))
	deadline := time.Now().Add(90 * time.Second)
	for {
		ctx, cancel := context.WithTimeout(context.Background(), 50*time.Second)
		cmd := exec.CommandContext(ctx, r.client, "-url", r.url, "-method", "tools/call", "-params", string(data), "-output", path, "-timeout", "45s")
		output, runErr := cmd.CombinedOutput()
		cancel()
		body, readErr := os.ReadFile(path)
		if readErr != nil {
			return nil, fmt.Errorf("%s: %v: %s", label, runErr, output)
		}
		var envelope map[string]any
		if err := json.Unmarshal(body, &envelope); err != nil {
			return nil, err
		}
		result, _ := envelope["result"].(map[string]any)
		reply, _ := result["structuredContent"].(map[string]any)
		if reply == nil {
			return nil, fmt.Errorf("%s has no structured receipt: %s", label, body)
		}
		receipt, _ := reply["data"].(map[string]any)
		status, _ := receipt["status"].(string)
		if status != "result_pending" {
			return reply, nil
		}
		if time.Now().After(deadline) {
			return nil, fmt.Errorf("%s receipt remains pending", label)
		}
		// Only an explicit pending receipt permits polling the same immutable ID.
		time.Sleep(100 * time.Millisecond)
	}
}

func (r *runner) runtime(operation string, args map[string]any, label string, read bool) (map[string]any, error) {
	name := "runtime_command"
	if read {
		name = "runtime_query"
	}
	return r.call(name, map[string]any{"operation": operation, "arguments": args}, label)
}

func payload(reply map[string]any) map[string]any {
	data, _ := reply["data"].(map[string]any)
	result, _ := data["result"].(map[string]any)
	return result
}
func success(reply map[string]any) bool { return reply["success"] == true }
func count(value map[string]any, name string) int {
	number, _ := value[name].(float64)
	return int(number)
}
func equalCounts(a, b map[string]any, fields ...string) bool {
	for _, field := range fields {
		if count(a, field) != count(b, field) {
			return false
		}
	}
	return true
}
func (r *runner) invoke(name string, parameters map[string]any, target string, label string) (map[string]any, error) {
	args := map[string]any{"name": name, "parameters": parameters}
	if target != "" {
		args["target"] = map[string]any{"path": target}
	}
	return r.runtime("invoke", args, label, false)
}

func greenPixels(path string) (int, int, int, error) {
	file, err := os.Open(path)
	if err != nil {
		return 0, 0, 0, err
	}
	defer file.Close()
	img, err := png.Decode(file)
	if err != nil {
		return 0, 0, 0, err
	}
	bounds := img.Bounds()
	green := 0
	for y := bounds.Min.Y; y < bounds.Max.Y; y++ {
		for x := bounds.Min.X; x < bounds.Max.X; x++ {
			r, g, b, _ := img.At(x, y).RGBA()
			if g > r+30*257 && g > b+30*257 {
				green++
			}
		}
	}
	return green, bounds.Dx(), bounds.Dy(), nil
}

func (r *runner) run() error {
	status, err := r.runtime("status", map[string]any{}, "status", true)
	if err != nil {
		return err
	}
	if !success(status) {
		return errors.New("runtime status failed")
	}
	s := payload(status)
	r.pid = count(s, "pid")
	r.revision, _ = s["revision"].(string)
	if r.pid <= 0 || r.revision == "" {
		return errors.New("runtime identity is missing")
	}
	paused, err := r.runtime("simulation_pause", map[string]any{}, "pause", false)
	if err != nil {
		return err
	}
	r.check("pause accepted", success(paused), payload(paused))
	reset, err := r.invoke("advanced.reset", map[string]any{}, "", "reset")
	if err != nil {
		return err
	}
	r.check("reset checkpoint", success(reset) && count(payload(reset), "value") == 5, payload(reset))
	before, err := r.invoke("advanced.read", map[string]any{}, "", "before-wait")
	if err != nil {
		return err
	}
	wait, err := r.runtime("wait_frames", map[string]any{"frames": 60}, "wait-60", false)
	if err != nil {
		return err
	}
	after, err := r.invoke("advanced.read", map[string]any{}, "", "after-wait")
	if err != nil {
		return err
	}
	r.check("render frames advance while game callbacks freeze", success(wait) && count(payload(wait), "frame") > count(payload(paused), "frame") && equalCounts(payload(before), payload(after), "updates", "fixedUpdates", "lateUpdates"), map[string]any{"before": payload(before), "after": payload(after), "wait": payload(wait)})
	catalog, err := r.runtime("tools", map[string]any{}, "catalog", true)
	if err != nil {
		return err
	}
	c := payload(catalog)
	names := map[string]bool{}
	if list, ok := c["tools"].([]any); ok {
		for _, item := range list {
			if tool, ok := item.(map[string]any); ok {
				name, _ := tool["name"].(string)
				names[name] = true
			}
		}
	}
	valid := true
	for _, name := range []string{"advanced.read", "advanced.reset", "advanced.bump", "advanced.instance", "advanced.persist", "advanced.throw", "advanced.invalid_output"} {
		valid = valid && names[name]
	}
	r.check("attributed catalog excludes invalid/private names", success(catalog) && valid && !names["advanced.collision"] && !names["advanced.unsupported"] && !names["advanced.private_hidden"], c)
	invalid := []map[string]any{{"amount": 2}, {"amount": 0, "message": "bad"}, {"amount": 11, "message": "bad"}, {"amount": 2, "message": nil}, {"amount": 2, "message": "ok", "extra": true}}
	for index, args := range invalid {
		reply, err := r.invoke("advanced.bump", args, "", fmt.Sprintf("invalid-%d", index))
		if err != nil {
			return err
		}
		r.check(fmt.Sprintf("invalid DTO %d rejected", index), !success(reply), reply)
	}
	read, err := r.invoke("advanced.read", map[string]any{}, "", "after-invalid")
	if err != nil {
		return err
	}
	r.check("validation precedes DTO construction and method", count(payload(read), "dtoConstructors") == 0 && count(payload(read), "toolCalls") == 0 && count(payload(read), "value") == 5, payload(read))
	first, err := r.call("project.advanced.bump", map[string]any{"parameters": map[string]any{"amount": 2, "message": "static"}}, "dynamic-static")
	if err != nil {
		return err
	}
	second, err := r.call("project.advanced.instance", map[string]any{"parameters": map[string]any{"amount": 3, "message": "instance"}, "target": map[string]any{"path": "AdvancedCanvas/AdvancedProbe"}}, "dynamic-instance")
	if err != nil {
		return err
	}
	r.check("dynamic MCP static and instance execute on main thread", success(first) && success(second) && count(payload(second), "value") == 10 && count(payload(second), "dtoConstructors") == 2 && count(payload(second), "toolCalls") == 2 && payload(second)["sameMainThread"] == true, payload(second))
	for _, name := range []string{"advanced.throw", "advanced.invalid_output"} {
		reply, err := r.invoke(name, map[string]any{}, "", strings.ReplaceAll(name, ".", "-"))
		if err != nil {
			return err
		}
		r.check(name+" execution failure", !success(reply), reply)
	}
	read, err = r.invoke("advanced.read", map[string]any{}, "", "after-execution-errors")
	if err != nil {
		return err
	}
	r.check("execution failure preserves observed effects", count(payload(read), "toolCalls") == 4 && count(payload(read), "dtoConstructors") == 2, payload(read))
	_, err = r.invoke("advanced.reset", map[string]any{}, "", "reset-input")
	if err != nil {
		return err
	}
	for index, args := range []map[string]any{{"action": "key", "key": "A", "pressed": true}, {"action": "mouse", "button": "Left", "pressed": true, "position": map[string]any{"x": 0, "y": 0}}, {"action": "ui_click", "target": map[string]any{"path": "AdvancedCanvas/AdvancedProbe"}}} {
		reply, err := r.runtime("input", args, fmt.Sprintf("input-%d", index), false)
		if err != nil {
			return err
		}
		r.check(fmt.Sprintf("input %d accepted", index), success(reply), reply)
	}
	step, err := r.runtime("simulation_step", map[string]any{}, "single-step", false)
	if err != nil {
		return err
	}
	read, err = r.invoke("advanced.read", map[string]any{}, "", "after-step")
	if err != nil {
		return err
	}
	p := payload(read)
	r.check("one actual Update FixedUpdate LateUpdate", success(step) && count(p, "updates") == 1 && count(p, "fixedUpdates") == 1 && count(p, "lateUpdates") == 1, map[string]any{"step": step, "counters": p})
	r.check("actual game sees key mouse and UI handlers", count(p, "keyPresses") == 1 && count(p, "mousePresses") == 1 && count(p, "pointerDowns") == 1 && count(p, "pointerUps") == 1 && count(p, "clicks") == 1, p)
	_, err = r.runtime("wait_frames", map[string]any{"frames": 60}, "wait-after-step", false)
	if err != nil {
		return err
	}
	read, err = r.invoke("advanced.read", map[string]any{}, "", "after-step-wait")
	if err != nil {
		return err
	}
	r.check("step returns to a paused game", equalCounts(p, payload(read), "updates", "fixedUpdates", "lateUpdates", "keyPresses", "mousePresses"), payload(read))
	for index, args := range []map[string]any{{"action": "key", "key": "A", "pressed": false}, {"action": "mouse", "button": "Left", "pressed": false}} {
		reply, err := r.runtime("input", args, fmt.Sprintf("input-release-%d", index), false)
		if err != nil {
			return err
		}
		r.check(fmt.Sprintf("input release %d queued", index), success(reply), reply)
	}
	screen, err := r.runtime("screenshot", map[string]any{}, "screen-paused", false)
	if err != nil {
		return err
	}
	if success(screen) {
		image := payload(screen)
		path, _ := image["path"].(string)
		green, w, h, err := greenPixels(path)
		r.check("paused end-of-frame screen includes overlay", err == nil && green > 1000 && w == count(image, "width") && h == count(image, "height") && image["includesOverlayUi"] == true, map[string]any{"receipt": image, "greenPixels": green, "error": fmt.Sprint(err)})
	} else {
		r.check("paused end-of-frame screen includes overlay", false, screen)
	}
	camera, err := r.runtime("screenshot", map[string]any{"mode": "camera", "width": 960, "height": 540, "target": map[string]any{"path": "AdvancedCamera"}}, "camera-paused", false)
	if err != nil {
		return err
	}
	if success(camera) {
		image := payload(camera)
		path, _ := image["path"].(string)
		green, w, h, err := greenPixels(path)
		r.check("camera mode excludes overlay", err == nil && green < 100 && w == 960 && h == 540 && image["includesOverlayUi"] == false, map[string]any{"receipt": image, "greenPixels": green, "error": fmt.Sprint(err)})
	} else {
		r.check("camera mode excludes overlay", false, camera)
	}
	viewport, _ := s["viewport"].(map[string]any)
	wrong, err := r.runtime("screenshot", map[string]any{"width": count(viewport, "width") + 1}, "screen-size-rejected", false)
	if err != nil {
		return err
	}
	r.check("screen cannot silently resize viewport", !success(wrong), wrong)
	persisted, err := r.call("project.advanced.persist", map[string]any{"parameters": map[string]any{}, "target": map[string]any{"path": "AdvancedCanvas/AdvancedProbe"}}, "persistent-root")
	if err != nil {
		return err
	}
	catalog, err = r.runtime("tools", map[string]any{}, "persistent-catalog", true)
	if err != nil {
		return err
	}
	read, err = r.call("project.advanced.instance", map[string]any{"parameters": map[string]any{"amount": 1, "message": "persistent"}, "target": map[string]any{"path": "AdvancedCanvas/AdvancedProbe"}}, "persistent-instance")
	if err != nil {
		return err
	}
	inspected, err := r.runtime("inspect", map[string]any{"path": "AdvancedCanvas/AdvancedProbe"}, "persistent-inspect", true)
	if err != nil {
		return err
	}
	hierarchy, err := r.runtime("hierarchy", map[string]any{}, "persistent-hierarchy", true)
	if err != nil {
		return err
	}
	r.check("persistent receiver remains in catalog and hierarchy", success(persisted) && payload(persisted)["persistent"] == true && success(catalog) && success(read) && payload(read)["persistent"] == true && count(payload(read), "value") == 6 && success(inspected) && success(hierarchy) && count(payload(hierarchy), "total") == 4, map[string]any{"persist": persisted, "instance": read, "inspect": inspected, "hierarchy": hierarchy})
	return nil
}

func main() {
	client := flag.String("client", "", "compiled mcp-call executable")
	url := flag.String("url", "http://127.0.0.1:8093/mcp", "owned advanced MCP endpoint")
	output := flag.String("output", "", "new evidence directory")
	flag.Parse()
	if *client == "" || *output == "" {
		fmt.Fprintln(os.Stderr, "-client and -output are required")
		os.Exit(2)
	}
	if err := os.MkdirAll(*output, 0700); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	r := runner{client: *client, url: *url, output: *output, prefix: fmt.Sprintf("advanced-%d", time.Now().UnixNano())}
	err := r.run()
	passed := err == nil
	for _, check := range r.checks {
		passed = passed && check["passed"] == true
	}
	report := map[string]any{"schemaVersion": 1, "passed": passed, "pid": r.pid, "revision": r.revision, "checks": r.checks}
	if err != nil {
		report["error"] = err.Error()
	}
	body, _ := json.MarshalIndent(report, "", "  ")
	os.WriteFile(filepath.Join(*output, "assertions.json"), append(body, '\n'), 0600)
	if !passed {
		fmt.Fprintln(os.Stderr, "advanced assertions failed; inspect evidence directory")
		os.Exit(1)
	}
}
