// Package editorbridge attaches to a public Editor mailbox or owns a batch Editor.
// Receipts and accepted IDs survive Editor and MCP process restarts.
package editorbridge

import (
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

	goproject "unitymicroeditor/internal/project"
)

const Profile = "6000.6.3f1"
const requestLimit = 256 * 1024
const responseLimit = 32 * 1024 * 1024

type Options struct {
	Project, Editor string
	StartupTimeout  time.Duration
	PollInterval    time.Duration
}
type Request struct {
	ID, Action  string
	Arguments   json.RawMessage
	ExpectedPID int
}
type Failure struct {
	Code    string `json:"code"`
	Message string `json:"message"`
}
type Response struct {
	SchemaVersion int             `json:"schemaVersion"`
	Backend       string          `json:"backend"`
	RequestID     string          `json:"requestId"`
	RequestSHA256 string          `json:"requestSha256"`
	Project       string          `json:"project"`
	UnityVersion  string          `json:"unityVersion"`
	Editor        string          `json:"editor"`
	PID           int             `json:"pid"`
	Status        string          `json:"status"`
	Success       bool            `json:"success"`
	Accepted      bool            `json:"accepted"`
	Receipt       string          `json:"receipt"`
	Result        json.RawMessage `json:"result,omitempty"`
	Error         *Failure        `json:"error,omitempty"`
}
type Connection struct {
	Backend      string `json:"backend"`
	PID          int    `json:"pid"`
	Owned        bool   `json:"owned"`
	Mailbox      string `json:"mailbox"`
	Project      string `json:"project"`
	Editor       string `json:"editor"`
	UnityVersion string `json:"unityVersion"`
	BatchMode    bool   `json:"batchMode"`
	IsCompiling  bool   `json:"isCompiling"`
	IsUpdating   bool   `json:"isUpdating"`
}
type Preparation struct {
	Backend    string          `json:"backend"`
	PID        int             `json:"pid"`
	Graph      json.RawMessage `json:"graph"`
	SceneProof json.RawMessage `json:"sceneProof"`
}
type envelope struct {
	SchemaVersion int             `json:"schemaVersion"`
	ID            string          `json:"requestId"`
	Project       string          `json:"project"`
	UnityVersion  string          `json:"unityVersion"`
	Editor        string          `json:"editor"`
	ExpectedPID   int             `json:"expectedPid"`
	Action        string          `json:"action"`
	Arguments     json.RawMessage `json:"arguments"`
}
type heartbeat struct {
	SchemaVersion int    `json:"schemaVersion"`
	Backend       string `json:"backend"`
	Project       string `json:"project"`
	UnityVersion  string `json:"unityVersion"`
	Editor        string `json:"editor"`
	PID           int    `json:"pid"`
	OwnerToken    string `json:"ownerToken"`
	BatchMode     bool   `json:"batchMode"`
	IsCompiling   bool   `json:"isCompiling"`
	IsUpdating    bool   `json:"isUpdating"`
}
type Client struct {
	options             Options
	mailbox             string
	ctx                 context.Context
	cancel              context.CancelFunc
	startMu             sync.Mutex
	mu                  sync.Mutex
	command             *exec.Cmd
	done                chan struct{}
	ownerToken, started string
	exitError           error
	closed              bool
	stopGrace           time.Duration
}

