// Package agentcontrol owns a runner process and validates immutable Player commands.
package agentcontrol

import (
	"bufio"
	"bytes"
	"context"
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"

	goproject "unitymicroeditor/project"
)

type Options struct {
	Project, Baseline, Output, Editor, Runner, Scene string
	FPS                                              int
}

type State struct {
	Phase                      string         `json:"phase"`
	SessionID                  string         `json:"sessionId,omitempty"`
	PID                        int            `json:"pid,omitempty"`
	Revision                   string         `json:"revision,omitempty"`
	ControlDirectory           string         `json:"controlDirectory,omitempty"`
	LastSequence               uint64         `json:"lastSequence"`
	LastReadySequence          uint64         `json:"lastReadySequence"`
	LastUpdateSequence         uint64         `json:"lastUpdateSequence"`
	LastUpdateRejectedSequence uint64         `json:"lastUpdateRejectedSequence"`
	UpdateError                string         `json:"updateError,omitempty"`
	LastError                  string         `json:"lastError,omitempty"`
	RunnerStderrPath           string         `json:"runnerStderrPath,omitempty"`
	RunnerStderrTail           string         `json:"runnerStderrTail,omitempty"`
	Events                     []Event        `json:"events,omitempty"`
	Ready                      map[string]any `json:"ready,omitempty"`
}

type Event struct {
	Sequence uint64         `json:"sequence"`
	Data     map[string]any `json:"data"`
}

type Controller struct {
	options                           Options
	ctx                               context.Context
	cancel                            context.CancelFunc
	mu                                sync.Mutex
	state                             State
	command                           *exec.Cmd
	done                              chan struct{}
	changed                           chan struct{}
	requests                          map[string]*requestRecord
	stopRequested                     bool
	player                            processIdentity
	eventSizes                        []int
	eventBytes                        int
	stopGrace, terminateGrace         time.Duration
	lastReadyBaseline, lastReadyScene string
	preparationUpdatePending          bool
}

type requestRecord struct {
	ID, Operation, SessionID, Directory, Revision, Hash string
	PID                                                 int
	Arguments, Envelope                                 []byte
}

type processIdentity struct {
	PID     int
	Started string
}

func New(ctx context.Context, options Options) (*Controller, error) {
	if !filepath.IsAbs(options.Project) || !filepath.IsAbs(options.Runner) {
		return nil, errors.New("runtime controller requires explicit absolute project and runner paths")
	}
	project, err := filepath.EvalSymlinks(options.Project)
	if err != nil {
		return nil, err
	}
	options.Project = project
	if info, err := os.Stat(project); err != nil || !info.IsDir() {
		return nil, errors.New("runtime project must be an existing directory")
	}
	runner, err := filepath.EvalSymlinks(options.Runner)
	if err != nil {
		return nil, err
	}
	if info, err := os.Stat(runner); err != nil || !info.Mode().IsRegular() || info.Mode().Perm()&0111 == 0 {
		return nil, errors.New("runtime runner must be an executable regular file")
	}
	options.Runner = runner
	if options.Output == "" {
		options.Output = filepath.Join(project, "Library", "UnityMicroEditor", "agent")
	}
	if !goproject.IsOwnedCachePath(project, options.Output) {
		return nil, errors.New("runtime output must stay inside the explicit project's Temp or Library/UnityMicroEditor cache")
	}
	if err := canonicalDestination(options.Output); err != nil {
		return nil, err
	}
	if options.FPS == 0 {
		options.FPS = 30
	}
	if options.FPS < 1 || options.FPS > 240 {
		return nil, errors.New("runtime FPS must be between 1 and 240")
	}
	lifetime, cancel := context.WithCancel(ctx)
	c := &Controller{options: options, ctx: lifetime, cancel: cancel, state: State{Phase: "stopped"}, changed: make(chan struct{}), requests: map[string]*requestRecord{}, stopGrace: 12 * time.Second, terminateGrace: 5 * time.Second}
	if err := c.loadRequestJournal(); err != nil {
		cancel()
		return nil, err
	}
	go func() {
		<-lifetime.Done()
		stopCtx, stopCancel := context.WithTimeout(context.Background(), 20*time.Second)
		defer stopCancel()
		_, _ = c.Stop(stopCtx)
	}()
	return c, nil
}

func (c *Controller) Status() State {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.snapshotLocked()
}