func New(ctx context.Context, options Options) (*Client, error) {
	if ctx == nil || !filepath.IsAbs(options.Project) || !filepath.IsAbs(options.Editor) {
		return nil, errors.New("Editor bridge requires an explicit absolute project and Editor executable")
	}
	var err error
	options.Project, err = filepath.EvalSymlinks(options.Project)
	if err != nil {
		return nil, err
	}
	options.Editor, err = filepath.EvalSymlinks(options.Editor)
	if err != nil {
		return nil, err
	}
	if info, err := os.Stat(options.Project); err != nil || !info.IsDir() {
		return nil, errors.New("Editor project does not exist")
	}
	if info, err := os.Stat(options.Editor); err != nil || !info.Mode().IsRegular() || info.Mode().Perm()&0111 == 0 {
		return nil, errors.New("Editor must be an executable regular file")
	}
	version, err := readPath(filepath.Join(options.Project, "ProjectSettings/ProjectVersion.txt"), 65536)
	if err != nil || ValidateProjectVersion(version) != nil {
		return nil, fmt.Errorf("Editor bridge requires project profile Unity %s", Profile)
	}
	if options.StartupTimeout == 0 {
		options.StartupTimeout = 180 * time.Second
	}
	if options.StartupTimeout <= 0 || options.StartupTimeout > 15*time.Minute {
		return nil, errors.New("Editor startup timeout must be positive and at most 15 minutes")
	}
	if options.PollInterval == 0 {
		options.PollInterval = 50 * time.Millisecond
	}
	if options.PollInterval < time.Millisecond || options.PollInterval > time.Second {
		return nil, errors.New("Editor poll interval must be between 1 ms and 1 second")
	}
	if err := validateEditorExecutable(ctx, options.Editor); err != nil {
		return nil, err
	}
	mailbox := filepath.Join(options.Project, "Library/UnityMicroEditor/editor-bridge")
	if err := canonical(mailbox); err != nil {
		return nil, err
	}
	if err := os.MkdirAll(mailbox, 0700); err != nil {
		return nil, err
	}
	lifetime, cancel := context.WithCancel(ctx)
	c := &Client{options: options, mailbox: mailbox, ctx: lifetime, cancel: cancel, stopGrace: 10 * time.Second}
	go func() { <-lifetime.Done(); _ = c.Close() }()
	return c, nil
}

// ValidateProjectVersion accepts one exact pinned version line with LF, CRLF,
// or no final newline. Conflicting, suffixed and duplicate declarations fail.
func ValidateProjectVersion(data []byte) error {
	if len(data) > 65536 {
		return errors.New("project version file exceeds its byte bound")
	}
	count := 0
	for _, line := range strings.Split(string(data), "\n") {
		line = strings.TrimSpace(line)
		if !strings.HasPrefix(line, "m_EditorVersion:") {
			continue
		}
		count++
		if line != "m_EditorVersion: "+Profile {
			return fmt.Errorf("project requires the verified Unity %s version line", Profile)
		}
	}
	if count != 1 {
		return errors.New("project version file requires one unambiguous Editor version declaration")
	}
	return nil
}

func (c *Client) Mailbox() string { return c.mailbox }

// OwnProcessOrAttach never starts a second Editor for an already open project.
// It never owns or terminates a discovered interactive Editor.
func (c *Client) OwnProcessOrAttach(ctx context.Context) (Connection, error) {
	c.startMu.Lock()
	defer c.startMu.Unlock()
	if err := c.ctx.Err(); err != nil {
		return Connection{}, err
	}
	if err := ctx.Err(); err != nil {
		return Connection{}, err
	}
	unlock, err := c.startupLock(ctx)
	if err != nil {
		return Connection{}, err
	}
	defer unlock()
	deadline := time.Now().Add(c.options.StartupTimeout)
	startedHere := false
	for {
		if connected, err := c.readConnection(); err == nil {
			return connected, nil
		}
		pid, err := c.findEditor()
		if err != nil {
			return Connection{}, err
		}
		c.mu.Lock()
		command, done, exitError := c.command, c.done, c.exitError
		c.mu.Unlock()
		if pid == 0 && command == nil {
			if startedHere {
				return Connection{}, fmt.Errorf("owned batch Editor exited before bridge readiness: %v; inspect %s", exitError, filepath.Join(c.mailbox, "editor.log"))
			}
			if err := c.startBatch(); err != nil {
				return Connection{}, err
			}
			startedHere = true
		} else if command != nil {
			select {
			case <-done:
				if startedHere {
					return Connection{}, fmt.Errorf("owned batch Editor exited before readiness: %v", exitError)
				}
			default:
			}
		}
		if time.Now().After(deadline) {
			return Connection{}, fmt.Errorf("Editor bridge unavailable for project (Editor PID %d); install the runtime package and inspect compilation errors; log %s", pid, filepath.Join(c.mailbox, "editor.log"))
		}
		if err := c.wait(ctx); err != nil {
			return Connection{}, err
		}
	}
}