// ConfigurePrepared changes the next launch only while no runner is active.
// A preparation failure leaves the current running Player and baseline intact.
func (c *Controller) ConfigurePrepared(baseline, scene string) error {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.command != nil {
		return errors.New("stop the current runtime before activating a new preparation")
	}
	if !filepath.IsAbs(baseline) || !goproject.IsOwnedCachePath(c.options.Project, baseline) {
		return errors.New("prepared baseline must be in the explicit project cache")
	}
	if err := canonicalDestination(baseline); err != nil {
		return err
	}
	if info, err := os.Stat(baseline); err != nil || !info.Mode().IsRegular() {
		return errors.New("prepared baseline does not exist")
	}
	if !strings.HasPrefix(scene, "Assets/") || filepath.ToSlash(filepath.Clean(scene)) != scene || filepath.Ext(scene) != ".unity" {
		return errors.New("prepared scene must be an authored Assets/...unity path")
	}
	c.preparationUpdatePending = c.lastReadyBaseline != "" && (baseline != c.lastReadyBaseline || scene != c.lastReadyScene)
	c.options.Baseline, c.options.Scene = baseline, scene
	return nil
}

func cloneMap(value map[string]any) map[string]any {
	if value == nil {
		return nil
	}
	result := make(map[string]any, len(value))
	for key, item := range value {
		result[key] = cloneValue(item)
	}
	return result
}
func cloneValue(value any) any {
	switch value := value.(type) {
	case map[string]any:
		return cloneMap(value)
	case []any:
		result := make([]any, len(value))
		for i, item := range value {
			result[i] = cloneValue(item)
		}
		return result
	default:
		return value
	}
}
func (c *Controller) snapshotLocked() State {
	result := c.state
	result.Events = make([]Event, len(c.state.Events))
	for i, event := range c.state.Events {
		result.Events[i] = Event{Sequence: event.Sequence, Data: cloneMap(event.Data)}
	}
	result.Ready = cloneMap(c.state.Ready)
	return result
}

func (c *Controller) notifyLocked() { close(c.changed); c.changed = make(chan struct{}) }

func (c *Controller) Start() (State, error) {
	c.mu.Lock()
	if c.command != nil {
		c.mu.Unlock()
		return c.Status(), nil
	}
	if err := c.ctx.Err(); err != nil {
		c.mu.Unlock()
		return State{}, err
	}
	if err := canonicalDestination(c.options.Output); err != nil {
		c.mu.Unlock()
		return State{}, err
	}
	if err := os.MkdirAll(c.options.Output, 0700); err != nil {
		c.mu.Unlock()
		return State{}, err
	}
	arguments := []string{"run", "--project", c.options.Project, "--output", c.options.Output, "--watch", "--control", "--fps", fmt.Sprint(c.options.FPS)}
	for _, pair := range [][2]string{{"--baseline", c.options.Baseline}, {"--editor", c.options.Editor}, {"--scene", c.options.Scene}} {
		if pair[1] != "" {
			arguments = append(arguments, pair[0], pair[1])
		}
	}
	command := exec.Command(c.options.Runner, arguments...)
	command.Dir = c.options.Project
	command.SysProcAttr = &syscall.SysProcAttr{Setpgid: true, Pdeathsig: syscall.SIGTERM}
	stdout, writer, err := os.Pipe()
	if err != nil {
		c.mu.Unlock()
		return State{}, err
	}
	command.Stdout = writer
	outputRoot, err := os.OpenRoot(c.options.Output)
	if err != nil {
		stdout.Close()
		writer.Close()
		c.mu.Unlock()
		return State{}, err
	}
	stderr, err := outputRoot.OpenFile("agent-runner-stderr.log", os.O_WRONLY|os.O_CREATE|os.O_TRUNC|syscall.O_NOFOLLOW, 0600)
	outputRoot.Close()
	if err != nil {
		stdout.Close()
		writer.Close()
		c.mu.Unlock()
		return State{}, err
	}
	command.Stderr = stderr
	if err := command.Start(); err != nil {
		stdout.Close()
		writer.Close()
		stderr.Close()
		c.state.Phase, c.state.LastError = "failed", err.Error()
		c.notifyLocked()
		c.mu.Unlock()
		return State{}, err
	}
	writer.Close()
	c.command, c.done = command, make(chan struct{})
	c.stopRequested = false
	c.player = processIdentity{}
	c.eventSizes = nil
	c.eventBytes = 0
	c.state = State{Phase: "starting", SessionID: newID(), LastSequence: c.state.LastSequence,
		RunnerStderrPath: filepath.Join(c.options.Output, "agent-runner-stderr.log")}
	c.notifyLocked()
	done := c.done
	c.mu.Unlock()
	go c.observe(command, stdout, stderr, done)
	return c.Status(), nil
}

func (c *Controller) observe(command *exec.Cmd, stdout *os.File, stderr *os.File, done chan struct{}) {
	readDone := make(chan error, 1)
	go func() {
		scanner := bufio.NewScanner(stdout)
		scanner.Buffer(make([]byte, 4096), 4*1024*1024)
		var readError error
		for scanner.Scan() {
			if len(bytes.TrimSpace(scanner.Bytes())) == 0 {
				continue
			}
			var event map[string]any
			decoder := json.NewDecoder(bytes.NewReader(scanner.Bytes()))
			decoder.UseNumber()
			if err := decoder.Decode(&event); err != nil || event == nil {
				readError = errors.New("runner stdout contained malformed JSON event")
				break
			}
			var extra any
			if decoder.Decode(&extra) != io.EOF {
				readError = errors.New("runner stdout event contains trailing data")
				break
			}
			c.mu.Lock()
			if c.command != command {
				c.mu.Unlock()
				break
			}
			readError = c.acceptEventLocked(command, event, len(scanner.Bytes()))
			c.notifyLocked()
			c.mu.Unlock()
			if readError != nil {
				break
			}
		}
		if readError == nil {
			readError = scanner.Err()
		}
		if readError != nil && !errors.Is(readError, os.ErrClosed) {
			c.mu.Lock()
			c.state.Phase, c.state.LastError = "failed", readError.Error()
			grace := c.terminateGrace
			c.notifyLocked()
			c.mu.Unlock()
			_ = syscall.Kill(-command.Process.Pid, syscall.SIGTERM)
			go func() {
				timer := time.NewTimer(grace)
				defer timer.Stop()
				select {
				case <-done:
				case <-timer.C:
					_ = syscall.Kill(-command.Process.Pid, syscall.SIGKILL)
				}
			}()
		}
		readDone <- readError
	}()
	// A separate pipe keeps Wait from discarding buffered events. Waiting for
	// process exit independently also handles descendants that retain stdout.
	exitError := command.Wait()
	var readError error
	select {
	case readError = <-readDone:
	case <-time.After(250 * time.Millisecond):
		stdout.Close()
		readError = <-readDone
		if errors.Is(readError, os.ErrClosed) {
			readError = nil
		}
	}
	stdout.Close()
	c.mu.Lock()
	failed := c.command == command && (readError != nil || exitError != nil && !c.stopRequested || !c.stopRequested && c.state.Phase == "starting")
	output := c.options.Output
	c.mu.Unlock()
	var stderrTail string
	if failed {
		// Read while the original writer descriptor still proves this log's
		// identity. Filesystem work stays outside the controller mutex.
		stderrTail, _ = readRunnerStderrTail(output, stderr)
	}
	stderr.Close()
	c.mu.Lock()
	if c.command == command {
		signalOwnedPlayer(c.player, syscall.SIGKILL)
		c.command = nil
		if readError != nil || exitError != nil && !c.stopRequested || !c.stopRequested && c.state.Phase == "starting" {
			c.state.Phase = "failed"
			if readError != nil {
				c.state.LastError = readError.Error()
			} else if exitError != nil {
				c.state.LastError = exitError.Error()
			} else {
				c.state.LastError = "runner exited before confirmed Player readiness"
			}
			c.state.RunnerStderrPath = filepath.Join(output, "agent-runner-stderr.log")
			c.state.RunnerStderrTail = stderrTail
			c.state.LastError = runnerFailureMessage(c.state.LastError, stderrTail)
		} else {
			c.state.Phase = "stopped"
		}
		c.state.PID, c.state.ControlDirectory = 0, ""
		c.notifyLocked()
	}
	close(done)
	c.mu.Unlock()
}

const runnerStderrMaximum = 8 * 1024

func readRunnerStderrTail(output string, writer *os.File) (string, error) {
	if err := canonicalDestination(output); err != nil {
		return "", err
	}
	expected, err := writer.Stat()
	if err != nil || !expected.Mode().IsRegular() {
		return "", errors.New("runner stderr writer is not a regular file")
	}
	root, err := os.OpenRoot(output)
	if err != nil {
		return "", err
	}
	defer root.Close()
	file, err := root.OpenFile("agent-runner-stderr.log", os.O_RDONLY|syscall.O_NOFOLLOW|syscall.O_NONBLOCK, 0)
	if err != nil {
		return "", err
	}
	defer file.Close()
	before, err := file.Stat()
	if err != nil || !before.Mode().IsRegular() || !os.SameFile(expected, before) {
		return "", errors.New("runner stderr log identity differs from its writer")
	}
	start := int64(0)
	if before.Size() > runnerStderrMaximum {
		start = before.Size() - runnerStderrMaximum
	}
	buffer := make([]byte, int(before.Size()-start))
	n, err := file.ReadAt(buffer, start)
	if err != nil && !errors.Is(err, io.EOF) {
		return "", err
	}
	after, statErr := file.Stat()
	current, pathErr := root.Lstat("agent-runner-stderr.log")
	if statErr != nil || pathErr != nil || !current.Mode().IsRegular() || !os.SameFile(expected, after) || !os.SameFile(expected, current) || n != len(buffer) || before.Size() != after.Size() || before.Size() != current.Size() || !before.ModTime().Equal(after.ModTime()) || !before.ModTime().Equal(current.ModTime()) {
		return "", errors.New("runner stderr log changed while reading diagnostics")
	}
	if start != 0 {
		// Avoid displaying a fragment of an oversized first line when subsequent
		// complete lines include the actual failure reason.
		if newline := bytes.IndexByte(buffer, '\n'); newline >= 0 && newline+1 < len(buffer) {
			buffer = buffer[newline+1:]
		}
	}
	buffer = bytes.ReplaceAll(buffer, []byte{0}, nil)
	buffer = bytes.ToValidUTF8(buffer, []byte("?"))
	return strings.TrimSpace(string(buffer)), nil
}