// Serializes discovery/startup across separate MCP or CLI clients. Unity's own
// project lock remains authoritative; this prevents racing two owned starts.
func (c *Client) startupLock(ctx context.Context) (func(), error) {
	root, err := c.openRoot()
	if err != nil {
		return nil, err
	}
	defer root.Close()
	file, err := root.OpenFile("startup.lock", os.O_CREATE|os.O_RDWR|syscall.O_NOFOLLOW|syscall.O_NONBLOCK, 0600)
	if err != nil {
		return nil, err
	}
	info, err := file.Stat()
	if err != nil || !info.Mode().IsRegular() {
		file.Close()
		return nil, errors.New("Editor startup lock is not a regular file")
	}
	for {
		err = syscall.Flock(int(file.Fd()), syscall.LOCK_EX|syscall.LOCK_NB)
		if err == nil {
			return func() { _ = syscall.Flock(int(file.Fd()), syscall.LOCK_UN); file.Close() }, nil
		}
		if err != syscall.EWOULDBLOCK && err != syscall.EAGAIN {
			file.Close()
			return nil, err
		}
		if err := c.wait(ctx); err != nil {
			file.Close()
			return nil, err
		}
	}
}

func (c *Client) readConnection() (Connection, error) {
	root, err := c.openRoot()
	if err != nil {
		return Connection{}, err
	}
	defer root.Close()
	data, err := read(root, "ready.json", 65536)
	if err != nil {
		return Connection{}, err
	}
	var h heartbeat
	if err := decode(data, &h); err != nil || h.SchemaVersion != 1 || h.Backend != "unity_editor" || h.Project != c.options.Project || h.Editor != c.options.Editor || h.UnityVersion != Profile || h.PID < 1 {
		return Connection{}, errors.New("Editor heartbeat has incompatible identity")
	}
	if !c.belongs(h.PID) {
		return Connection{}, errors.New("Editor heartbeat PID does not belong to this project and executable")
	}
	c.mu.Lock()
	owned := c.command != nil && c.command.Process.Pid == h.PID && c.ownerToken == h.OwnerToken
	c.mu.Unlock()
	return Connection{Backend: "unity_editor", PID: h.PID, Owned: owned, Mailbox: c.mailbox, Project: h.Project, Editor: h.Editor, UnityVersion: h.UnityVersion, BatchMode: h.BatchMode, IsCompiling: h.IsCompiling, IsUpdating: h.IsUpdating}, nil
}

func (c *Client) startBatch() error {
	if err := c.ctx.Err(); err != nil {
		return err
	}
	if err := validateEditorExecutable(c.ctx, c.options.Editor); err != nil {
		return err
	}
	if err := canonical(filepath.Join(c.mailbox, "editor.log")); err != nil {
		return err
	}
	owner := newID()
	command := exec.Command(c.options.Editor, "-batchmode", "-nographics", "-projectPath", c.options.Project, "-buildTarget", "Linux64", "-executeMethod", "Dungeon.CLIPlay.Editor.AgentEditorBridge.RunBatch", "-logFile", filepath.Join(c.mailbox, "editor.log"))
	command.Env = append(os.Environ(), "UNITY_MICRO_EDITOR_BRIDGE_OWNER="+owner)
	command.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	// Unity logs to its owned file. Avoid an unread stdout pipe blocking startup.
	command.Stdout, command.Stderr = io.Discard, io.Discard
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.closed || c.ctx.Err() != nil {
		return errors.New("Editor client is closed")
	}
	if c.command != nil {
		return nil
	}
	if err := command.Start(); err != nil {
		return err
	}
	started, _, _, err := process(command.Process.Pid)
	if err != nil {
		command.Process.Kill()
		command.Wait()
		return err
	}
	c.command, c.ownerToken, c.started, c.exitError = command, owner, started, nil
	c.done = make(chan struct{})
	done := c.done
	go func() {
		err := command.Wait()
		c.mu.Lock()
		if c.command == command {
			c.command = nil
			c.exitError = err
		}
		close(done)
		c.mu.Unlock()
	}()
	return nil
}

type boundedCommandOutput struct {
	mu       sync.Mutex
	data     bytes.Buffer
	overflow bool
}