func runnerFailureMessage(reason, tail string) string {
	if tail == "" {
		return reason
	}
	prefix := reason + "\nrunner stderr:\n"
	if len(prefix) > runnerStderrMaximum {
		prefix = prefix[:runnerStderrMaximum]
	}
	available := runnerStderrMaximum - len(prefix)
	if len(tail) > available {
		tail = string(bytes.ToValidUTF8([]byte(tail[len(tail)-available:]), []byte("?")))
	}
	return prefix + tail
}

func (c *Controller) acceptEventLocked(command *exec.Cmd, event map[string]any, size int) error {
	name, _ := event["event"].(string)
	if name == "" || len(name) > 128 {
		return errors.New("runner stdout has no bounded event name")
	}
	c.state.LastSequence++
	sequence := c.state.LastSequence
	c.state.Events = append(c.state.Events, Event{Sequence: sequence, Data: event})
	c.eventSizes = append(c.eventSizes, size)
	c.eventBytes += size
	for len(c.state.Events) > 64 || c.eventBytes > 8*1024*1024 {
		c.eventBytes -= c.eventSizes[0]
		c.eventSizes = c.eventSizes[1:]
		c.state.Events = c.state.Events[1:]
	}
	switch name {
	case "ready", "code-updated", "content-updated":
		report, _ := event["report"].(string)
		manifest, _ := event["manifest"].(string)
		if !ownedPath(c.options.Output, report) || !goproject.IsOwnedCachePath(c.options.Project, manifest) || canonicalDestination(manifest) != nil {
			return errors.New("runner readiness names an invalid owned report/manifest")
		}
		if info, err := os.Lstat(manifest); err != nil || !info.Mode().IsRegular() {
			return errors.New("runner ready manifest is not a regular file")
		}
		data, err := readBounded(report+".ready.json", 4*1024*1024)
		var ready struct {
			Schema             int    `json:"schemaVersion"`
			PID                int    `json:"pid"`
			ControlDirectory   string `json:"controlDirectory"`
			Errors, Exceptions int
			Scene              struct {
				IsLoaded bool `json:"isLoaded"`
			}
		}
		if err != nil || json.Unmarshal(data, &ready) != nil || ready.Schema != 1 || ready.PID < 1 || numberPID(event["pid"]) != ready.PID || !ready.Scene.IsLoaded || ready.Errors != 0 || ready.Exceptions != 0 {
			return errors.New("runner returned an incompatible Player readiness identity")
		}
		root, err := openOwnedDirectory(c.options.Output, ready.ControlDirectory)
		if err != nil {
			return fmt.Errorf("runner returned an invalid control endpoint: %w", err)
		}
		root.Close()
		revision := filepath.Base(filepath.Dir(manifest))
		if !validID(revision) {
			return errors.New("runner readiness has no valid revision")
		}
		identity, parent, err := readProcessIdentity(ready.PID)
		if err != nil || ready.PID != command.Process.Pid && parent != command.Process.Pid {
			return errors.New("runner readiness PID does not belong to its session")
		}
		if ready.PID != command.Process.Pid {
			c.player = identity
		}
		c.state.PID, c.state.ControlDirectory, c.state.Revision = ready.PID, ready.ControlDirectory, revision
		if err := json.Unmarshal(data, &c.state.Ready); err != nil {
			return err
		}
		if !c.stopRequested {
			c.state.Phase = "ready"
		}
		c.state.LastError = ""
		c.state.LastReadySequence = sequence
		if name != "ready" || c.preparationUpdatePending {
			c.state.LastUpdateSequence = sequence
			c.state.UpdateError = ""
		}
		if c.preparationUpdatePending {
			event["preparationActivated"] = true
			c.preparationUpdatePending = false
		}
		c.lastReadyBaseline, c.lastReadyScene = c.options.Baseline, c.options.Scene
	case "updated":
		ack, ok := event["ack"].(map[string]any)
		revision, _ := ack["loadedRevision"].(string)
		if !ok || ack["success"] != true || ack["restartRequired"] != false || numberPID(ack["pid"]) != c.state.PID || !validID(revision) || c.state.PID < 1 {
			return errors.New("runner update lacks a confirmed Player revision/PID acknowledgement")
		}
		c.state.Revision = revision
		c.state.LastUpdateSequence = sequence
		c.state.LastReadySequence = sequence
		c.state.LastError = ""
		c.state.UpdateError = ""
	case "unchanged":
		revision, _ := event["revision"].(string)
		if c.state.Phase != "ready" || c.state.PID < 1 || numberPID(event["pid"]) != c.state.PID || !validID(revision) || revision != c.state.Revision {
			return errors.New("runner unchanged update does not confirm the accepted Player revision/PID")
		}
		c.state.LastUpdateSequence = sequence
		c.state.LastError = ""
		c.state.UpdateError = ""
	case "update-rejected":
		message, _ := event["error"].(string)
		if message == "" {
			message = "runner rejected the source update"
		}
		c.state.LastError, c.state.UpdateError = message, message
		c.state.LastUpdateRejectedSequence = sequence
	}
	return nil
}