func (output *boundedCommandOutput) Write(data []byte) (int, error) {
	output.mu.Lock()
	defer output.mu.Unlock()
	remaining := 4096 - output.data.Len()
	if len(data) > remaining {
		output.overflow = true
	}
	if remaining > 0 {
		output.data.Write(data[:min(len(data), remaining)])
	}
	return len(data), nil
}

func validateEditorExecutable(ctx context.Context, editor string) error {
	// A version-only invocation opens no project and acquires no project lock.
	// Validate before package installation can change authored inputs. Discard
	// excess output while retaining a strict bound even for a bad executable.
	versionCtx, cancel := context.WithTimeout(ctx, 15*time.Second)
	defer cancel()
	command := exec.CommandContext(versionCtx, editor, "-version")
	command.WaitDelay = time.Second
	var output boundedCommandOutput
	command.Stdout, command.Stderr = &output, &output
	if err := command.Run(); err != nil {
		return fmt.Errorf("Editor executable does not report pinned Unity %s: %w", Profile, err)
	}
	if output.overflow || strings.TrimSpace(output.data.String()) != Profile {
		return fmt.Errorf("Editor executable does not report pinned Unity %s", Profile)
	}
	return nil
}

func (c *Client) Do(ctx context.Context, request Request) (Response, error) {
	if err := ctx.Err(); err != nil {
		return Response{}, err
	}
	if request.ID == "" {
		request.ID = newID()
	}
	if !validID(request.ID) || !allowed(request.Action) || request.ExpectedPID < 0 {
		return Response{}, errors.New("Editor request requires a bounded immutable ID, allowlisted action and nonnegative expected PID")
	}
	arguments, err := canonicalArguments(request.Arguments)
	if err != nil {
		return Response{}, err
	}
	request.Arguments = arguments
	if request.Action == "prefab_prepare" || request.Action == "native_prepare" {
		if err := c.validateStaging(); err != nil {
			return Response{}, err
		}
	}
	root, err := c.openRoot()
	if err != nil {
		return Response{}, err
	}
	defer root.Close()
	data, accepted, err := existingEnvelope(root, request.ID)
	if err != nil {
		return Response{}, err
	}
	var original envelope
	if data != nil {
		if err := decode(data, &original); err != nil {
			return Response{}, err
		}
		if err := c.sameRequest(original, request); err != nil {
			return Response{}, err
		}
	} else {
		connection, err := c.OwnProcessOrAttach(ctx)
		if err != nil {
			return Response{}, err
		}
		pid := request.ExpectedPID
		if pid == 0 {
			pid = connection.PID
		}
		if pid != connection.PID {
			return Response{}, fmt.Errorf("Editor request targets stale PID %d; current PID %d", pid, connection.PID)
		}
		original = envelope{SchemaVersion: 1, ID: request.ID, Project: c.options.Project, UnityVersion: Profile, Editor: c.options.Editor, ExpectedPID: pid, Action: request.Action, Arguments: arguments}
		data, err = json.Marshal(original)
		if err != nil {
			return Response{}, err
		}
		data = append(data, '\n')
		if len(data) > requestLimit {
			return Response{}, errors.New("Editor request exceeds byte bound")
		}
		if err := publish(root, request.ID+".request.json", data); err != nil {
			if !errors.Is(err, os.ErrExist) {
				return Response{}, err
			}
			data, accepted, err = existingEnvelope(root, request.ID)
			if err != nil {
				return Response{}, err
			}
			if data == nil {
				return Response{}, errors.New("concurrent Editor request publication lost immutable envelope")
			}
			if err := decode(data, &original); err != nil {
				return Response{}, err
			}
			if err := c.sameRequest(original, request); err != nil {
				return Response{}, err
			}
		}
	}
	hash := digest(data)
	if accepted && c.belongs(original.ExpectedPID) {
		// Ask the current Editor to replay an unavailable receipt. Its accepted
		// marker prevents executing the action again, even across domain reload.
		if _, err := read(root, request.ID+".response.json", responseLimit); errors.Is(err, os.ErrNotExist) {
			if err := publish(root, request.ID+".request.json", data); err != nil && !errors.Is(err, os.ErrExist) {
				return Response{}, err
			}
		}
	}
	for {
		response, err := c.readResponse(root, original, hash)
		if err == nil {
			return response, nil
		}
		if !errors.Is(err, os.ErrNotExist) {
			return Response{}, err
		}
		if claim, err := read(root, request.ID+".accepted.json", requestLimit); err == nil {
			if !bytes.Equal(claim, data) {
				return Response{}, errors.New("Editor accepted marker differs from immutable request")
			}
			accepted = true
		} else if !errors.Is(err, os.ErrNotExist) {
			return Response{}, err
		}
		if !c.belongs(original.ExpectedPID) {
			return c.uncertain(original, hash, accepted, "result_unknown", "The original Editor process is unavailable; the immutable request will not be reexecuted."), nil
		}
		if err := c.wait(ctx); err != nil {
			return c.uncertain(original, hash, accepted, "result_pending", "Waiting ended; the delivered request may still complete. Retry the same request ID to read its receipt."), nil
		}
	}
}