func (c *Controller) Stop(ctx context.Context) (State, error) {
	c.mu.Lock()
	command, done := c.command, c.done
	if command == nil {
		c.mu.Unlock()
		return c.Status(), nil
	}
	c.state.Phase = "stopping"
	startShutdown := !c.stopRequested
	c.stopRequested = true
	grace, terminateGrace := c.stopGrace, c.terminateGrace
	c.notifyLocked()
	c.mu.Unlock()
	if startShutdown {
		go c.stopProcess(command, done, grace, terminateGrace)
	}
	select {
	case <-done:
		return c.Status(), nil
	case <-ctx.Done():
		return c.Status(), ctx.Err()
	}
}

func (c *Controller) stopProcess(command *exec.Cmd, done <-chan struct{}, grace, terminateGrace time.Duration) {
	for _, stage := range []struct {
		signal   syscall.Signal
		duration time.Duration
	}{{syscall.SIGINT, grace}, {syscall.SIGTERM, terminateGrace}, {syscall.SIGKILL, 0}} {
		select {
		case <-done:
			return
		default:
		}
		_ = syscall.Kill(-command.Process.Pid, stage.signal)
		if stage.duration == 0 {
			return
		}
		timer := time.NewTimer(stage.duration)
		select {
		case <-done:
			timer.Stop()
			return
		case <-timer.C:
		}
	}
}

func (c *Controller) Close() error {
	c.cancel()
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	_, err := c.Stop(ctx)
	return err
}

func (c *Controller) Wait(ctx context.Context, after uint64, ready bool) (State, error) {
	for {
		c.mu.Lock()
		state := c.snapshotLocked()
		changed := c.changed
		finished := state.Phase == "failed" || state.Phase == "stopped" || ready && state.Phase == "ready" && state.LastReadySequence > after || !ready && (state.LastUpdateSequence > after || state.LastUpdateRejectedSequence > after)
		c.mu.Unlock()
		if finished {
			if state.Phase == "failed" {
				return state, errors.New(state.LastError)
			}
			if state.Phase == "stopped" {
				return state, errors.New("runtime session stopped before the requested readiness/update")
			}
			if !ready && state.LastUpdateRejectedSequence > after && state.LastUpdateRejectedSequence > state.LastUpdateSequence {
				return state, errors.New(state.UpdateError)
			}
			return state, nil
		}
		select {
		case <-ctx.Done():
			return c.Status(), ctx.Err()
		case <-changed:
		}
	}
}

// Handle is the MCP boundary. Runtime commands accept expectedRevision and expectedPid
// so clients can bind observations to the Player they actually inspected.
func (c *Controller) Handle(ctx context.Context, operation string, raw json.RawMessage) (any, error) {
	if len(raw) > 65536 {
		return nil, errors.New("runtime arguments exceed 64 KiB")
	}
	var args map[string]json.RawMessage
	if len(raw) == 0 {
		raw = []byte("{}")
	}
	if err := json.Unmarshal(raw, &args); err != nil || args == nil {
		if err == nil {
			err = errors.New("runtime arguments must be an object")
		}
		return nil, err
	}
	if operation == "session_control" {
		var action string
		if err := json.Unmarshal(args["action"], &action); err != nil {
			return nil, err
		}
		operation = "session_" + action
	}
	switch operation {
	case "session_status":
		return c.Status(), nil
	case "session_start":
		return c.Start()
	case "session_stop":
		return c.Stop(ctx)
	case "session_reset":
		if _, err := c.Stop(ctx); err != nil {
			return nil, err
		}
		return c.Start()
	case "session_wait", "session_wait_update":
		var after uint64
		if value := args["afterSequence"]; value != nil {
			if err := json.Unmarshal(value, &after); err != nil {
				return nil, err
			}
		}
		return c.Wait(ctx, after, operation == "session_wait")
	case "runtime_query", "runtime_command":
		if err := json.Unmarshal(args["operation"], &operation); err != nil {
			return nil, errors.New("runtime operation is required")
		}
	}
	if !allowedOperation(operation) {
		return nil, fmt.Errorf("unsupported runtime operation %q", operation)
	}
	return c.Request(ctx, operation, args)
}

func (c *Controller) Request(ctx context.Context, operation string, args map[string]json.RawMessage) (any, error) {
	if !allowedOperation(operation) {
		return nil, fmt.Errorf("unsupported runtime operation %q", operation)
	}
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	id := newID()
	if value := args["requestId"]; value != nil {
		if err := json.Unmarshal(value, &id); err != nil {
			return nil, err
		}
	}
	if !validID(id) {
		return nil, errors.New("requestId must be ASCII letters, digits or hyphens, at most 64 characters")
	}
	arguments := args["arguments"]
	if len(arguments) == 0 {
		forward := make(map[string]json.RawMessage)
		for key, value := range args {
			if key != "operation" && key != "requestId" && key != "expectedPid" && key != "expectedRevision" {
				forward[key] = value
			}
		}
		arguments, _ = json.Marshal(forward)
	}
	if len(arguments) > 65536 {
		return nil, errors.New("runtime request exceeds 64 KiB")
	}
	var object map[string]any
	decoder := json.NewDecoder(bytes.NewReader(arguments))
	decoder.UseNumber()
	if decoder.Decode(&object) != nil || object == nil {
		return nil, errors.New("runtime arguments must be an object")
	}
	canonicalArguments, err := json.Marshal(object)
	if err != nil {
		return nil, err
	}
	var extra any
	if decoder.Decode(&extra) != io.EOF {
		return nil, errors.New("runtime arguments contain trailing data")
	}
	c.mu.Lock()
	state := c.state
	record := c.requests[id]
	pid, revision := state.PID, state.Revision
	if record != nil {
		pid, revision = record.PID, record.Revision
	}
	if value := args["expectedPid"]; value != nil {
		if err := json.Unmarshal(value, &pid); err != nil || pid < 1 {
			c.mu.Unlock()
			return nil, errors.New("expectedPid must be a positive integer")
		}
	}
	if value := args["expectedRevision"]; value != nil {
		if err := json.Unmarshal(value, &revision); err != nil || !validID(revision) {
			c.mu.Unlock()
			return nil, errors.New("expectedRevision must be a bounded revision identifier")
		}
	}
	if record != nil {
		if record.Operation != operation || record.PID != pid || record.Revision != revision || !bytes.Equal(record.Arguments, canonicalArguments) {
			c.mu.Unlock()
			return nil, errors.New("requestId was already used for another command or revision")
		}
	} else {
		if state.Phase != "ready" || state.PID < 1 || state.ControlDirectory == "" {
			c.mu.Unlock()
			return nil, errors.New("Player is not ready; use session_start and session_wait")
		}
		if len(c.requests) >= 4096 {
			c.mu.Unlock()
			return nil, errors.New("session request journal is full; inspect outstanding results and create a new controller")
		}
		if pid != state.PID || operation != "status" && revision != state.Revision {
			code, message := "stale_revision", "Command targets another scene revision"
			if pid != state.PID {
				code, message = "stale_process", "Command targets another Player process"
			}
			c.mu.Unlock()
			return map[string]any{"requestId": id, "pid": state.PID, "revision": state.Revision, "success": false, "error": map[string]any{"code": code, "message": message}}, nil
		}
		envelope := struct {
			Schema           int             `json:"schemaVersion"`
			RequestID        string          `json:"requestId"`
			ExpectedPID      int             `json:"expectedPid"`
			ExpectedRevision string          `json:"expectedRevision"`
			Operation        string          `json:"operation"`
			Arguments        json.RawMessage `json:"arguments"`
		}{1, id, pid, revision, operation, canonicalArguments}
		payload, err := json.Marshal(envelope)
		if err != nil {
			c.mu.Unlock()
			return nil, err
		}
		if len(payload) > 65536 {
			c.mu.Unlock()
			return nil, errors.New("runtime request exceeds 64 KiB")
		}
		hash := sha256.Sum256(payload)
		record = &requestRecord{ID: id, Operation: operation, SessionID: state.SessionID, Directory: state.ControlDirectory, PID: pid, Revision: revision, Hash: hex.EncodeToString(hash[:]), Arguments: canonicalArguments, Envelope: payload}
		if err := c.persistRequest(record); err != nil {
			c.mu.Unlock()
			return nil, err
		}
		c.requests[id] = record
	}
	c.mu.Unlock()
	if receipt, err := c.readJournalReceipt(record); err == nil {
		return decodeRequestReceipt(receipt, record, operation)
	} else if !errors.Is(err, os.ErrNotExist) {
		return nil, err
	}
	root, err := openOwnedDirectory(c.options.Output, record.Directory)
	if err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return map[string]any{"requestId": id, "status": "result_unknown", "response": filepath.Join(record.Directory, id+".response.json")}, nil
		}
		return nil, err
	}
	defer root.Close()
	current := c.Status()
	if current.SessionID == record.SessionID && current.PID == record.PID && current.ControlDirectory == record.Directory && current.Phase == "ready" {
		if err := ctx.Err(); err != nil {
			return nil, err
		}
		if err := publishRootImmutable(root, id+".request.json", record.Envelope); err != nil {
			return nil, err
		}
	}
	response := filepath.Join(record.Directory, id+".response.json")
	ticker := time.NewTicker(20 * time.Millisecond)
	defer ticker.Stop()
	for {
		contents, err := readRootBounded(root, id+".response.json", 4*1024*1024)
		if err == nil {
			result, err := decodeRequestReceipt(contents, record, operation)
			if err != nil {
				return nil, err
			}
			if err := c.persistReceipt(record, contents); err != nil {
				return nil, err
			}
			return result, nil
		}
		if !errors.Is(err, os.ErrNotExist) {
			return nil, err
		}
		current := c.Status()
		if current.SessionID != record.SessionID || current.PID != record.PID || current.ControlDirectory != record.Directory || current.Phase != "ready" {
			return map[string]any{"requestId": id, "status": "result_unknown", "response": response}, nil
		}
		select {
		case <-ctx.Done():
			return map[string]any{"requestId": id, "status": "result_pending", "response": response}, nil
		case <-ticker.C:
		}
	}
}