func (c *Client) readResponse(root *os.Root, request envelope, hash string) (Response, error) {
	data, err := read(root, request.ID+".response.json", responseLimit)
	if err != nil {
		return Response{}, err
	}
	var response Response
	var fields map[string]json.RawMessage
	if err := decode(data, &response); err != nil {
		return Response{}, err
	}
	if err := decode(data, &fields); err != nil {
		return Response{}, err
	}
	if response.SchemaVersion != 1 || response.Backend != "unity_editor" || response.RequestID != request.ID || response.RequestSHA256 != hash || response.Project != c.options.Project || response.Editor != c.options.Editor || response.UnityVersion != Profile || response.Status != "completed" || response.PID < 1 || string(fields["success"]) != "true" && string(fields["success"]) != "false" || response.Success && response.PID != request.ExpectedPID || !response.Success && response.Error == nil {
		return Response{}, errors.New("Editor receipt identity, bounds or result is incompatible")
	}
	response.Accepted = true
	response.Receipt = filepath.Join(c.mailbox, request.ID+".response.json")
	return response, nil
}

func (c *Client) sameRequest(original envelope, request Request) error {
	arguments, err := canonicalArguments(original.Arguments)
	if err != nil {
		return err
	}
	if original.SchemaVersion != 1 || original.ID != request.ID || original.Project != c.options.Project || original.Editor != c.options.Editor || original.UnityVersion != Profile || original.Action != request.Action || original.ExpectedPID < 1 || request.ExpectedPID != 0 && request.ExpectedPID != original.ExpectedPID || !bytes.Equal(arguments, request.Arguments) {
		return errors.New("Editor request ID is already bound to different arguments or process guards")
	}
	return nil
}
func (c *Client) uncertain(request envelope, hash string, accepted bool, status, message string) Response {
	return Response{SchemaVersion: 1, Backend: "unity_editor", RequestID: request.ID, RequestSHA256: hash, Project: c.options.Project, UnityVersion: Profile, Editor: c.options.Editor, PID: request.ExpectedPID, Status: status, Accepted: accepted, Receipt: filepath.Join(c.mailbox, request.ID+".response.json"), Error: &Failure{Code: status, Message: message}}
}

// Prepare exports refreshed graph and saved-scene proof through the same real Editor.
func (c *Client) Prepare(ctx context.Context, scene string) (Preparation, error) {
	connection, err := c.OwnProcessOrAttach(ctx)
	if err != nil {
		return Preparation{}, err
	}
	preparation := Preparation{Backend: "unity_editor", PID: connection.PID}
	for _, action := range []string{"refresh", "export_graph", "scene_proof"} {
		args := json.RawMessage(`{}`)
		if action == "scene_proof" {
			args, _ = json.Marshal(map[string]string{"scene": scene})
		}
		response, err := c.Do(ctx, Request{Action: action, Arguments: args, ExpectedPID: connection.PID})
		if err != nil {
			return Preparation{}, err
		}
		if !response.Success {
			message := response.Status
			if response.Error != nil {
				message = response.Error.Message
			}
			return Preparation{}, fmt.Errorf("Editor %s: %s; request %s (%s)", action, message, response.RequestID, response.Status)
		}
		if action == "export_graph" {
			preparation.Graph = response.Result
		}
		if action == "scene_proof" {
			preparation.SceneProof = response.Result
		}
	}
	return preparation, nil
}