func allowedOperation(operation string) bool {
	switch operation {
	case "status", "hierarchy", "inspect", "logs", "pause", "resume", "wait_frames", "screenshot", "raycast", "custom", "tools", "invoke", "simulation_pause", "simulation_resume", "simulation_step", "input":
		return true
	}
	return false
}

func publishImmutable(path string, bytes []byte) error {
	root, err := openOwnedDirectory(filepath.Dir(path), filepath.Dir(path))
	if err != nil {
		return err
	}
	defer root.Close()
	return publishRootImmutable(root, filepath.Base(path), bytes)
}

func publishRootImmutable(root *os.Root, name string, bytes []byte) error {
	return publishRootBoundedImmutable(root, name, bytes, 65536)
}

func publishRootBoundedImmutable(root *os.Root, name string, bytes []byte, maximum int64) error {
	if int64(len(bytes)) > maximum {
		return errors.New("immutable artifact exceeds its size limit")
	}
	if old, err := readRootBounded(root, name, maximum); err == nil {
		if string(old) != string(bytes) {
			return errors.New("requestId was already used for another command")
		}
		return nil
	} else if !errors.Is(err, os.ErrNotExist) {
		return err
	}
	temporary := ".command-" + newID()
	file, err := root.OpenFile(temporary, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0600)
	if err != nil {
		return err
	}
	defer root.Remove(temporary)
	if err := file.Chmod(0600); err != nil {
		file.Close()
		return err
	}
	if _, err := file.Write(bytes); err != nil {
		file.Close()
		return err
	}
	if err := file.Sync(); err != nil {
		file.Close()
		return err
	}
	if err := file.Close(); err != nil {
		return err
	}
	if err := root.Link(temporary, name); err != nil {
		if !errors.Is(err, os.ErrExist) {
			return err
		}
		old, err := readRootBounded(root, name, maximum)
		if err != nil || string(old) != string(bytes) {
			return errors.New("requestId conflicts with a concurrent command")
		}
	}
	return nil
}

func readBounded(path string, maximum int64) ([]byte, error) {
	root, err := openOwnedDirectory(filepath.Dir(path), filepath.Dir(path))
	if err != nil {
		return nil, err
	}
	defer root.Close()
	return readRootBounded(root, filepath.Base(path), maximum)
}

func readRootBounded(root *os.Root, name string, maximum int64) ([]byte, error) {
	if maximum < 0 {
		return nil, errors.New("invalid control artifact size limit")
	}
	info, err := root.Lstat(name)
	if err != nil {
		return nil, err
	}
	if !info.Mode().IsRegular() || info.Size() > maximum {
		return nil, errors.New("control artifact is not a bounded regular file")
	}
	file, err := root.OpenFile(name, os.O_RDONLY|syscall.O_NOFOLLOW|syscall.O_NONBLOCK, 0)
	if err != nil {
		return nil, err
	}
	defer file.Close()
	opened, err := file.Stat()
	if err != nil || !opened.Mode().IsRegular() || !os.SameFile(info, opened) || opened.Size() > maximum {
		return nil, errors.New("control artifact changed or is not a bounded regular file")
	}
	bytes, err := io.ReadAll(io.LimitReader(file, maximum+1))
	if len(bytes) > int(maximum) {
		return nil, errors.New("control artifact exceeded its size limit")
	}
	return bytes, err
}

func ownedPath(root, path string) bool {
	return filepath.IsAbs(path) && filepath.Clean(path) == path && path != root && within(root, path)
}

func canonicalDestination(path string) error {
	if !filepath.IsAbs(path) || filepath.Clean(path) != path {
		return errors.New("control paths must be clean absolute paths")
	}
	for current := path; ; current = filepath.Dir(current) {
		_, err := os.Lstat(current)
		if errors.Is(err, os.ErrNotExist) {
			if filepath.Dir(current) == current {
				return err
			}
			continue
		}
		if err != nil {
			return err
		}
		canonical, err := filepath.EvalSymlinks(current)
		if err != nil || canonical != current {
			return errors.New("control path contains symlink ancestors: " + path)
		}
		return nil
	}
}

func openOwnedDirectory(root, path string) (*os.Root, error) {
	if !filepath.IsAbs(root) || !filepath.IsAbs(path) || filepath.Clean(path) != path || !within(root, path) {
		return nil, errors.New("control directory is outside its owned output")
	}
	if err := canonicalDestination(root); err != nil {
		return nil, err
	}
	if err := canonicalDestination(path); err != nil {
		return nil, err
	}
	parent, err := os.OpenRoot(root)
	if err != nil {
		return nil, err
	}
	if root == path {
		return parent, nil
	}
	relative, err := filepath.Rel(root, path)
	if err != nil {
		parent.Close()
		return nil, err
	}
	result, err := parent.OpenRoot(relative)
	parent.Close()
	return result, err
}

func numberPID(value any) int {
	var integer int64
	switch value := value.(type) {
	case json.Number:
		var err error
		integer, err = value.Int64()
		if err != nil {
			return 0
		}
	case float64:
		integer = int64(value)
		if float64(integer) != value {
			return 0
		}
	case int:
		integer = int64(value)
	default:
		return 0
	}
	if integer < 1 || integer > int64(^uint(0)>>1) {
		return 0
	}
	return int(integer)
}

func readProcessIdentity(pid int) (processIdentity, int, error) {
	file, err := os.Open(filepath.Join("/proc", strconv.Itoa(pid), "stat"))
	if err != nil {
		return processIdentity{}, 0, err
	}
	defer file.Close()
	data, err := io.ReadAll(io.LimitReader(file, 4097))
	if err != nil || len(data) > 4096 {
		return processIdentity{}, 0, errors.New("process identity exceeds bounds")
	}
	index := strings.LastIndexByte(string(data), ')')
	if index < 0 {
		return processIdentity{}, 0, errors.New("invalid process identity")
	}
	fields := strings.Fields(string(data[index+1:]))
	if len(fields) < 20 || fields[0] == "Z" {
		return processIdentity{}, 0, errors.New("process is missing or exited")
	}
	parent, err := strconv.Atoi(fields[1])
	if err != nil {
		return processIdentity{}, 0, err
	}
	return processIdentity{PID: pid, Started: fields[19]}, parent, nil
}

func signalOwnedPlayer(identity processIdentity, signal syscall.Signal) {
	if identity.PID < 1 {
		return
	}
	current, _, err := readProcessIdentity(identity.PID)
	group, groupErr := syscall.Getpgid(identity.PID)
	if err == nil && groupErr == nil && group == identity.PID && current.Started == identity.Started {
		_ = syscall.Kill(-identity.PID, signal)
	}
}

func within(root, path string) bool {
	relative, err := filepath.Rel(root, path)
	return err == nil && relative != ".." && !strings.HasPrefix(relative, ".."+string(filepath.Separator)) && !filepath.IsAbs(relative)
}
func validID(value string) bool {
	if len(value) < 1 || len(value) > 64 {
		return false
	}
	for _, c := range value {
		if !(c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' || c == '-') {
			return false
		}
	}
	return true
}
func newID() string {
	var value [16]byte
	if _, err := rand.Read(value[:]); err != nil {
		panic(err)
	}
	return hex.EncodeToString(value[:])
}