// SaveResult publishes JSON only into owned project caches, without overwriting.
func (c *Client) SaveResult(response Response, path string) error {
	if !response.Success || response.Status != "completed" || !json.Valid(response.Result) || !goproject.IsOwnedCachePath(c.options.Project, path) {
		return errors.New("only successful Editor results may be saved to a new owned cache file")
	}
	if err := canonical(path); err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(path), 0700); err != nil {
		return err
	}
	root, err := os.OpenRoot(filepath.Dir(path))
	if err != nil {
		return err
	}
	defer root.Close()
	return publish(root, filepath.Base(path), append(append([]byte(nil), response.Result...), '\n'))
}

func (c *Client) Close() error {
	c.mu.Lock()
	if c.closed {
		done := c.done
		c.mu.Unlock()
		if done != nil {
			select {
			case <-done:
			case <-time.After(20 * time.Second):
				return errors.New("owned Editor close timed out")
			}
		}
		return nil
	}
	c.closed = true
	c.cancel()
	command, done, started, grace, owner := c.command, c.done, c.started, c.stopGrace, c.ownerToken
	c.mu.Unlock()
	if command == nil {
		return nil
	}
	signal := func(sig syscall.Signal) {
		now, _, _, err := process(command.Process.Pid)
		if err == nil && now == started {
			_ = syscall.Kill(-command.Process.Pid, sig)
		}
	}
	// Ask the owned batch Editor to exit through its public update loop.
	// Unix SIGINT can crash Unity's native shutdown path on Linux; retain
	// bounded signals only if the Editor cannot process its mailbox.
	if root, err := c.openRoot(); err == nil {
		body, marshalErr := json.Marshal(map[string]any{"ownerToken": owner, "project": c.options.Project, "pid": command.Process.Pid})
		if marshalErr == nil {
			_ = publish(root, "owner-"+owner+".stop.json", append(body, '\n'))
		}
		root.Close()
	}
	select {
	case <-done:
		return nil
	case <-time.After(grace):
	}
	signal(syscall.SIGTERM)
	select {
	case <-done:
		return nil
	case <-time.After(3 * time.Second):
	}
	signal(syscall.SIGKILL)
	select {
	case <-done:
		return nil
	case <-time.After(3 * time.Second):
		return errors.New("owned Editor did not exit after bounded cleanup")
	}
}

func (c *Client) wait(ctx context.Context) error {
	timer := time.NewTimer(c.options.PollInterval)
	defer timer.Stop()
	select {
	case <-ctx.Done():
		return ctx.Err()
	case <-c.ctx.Done():
		return c.ctx.Err()
	case <-timer.C:
		return nil
	}
}
func (c *Client) openRoot() (*os.Root, error) {
	if err := canonical(c.mailbox); err != nil {
		return nil, err
	}
	return os.OpenRoot(c.mailbox)
}
func (c *Client) validateStaging() error {
	data, err := readPath(filepath.Join(c.options.Project, "ProjectSettings/UnityMicroEditorStaging.json"), 65536)
	if err != nil {
		return errors.New("Editor preparation requires an isolated staging project marker")
	}
	var marker struct {
		SchemaVersion int    `json:"schemaVersion"`
		SourceProject string `json:"sourceProject"`
	}
	if decode(data, &marker) != nil || marker.SchemaVersion != 1 || !filepath.IsAbs(marker.SourceProject) || filepath.Clean(marker.SourceProject) != marker.SourceProject || marker.SourceProject == c.options.Project {
		return errors.New("Editor preparation staging marker is invalid")
	}
	if err := canonical(marker.SourceProject); err != nil {
		return err
	}
	if info, err := os.Stat(marker.SourceProject); err != nil || !info.IsDir() {
		return errors.New("Editor preparation source project is absent")
	}
	return nil
}
func (c *Client) belongs(pid int) bool {
	started, args, _, err := process(pid)
	if err != nil || importWorker(args) {
		return false
	}
	c.mu.Lock()
	owned := c.command != nil && c.command.Process.Pid == pid && c.started == started
	c.mu.Unlock()
	if !owned {
		executable, err := os.Readlink(filepath.Join("/proc", strconv.Itoa(pid), "exe"))
		if err != nil || executable != c.options.Editor {
			return false
		}
	}
	project := argument(args, "-projectPath")
	if project == "" {
		return false
	}
	project, err = filepath.EvalSymlinks(project)
	return err == nil && project == c.options.Project
}
func (c *Client) findEditor() (int, error) {
	entries, err := os.ReadDir("/proc")
	if err != nil {
		return 0, err
	}
	for _, entry := range entries {
		pid, err := strconv.Atoi(entry.Name())
		if err != nil {
			continue
		}
		_, args, _, err := process(pid)
		if err != nil || importWorker(args) {
			continue
		}
		project := argument(args, "-projectPath")
		if project == "" {
			continue
		}
		project, err = filepath.EvalSymlinks(project)
		if err != nil || project != c.options.Project {
			continue
		}
		if c.belongs(pid) {
			return pid, nil
		}
		return 0, fmt.Errorf("project already has another Editor/runtime executable (PID %d); refusing a second Editor", pid)
	}
	return 0, nil
}
func importWorker(args []string) bool {
	return strings.HasPrefix(argument(args, "-name"), "AssetImportWorker") && argument(args, "-parentPid") != ""
}
func argument(args []string, name string) string {
	for index, arg := range args {
		if strings.EqualFold(arg, name) && index+1 < len(args) {
			return args[index+1]
		}
		if len(arg) > len(name)+1 && strings.EqualFold(arg[:len(name)+1], name+"=") {
			return arg[len(name)+1:]
		}
	}
	return ""
}
func process(pid int) (string, []string, int, error) {
	if pid < 1 {
		return "", nil, 0, errors.New("invalid process ID")
	}
	data, err := os.ReadFile(filepath.Join("/proc", strconv.Itoa(pid), "stat"))
	if err != nil {
		return "", nil, 0, err
	}
	index := strings.LastIndexByte(string(data), ')')
	if index < 0 {
		return "", nil, 0, errors.New("invalid process identity")
	}
	fields := strings.Fields(string(data[index+1:]))
	if len(fields) < 20 || fields[0] == "Z" {
		return "", nil, 0, errors.New("process is exited")
	}
	parent, err := strconv.Atoi(fields[1])
	if err != nil {
		return "", nil, 0, err
	}
	data, err = os.ReadFile(filepath.Join("/proc", strconv.Itoa(pid), "cmdline"))
	if err != nil || len(data) > 65536 {
		return "", nil, 0, errors.New("process arguments unavailable")
	}
	return fields[19], strings.Split(strings.TrimRight(string(data), "\x00"), "\x00"), parent, nil
}
func existingEnvelope(root *os.Root, id string) ([]byte, bool, error) {
	for _, suffix := range []string{".accepted.json", ".request.json"} {
		data, err := read(root, id+suffix, requestLimit)
		if err == nil {
			return data, suffix == ".accepted.json", nil
		}
		if !errors.Is(err, os.ErrNotExist) {
			return nil, false, err
		}
	}
	return nil, false, nil
}
func readPath(path string, limit int) ([]byte, error) {
	if err := canonical(path); err != nil {
		return nil, err
	}
	root, err := os.OpenRoot(filepath.Dir(path))
	if err != nil {
		return nil, err
	}
	defer root.Close()
	return read(root, filepath.Base(path), limit)
}
func read(root *os.Root, name string, limit int) ([]byte, error) {
	file, err := root.OpenFile(name, os.O_RDONLY|syscall.O_NOFOLLOW|syscall.O_NONBLOCK, 0)
	if err != nil {
		return nil, err
	}
	defer file.Close()
	info, err := file.Stat()
	if err != nil || !info.Mode().IsRegular() || info.Size() > int64(limit) {
		return nil, errors.New("Editor artifact is not a bounded regular file")
	}
	data, err := io.ReadAll(io.LimitReader(file, int64(limit)+1))
	if err != nil {
		return nil, err
	}
	if len(data) > limit {
		return nil, errors.New("Editor artifact exceeds byte bound")
	}
	return data, nil
}
func publish(root *os.Root, name string, data []byte) error {
	temporary := ".publish-" + newID()
	file, err := root.OpenFile(temporary, os.O_CREATE|os.O_EXCL|os.O_WRONLY|syscall.O_NOFOLLOW, 0600)
	if err != nil {
		return err
	}
	defer root.Remove(temporary)
	if _, err := file.Write(data); err != nil {
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
	return root.Link(temporary, name)
}
func canonical(path string) error {
	if !filepath.IsAbs(path) || filepath.Clean(path) != path {
		return errors.New("Editor path must be absolute and canonical")
	}
	for current := path; ; current = filepath.Dir(current) {
		info, err := os.Lstat(current)
		if err == nil && info.Mode()&os.ModeSymlink != 0 {
			return errors.New("Editor paths cannot contain symlinks")
		}
		if err != nil && !errors.Is(err, os.ErrNotExist) {
			return err
		}
		parent := filepath.Dir(current)
		if parent == current {
			break
		}
	}
	return nil
}
func canonicalArguments(data []byte) (json.RawMessage, error) {
	if len(data) == 0 {
		data = []byte(`{}`)
	}
	if len(data) > requestLimit {
		return nil, errors.New("Editor arguments exceed byte bound")
	}
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.UseNumber()
	value, err := jsonValue(decoder, 0)
	if err != nil {
		return nil, err
	}
	if _, ok := value.(map[string]any); !ok {
		return nil, errors.New("Editor arguments must be a JSON object")
	}
	if _, err := decoder.Token(); !errors.Is(err, io.EOF) {
		return nil, errors.New("trailing Editor argument JSON")
	}
	result, err := json.Marshal(value)
	return result, err
}
func jsonValue(decoder *json.Decoder, depth int) (any, error) {
	if depth > 128 {
		return nil, errors.New("JSON nesting exceeds bounds")
	}
	token, err := decoder.Token()
	if err != nil {
		return nil, err
	}
	delimiter, ok := token.(json.Delim)
	if !ok {
		return token, nil
	}
	switch delimiter {
	case '{':
		mapValue := map[string]any{}
		for decoder.More() {
			key, err := decoder.Token()
			if err != nil {
				return nil, err
			}
			name, ok := key.(string)
			if !ok {
				return nil, errors.New("JSON key must be string")
			}
			if _, exists := mapValue[name]; exists {
				return nil, errors.New("duplicate JSON argument key")
			}
			value, err := jsonValue(decoder, depth+1)
			if err != nil {
				return nil, err
			}
			mapValue[name] = value
		}
		_, err := decoder.Token()
		return mapValue, err
	case '[':
		var items []any
		for decoder.More() {
			value, err := jsonValue(decoder, depth+1)
			if err != nil {
				return nil, err
			}
			items = append(items, value)
		}
		_, err := decoder.Token()
		return items, err
	default:
		return nil, errors.New("unexpected JSON delimiter")
	}
}
func decode(data []byte, value any) error {
	decoder := json.NewDecoder(bytes.NewReader(data))
	if err := decoder.Decode(value); err != nil {
		return err
	}
	var extra any
	if err := decoder.Decode(&extra); !errors.Is(err, io.EOF) {
		return errors.New("trailing Editor artifact JSON")
	}
	return nil
}
func allowed(action string) bool {
	switch action {
	case "status", "export_graph", "scene_proof", "refresh", "build_project", "build_host", "prefab_prepare", "component_schema", "native_prepare":
		return true
	}
	return false
}
func validID(id string) bool {
	if len(id) < 1 || len(id) > 64 {
		return false
	}
	for _, c := range id {
		if !(c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' || c == '-') {
			return false
		}
	}
	return true
}
func digest(data []byte) string { sum := sha256.Sum256(data); return hex.EncodeToString(sum[:]) }
func newID() string {
	var data [16]byte
	if _, err := rand.Read(data[:]); err != nil {
		panic(err)
	}
	return hex.EncodeToString(data[:])
}
