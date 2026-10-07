package main

import (
	"bytes"
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"os/signal"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
	"syscall"
	"time"

	"unitymicroeditor/internal/agentcontrol"
	"unitymicroeditor/internal/editorbridge"
	goproject "unitymicroeditor/internal/project"
	"unitymicroeditor/internal/project/authoring"
)

var agentOperationID = regexp.MustCompile(`^[A-Za-z0-9-]{1,64}$`)

type agentProjectService struct {
	project, editor, host, scene, baseline string
	engineProfilePath                      string
	noEditor                               bool
	controller                             *agentcontrol.Controller
	lifetime                               context.Context
	mu                                     sync.Mutex
	client                                 *editorbridge.Client
	Async                                  bool
	stateMu                                sync.RWMutex
	jobMu                                  sync.Mutex
	jobs                                   map[string]*agentActionJob
	jobContext                             context.Context
	jobCancel                              context.CancelFunc
	jobClosed                              bool
	jobWait                                sync.WaitGroup
	autoPreparationID                      string
}

type agentPreparation struct {
	SchemaVersion            int               `json:"schemaVersion"`
	RequestID                string            `json:"requestId"`
	Project                  string            `json:"project"`
	Scene                    string            `json:"scene"`
	Baseline                 string            `json:"baseline"`
	HostPlayer               string            `json:"hostPlayer"`
	NativeCatalog            string            `json:"nativeCatalog,omitempty"`
	HostEvidence             string            `json:"hostEvidence,omitempty"`
	EngineProfile            string            `json:"engineProfile,omitempty"`
	PackageSnapshot          string            `json:"packageSnapshot,omitempty"`
	ArtifactReceipt          string            `json:"artifactReceipt,omitempty"`
	ComponentDefaultsProfile string            `json:"componentDefaultsProfile,omitempty"`
	ReferenceGraph           string            `json:"referenceGraph,omitempty"`
	Backends                 map[string]string `json:"backends"`
}

// Check a confirmed preparation before it can stop an accepted runtime. This
// reads descriptors and file identities only; Editor/SDK work is never replayed.
func validateAgentCachedPreparation(project, editor, scene, id, directory string, result agentPreparation) error {
	if result.SchemaVersion != 1 || result.Project != project || result.Scene != scene || result.RequestID != id || result.Baseline != filepath.Join(directory, "current.json") {
		return agentFailure("invalid_preparation_receipt", "cached preparation does not match its exact project, scene, requestId and owned baseline")
	}
	regularOwned := func(path, label string) error {
		if !filepath.IsAbs(path) || !goproject.IsOwnedCachePath(project, path) {
			return agentFailure("invalid_preparation_receipt", "cached "+label+" is outside owned project caches")
		}
		if err := agentCanonical(path); err != nil {
			return err
		}
		if info, err := os.Lstat(path); err != nil || !info.Mode().IsRegular() {
			return agentFailure("invalid_preparation_receipt", "cached "+label+" is missing or is not a canonical regular file")
		}
		return nil
	}
	if err := regularOwned(result.Baseline, "baseline"); err != nil {
		return err
	}
	var baseline struct {
		SchemaVersion int               `json:"schemaVersion"`
		Project       string            `json:"project"`
		Manifest      string            `json:"manifest"`
		Player        string            `json:"player"`
		Compiler      goCompileManifest `json:"compiler"`
	}
	if err := agentReadJSON(result.Baseline, 32<<20, &baseline); err != nil {
		return err
	}
	if baseline.SchemaVersion != 1 || baseline.Project != project || baseline.Compiler.Schema != 1 || baseline.Compiler.Project != project || baseline.Compiler.Unity != editor || len(baseline.Compiler.CompilerInputs) == 0 {
		return agentFailure("invalid_preparation_receipt", "cached baseline schema/project/compiler identity is invalid")
	}
	if err := regularOwned(baseline.Manifest, "source manifest"); err != nil {
		return err
	}
	var manifest struct {
		SchemaVersion int    `json:"schemaVersion"`
		ProjectRoot   string `json:"projectRoot"`
		ScenePath     string `json:"scenePath"`
		UnityVersion  string `json:"unityVersion"`
		Supported     bool   `json:"supported"`
	}
	if err := agentReadJSON(baseline.Manifest, 32<<20, &manifest); err != nil {
		return err
	}
	if manifest.SchemaVersion != 1 || !manifest.Supported || manifest.ProjectRoot != project || manifest.ScenePath != scene || manifest.UnityVersion != fixtureUnityVersion {
		return agentFailure("invalid_preparation_receipt", "cached source manifest does not match the supported project/scene/engine profile")
	}
	if err := regularOwned(baseline.Player, "Player"); err != nil {
		return err
	}
	if result.ArtifactReceipt != "" {
		return validateAgentPreparationArtifacts(project, result.Baseline, baseline.Player, result.ArtifactReceipt)
	}
	if baseline.Compiler.Backend == "go_authored_source" {
		return agentFailure("invalid_preparation_receipt", "source preparation lacks its frozen generated artifact inventory")
	}
	for _, assembly := range baseline.Compiler.Assemblies {
		actual, err := goFileHash(assembly.Path)
		if err != nil || !validSHA256(assembly.SHA256) || actual != assembly.SHA256 {
			return errors.Join(errors.New("cached compiled assembly changed"), err)
		}
	}
	return nil
}

func agentID() string {
	var b [16]byte
	if _, err := rand.Read(b[:]); err != nil {
		panic(err)
	}
	return hex.EncodeToString(b[:])
}
func agentString(args map[string]any, key string) string {
	value, _ := args[key].(string)
	return value
}
func agentFailure(code, message string) error { return &authoring.Error{Code: code, Message: message} }

func agentProjectBuildArguments(args map[string]any) (json.RawMessage, error) {
	development := false
	if value, present := args["development"]; present {
		var ok bool
		development, ok = value.(bool)
		if !ok {
			return nil, agentFailure("invalid_request", "development must be a boolean")
		}
	}
	return json.Marshal(map[string]any{"scenes": args["scenes"], "development": development})
}

func agentCanonical(path string) error {
	canonical, err := canonicalDestination(path)
	if err != nil {
		return err
	}
	if canonical != path {
		return errors.New("agent path is not canonical or crosses a symlink: " + path)
	}
	return nil
}

func (s *agentProjectService) Close() {
	s.closeActionJobs()
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.client != nil {
		_ = s.client.Close()
	}
}

// All coordinators sharing a project serialize preparation/publication. Unity's
// own project lock is a second boundary, not our transaction lock.
func (s *agentProjectService) locked(ctx context.Context, work func() (any, error)) (any, error) {
	for !s.mu.TryLock() {
		select {
		case <-ctx.Done():
			return nil, ctx.Err()
		case <-time.After(50 * time.Millisecond):
		}
	}
	defer s.mu.Unlock()
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	directory := filepath.Join(s.project, "Library/UnityMicroEditor")
	if err := agentCanonical(directory); err != nil {
		return nil, err
	}
	if err := os.MkdirAll(directory, 0700); err != nil {
		return nil, err
	}
	root, err := os.OpenRoot(directory)
	if err != nil {
		return nil, err
	}
	defer root.Close()
	lock, err := root.OpenFile("project-actions.lock", os.O_RDWR|os.O_CREATE|syscall.O_NOFOLLOW, 0600)
	if err != nil {
		return nil, err
	}
	defer lock.Close()
	for {
		err = syscall.Flock(int(lock.Fd()), syscall.LOCK_EX|syscall.LOCK_NB)
		if err == nil {
			break
		}
		if !errors.Is(err, syscall.EWOULDBLOCK) {
			return nil, err
		}
		select {
		case <-ctx.Done():
			return nil, ctx.Err()
		case <-time.After(50 * time.Millisecond):
		}
	}
	defer syscall.Flock(int(lock.Fd()), syscall.LOCK_UN)
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	return work()
}

func agentReadJSON(path string, limit int64, target any) error {
	data, err := readAgentFile(path, limit)
	if err != nil {
		return err
	}
	decoder := json.NewDecoder(bytes.NewReader(data))
	if err = decoder.Decode(target); err != nil {
		return err
	}
	var trailing any
	if err = decoder.Decode(&trailing); !errors.Is(err, io.EOF) {
		if err == nil {
			return errors.New("unexpected JSON content")
		}
		return err
	}
	return nil
}

func readAgentFile(path string, limit int64) ([]byte, error) {
	if err := agentCanonical(path); err != nil {
		return nil, err
	}
	info, err := os.Lstat(path)
	if err != nil {
		return nil, err
	}
	if !info.Mode().IsRegular() || info.Size() > limit {
		return nil, errors.New("expected a bounded regular file: " + path)
	}
	return os.ReadFile(path)
}

func agentImmutableJSON(path string, value any) error {
	data, err := json.Marshal(value)
	if err != nil {
		return err
	}
	data = append(data, '\n')
	if err := agentCanonical(path); err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(path), 0700); err != nil {
		return err
	}
	file, err := os.CreateTemp(filepath.Dir(path), ".immutable-agent-")
	if err != nil {
		return err
	}
	temporary := file.Name()
	defer os.Remove(temporary)
	if _, err = file.Write(data); err != nil {
		file.Close()
		return err
	}
	if err = file.Sync(); err != nil {
		file.Close()
		return err
	}
	if err = file.Close(); err != nil {
		return err
	}
	if err = os.Link(temporary, path); errors.Is(err, os.ErrExist) {
		existing, readErr := readAgentFile(path, 80<<20)
		if readErr != nil {
			return readErr
		}
		if bytes.Equal(existing, data) {
			return nil
		}
		return agentFailure("request_id_conflict", "request identity already belongs to different arguments or result")
	} else if err != nil {
		return err
	}
	directory, err := os.Open(filepath.Dir(path))
	if err != nil {
		return err
	}
	defer directory.Close()
	return directory.Sync()
}

// Package installation is explicit project preparation. Keep a durable backup,
// preserve unrelated JSON entries, and refuse a different installed package.
func installAgentPackage(project string) error {
	tool, err := microEditorRoot()
	if err != nil {
		return err
	}
	packageRoot, err := filepath.EvalSymlinks(filepath.Join(tool, "Packages/com.unitymicroeditor.runtime"))
	if err != nil {
		return err
	}
	path := filepath.Join(project, "Packages/manifest.json")
	before, err := readAgentFile(path, 1<<20)
	if err != nil {
		return err
	}
	var manifest map[string]any
	if err = json.Unmarshal(before, &manifest); err != nil {
		return err
	}
	dependencies, ok := manifest["dependencies"].(map[string]any)
	if !ok {
		return errors.New("package manifest dependencies must be an object")
	}
	expected := "file:" + filepath.ToSlash(packageRoot)
	if installed, exists := dependencies["com.unitymicroeditor.runtime"]; exists {
		if installed == expected {
			return nil
		}
		if location, ok := installed.(string); ok && strings.HasPrefix(location, "file:") {
			physical := strings.TrimPrefix(location, "file:")
			if !filepath.IsAbs(physical) {
				physical = filepath.Join(project, "Packages", physical)
			}
			canonical, resolveErr := filepath.EvalSymlinks(physical)
			if resolveErr == nil && canonical == packageRoot {
				return nil
			}
		}
		return agentFailure("package_conflict", "the target already requests another UnityMicroEditor package; resolve it explicitly before preparation")
	}
	dependencies["com.unitymicroeditor.runtime"] = expected
	after, err := json.MarshalIndent(manifest, "", "  ")
	if err != nil {
		return err
	}
	after = append(after, '\n')
	backup := filepath.Join(project, "Library/UnityMicroEditor/package-install", bytesSHA(before)+".json")
	if err = agentImmutableJSON(backup, map[string]any{"schemaVersion": 1, "project": project, "path": "Packages/manifest.json", "before": string(before), "beforeSha256": bytesSHA(before), "afterSha256": bytesSHA(after)}); err != nil {
		return err
	}
	latest, err := readAgentFile(path, 1<<20)
	if err != nil {
		return err
	}
	if !bytes.Equal(latest, before) {
		return agentFailure("revision_conflict", "package manifest changed during preparation")
	}
	temporary, err := os.CreateTemp(filepath.Dir(path), ".micro-editor-manifest-")
	if err != nil {
		return err
	}
	name := temporary.Name()
	defer os.Remove(name)
	if _, err = temporary.Write(after); err != nil {
		temporary.Close()
		return err
	}
	if err = temporary.Sync(); err != nil {
		temporary.Close()
		return err
	}
	if err = temporary.Close(); err != nil {
		return err
	}
	return os.Rename(name, path)
}

func (s *agentProjectService) editorClient(ctx context.Context) (*editorbridge.Client, error) {
	if s.noEditor {
		return nil, agentFailure("editor_disabled", "main Editor execution is disabled; provide a compatible cached engine/host/native profile")
	}
	if s.client == nil {
		var err error
		s.client, err = editorbridge.New(s.lifetime, editorbridge.Options{Project: s.project, Editor: s.editor, StartupTimeout: 10 * time.Minute})
		if err != nil {
			return nil, err
		}
	}
	// Validate the target profile/executable before changing authored package input.
	if err := installAgentPackage(s.project); err != nil {
		return nil, err
	}
	_, err := s.client.OwnProcessOrAttach(ctx)
	return s.client, err
}

func agentRequireResponse(response editorbridge.Response, err error) (json.RawMessage, error) {
	if err != nil {
		return nil, err
	}
	if response.Status != "completed" {
		return nil, &authoring.Error{Code: response.Status, Message: "Editor result is not confirmed; retry the same requestId", Details: map[string]any{"receipt": response.Receipt, "requestId": response.RequestID, "backend": response.Backend}}
	}
	if !response.Success {
		code, message := "editor_rejected", "Editor rejected operation"
		if response.Error != nil {
			code, message = response.Error.Code, response.Error.Message
		}
		return nil, &authoring.Error{Code: code, Message: message, Details: map[string]any{"receipt": response.Receipt, "requestId": response.RequestID, "backend": response.Backend}}
	}
	return response.Result, nil
}

func (s *agentProjectService) Handle(ctx context.Context, action string, raw json.RawMessage) (any, error) {
	if s.Async && agentAsyncAction(action) {
		result, err := s.enqueueAction(ctx, action, raw, s.HandleSync)
		if err == nil && action == "project_prepare" {
			return s.restorePreparedJobActivation(result, raw)
		}
		return result, err
	}
	return s.HandleSync(ctx, action, raw)
}

func (s *agentProjectService) HandleSync(ctx context.Context, action string, raw json.RawMessage) (any, error) {
	if len(raw) > 1<<20 {
		return nil, agentFailure("invalid_request", "project action exceeds 1 MiB")
	}
	var args map[string]any
	decoder := json.NewDecoder(bytes.NewReader(raw))
	decoder.UseNumber()
	if err := decoder.Decode(&args); err != nil || args == nil {
		return nil, agentFailure("invalid_request", "project arguments must be an object")
	}
	var trailing any
	if err := decoder.Decode(&trailing); !errors.Is(err, io.EOF) {
		return nil, agentFailure("invalid_request", "project arguments must contain exactly one object")
	}
	return s.locked(ctx, func() (any, error) {
		switch action {
		case "project_prepare":
			result, err := s.prepare(ctx, args)
			if err != nil {
				return nil, err
			}
			activate := true
			if value, ok := args["activate"].(bool); ok {
				activate = value
			}
			if activate && s.controller != nil {
				running := s.controller.Status().Phase != "stopped"
				if running {
					if _, err = s.controller.Stop(ctx); err != nil {
						return nil, err
					}
				}
				if err = s.controller.ConfigurePrepared(result.Baseline, result.Scene); err != nil {
					return nil, err
				}
				s.stateMu.Lock()
				s.baseline, s.scene = result.Baseline, result.Scene
				s.stateMu.Unlock()
				if running {
					if _, err = s.controller.Start(); err != nil {
						return nil, err
					}
				}
			}
			return result, nil
		case "project_build":
			encoded, err := agentProjectBuildArguments(args)
			if err != nil {
				return nil, err
			}
			client, err := s.editorClient(ctx)
			if err != nil {
				return nil, err
			}
			id := agentString(args, "requestId")
			if id == "" {
				id = agentID()
			}
			if !agentOperationID.MatchString(id) {
				return nil, agentFailure("invalid_request", "requestId must be at most 64 ASCII letters/digits/hyphens")
			}
			response, err := client.Do(ctx, editorbridge.Request{ID: id, Action: "build_project", Arguments: encoded})
			if err != nil {
				return nil, err
			}
			// Keep backend and request receipt visible even for build failure.
			var result map[string]any
			encoded, _ = json.Marshal(response)
			json.Unmarshal(encoded, &result)
			return result, nil
		case "prefab_edit":
			return s.prefabEdit(ctx, args)
		case "component_schema":
			return s.componentSchema(ctx, args)
		case "component_add":
			return s.componentAdd(ctx, args)
		default:
			return nil, agentFailure("unsupported_operation", "unknown project action "+action)
		}
	})
}

// Retained for historical Editor-prepared receipts. The active coordinator
// uses prepareEngineBootstrap followed by our source graph and SDK compiler.
func (s *agentProjectService) prepareEditor(ctx context.Context, args map[string]any) (agentPreparation, error) {
	var result agentPreparation
	scene := agentString(args, "scene")
	if scene == "" {
		scene = s.scene
	}
	if !strings.HasPrefix(scene, "Assets/") || filepath.ToSlash(filepath.Clean(scene)) != scene || filepath.Ext(scene) != ".unity" {
		return result, agentFailure("invalid_request", "preparation requires an explicit authored Assets/...unity scene")
	}
	id := agentString(args, "requestId")
	if id == "" {
		id = agentID()
	}
	if !agentOperationID.MatchString(id) {
		return result, agentFailure("invalid_request", "invalid preparation requestId")
	}
	host := agentString(args, "hostPlayer")
	if host == "" {
		host = s.host
	}
	directory := filepath.Join(s.project, "Library/UnityMicroEditor/preparations", id)
	if err := agentImmutableJSON(filepath.Join(directory, "request.json"), map[string]any{"schemaVersion": 1, "project": s.project, "scene": scene, "hostPlayer": host, "editor": s.editor, "nativeCatalog": agentString(args, "nativeCatalog")}); err != nil {
		return result, err
	}
	resultPath := filepath.Join(directory, "result.json")
	if err := agentReadJSON(resultPath, 1<<20, &result); err == nil {
		if err = validateAgentCachedPreparation(s.project, s.editor, scene, id, directory, result); err != nil {
			return result, err
		}
		return result, nil
	} else if !errors.Is(err, os.ErrNotExist) {
		return result, err
	}
	client, err := s.editorClient(ctx)
	if err != nil {
		return result, err
	}
	// An attached Editor may still hold the previously compiled local package.
	// Import current source before asking it to build the reusable host.
	response, callErr := client.Do(ctx, editorbridge.Request{ID: agentPhaseID(id, "initial-refresh"), Action: "refresh", Arguments: json.RawMessage(`{}`)})
	if _, err = agentRequireResponse(response, callErr); err != nil {
		return result, err
	}
	hostEvidencePath := ""
	if host == "" {
		var evidence map[string]any
		host, evidence, err = prepareAgentHost(ctx, s, id)
		if err != nil {
			return result, err
		}
		hostEvidencePath = filepath.Join(directory, "host-evidence.json")
		if err = agentImmutableJSON(hostEvidencePath, evidence); err != nil {
			return result, err
		}
		client, err = s.editorClient(ctx)
		if err != nil {
			return result, err
		}
	}
	// Export after the build restores public Player settings. Each phase gets
	// its own stable ID so a retry cannot rebuild or export different bytes.
	response, callErr = client.Do(ctx, editorbridge.Request{ID: agentPhaseID(id, "refresh"), Action: "refresh", Arguments: json.RawMessage(`{}`)})
	if _, err = agentRequireResponse(response, callErr); err != nil {
		return result, err
	}
	response, callErr = client.Do(ctx, editorbridge.Request{ID: agentPhaseID(id, "graph"), Action: "export_graph", Arguments: json.RawMessage(`{}`)})
	graph, err := agentRequireResponse(response, callErr)
	if err != nil {
		return result, err
	}
	graphPath := filepath.Join(directory, "graph.json")
	var graphValue any
	if err = json.Unmarshal(graph, &graphValue); err != nil {
		return result, err
	}
	if err = agentImmutableJSON(graphPath, graphValue); err != nil {
		return result, err
	}
	arguments, _ := json.Marshal(map[string]any{"scene": scene})
	response, callErr = client.Do(ctx, editorbridge.Request{ID: agentPhaseID(id, "proof"), Action: "scene_proof", Arguments: arguments})
	proof, err := agentRequireResponse(response, callErr)
	if err != nil {
		return result, err
	}
	proofPath := filepath.Join(directory, "scene-proof.json")
	var proofValue any
	if err = json.Unmarshal(proof, &proofValue); err != nil {
		return result, err
	}
	if err = agentImmutableJSON(proofPath, proofValue); err != nil {
		return result, err
	}
	compilerPath := filepath.Join(directory, "compiler.json")
	if _, err = os.Stat(compilerPath); errors.Is(err, os.ErrNotExist) {
		if _, err = prepareGoCompilerGraph(ctx, s.project, s.editor, host, graphPath, compilerPath); err != nil {
			return result, err
		}
	} else if err != nil {
		return result, err
	}
	nativeCatalog := agentString(args, "nativeCatalog")
	if nativeCatalog == "" {
		candidate := filepath.Join(directory, "native-catalog.json")
		if _, existsErr := os.Stat(candidate); existsErr == nil {
			nativeCatalog = candidate
		} else if !errors.Is(existsErr, os.ErrNotExist) {
			return result, existsErr
		} else {
			needed, needErr := agentMayNeedNative(s.project)
			if needErr != nil {
				return result, needErr
			}
			if needed {
				nativeCatalog, err = s.prepareNative(ctx, scene, id, directory)
				if err != nil {
					return result, err
				}
			}
		}
	}
	baseline := filepath.Join(directory, "current.json")
	if nativeCatalog == "" {
		nativeCatalog, err = agentIdentityCatalog(s.project, graphPath, directory)
		if err != nil {
			return result, err
		}
	}
	if _, err = os.Stat(baseline); errors.Is(err, os.ErrNotExist) {
		if err = prepareAgentProjectWithNative(ctx, s.project, s.editor, compilerPath, scene, baseline, proofPath, nativeCatalog); err != nil {
			return result, err
		}
	} else if err != nil {
		return result, err
	}
	result = agentPreparation{SchemaVersion: 1, RequestID: id, Project: s.project, Scene: scene, Baseline: baseline, HostPlayer: host, NativeCatalog: nativeCatalog, HostEvidence: hostEvidencePath, Backends: map[string]string{"sceneLoader": "raw_source", "compiler": "dotnet_sdk", "preparation": "unity_editor_public_api", "build": "unity_editor_proxy"}}
	if hostEvidencePath != "" {
		result.Backends["host"] = "isolated_unity_editor_proxy"
	}
	if err = agentImmutableJSON(resultPath, result); err != nil {
		return result, err
	}
	return result, nil
}

func (s *agentProjectService) Runtime(ctx context.Context, name string, raw json.RawMessage) (any, error) {
	if s.Async {
		result, err := s.runtimeWithActionJobs(ctx, name, raw)
		return s.recoverConfiguration(ctx, name, raw, result, err)
	}
	if name == "session_control" {
		var input struct {
			Action    string         `json:"action"`
			Arguments map[string]any `json:"arguments"`
		}
		if err := json.Unmarshal(raw, &input); err != nil {
			return nil, err
		}
		if input.Action == "start" || input.Action == "reset" {
			_, err := s.locked(ctx, func() (any, error) {
				if s.baseline != "" {
					return nil, nil
				}
				args := map[string]any{"scene": s.scene}
				if scene := agentString(input.Arguments, "scene"); scene != "" {
					args["scene"] = scene
				}
				result, err := s.prepare(ctx, args)
				if err != nil {
					return nil, err
				}
				if err = s.controller.ConfigurePrepared(result.Baseline, result.Scene); err != nil {
					return nil, err
				}
				s.stateMu.Lock()
				s.baseline, s.scene = result.Baseline, result.Scene
				s.stateMu.Unlock()
				return nil, nil
			})
			if err != nil {
				return nil, err
			}
		}
		if input.Action == "start" || input.Action == "reset" || input.Action == "stop" {
			return s.locked(ctx, func() (any, error) { return s.controller.Handle(ctx, name, raw) })
		}
	}
	return s.controller.Handle(ctx, name, raw)
}

func runAgentAutoPrepareCLI(args []string) error {
	var project, editor, scene, host, id, native, profile string
	var noEditor bool
	flags := flag.NewFlagSet("auto-prepare", flag.ContinueOnError)
	flags.StringVar(&project, "project", "", "explicit Unity project")
	flags.StringVar(&scene, "scene", "", "authored Assets scene")
	flags.StringVar(&editor, "editor", defaultUnityEditor(), "matching Unity Editor")
	flags.StringVar(&host, "host-player", "", "optional compatible package-aware host")
	flags.StringVar(&id, "request-id", "", "durable preparation identity")
	flags.StringVar(&native, "native-catalog", "", "optional validated native catalog")
	flags.StringVar(&profile, "engine-profile", "", "cached pinned engine/native-host profile")
	flags.BoolVar(&noEditor, "no-editor", false, "forbid main Editor execution, including native profile bootstrap")
	if err := flags.Parse(args); err != nil {
		return err
	}
	if flags.NArg() != 0 || project == "" || scene == "" {
		return errors.New("auto-prepare requires --project and --scene")
	}
	project, err := filepath.Abs(project)
	if err != nil {
		return err
	}
	project, err = filepath.EvalSymlinks(project)
	if err != nil {
		return err
	}
	editor, err = filepath.EvalSymlinks(editor)
	if err != nil {
		return err
	}
	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()
	service := &agentProjectService{project: project, editor: editor, lifetime: ctx, noEditor: noEditor, engineProfilePath: profile}
	defer service.Close()
	raw, _ := json.Marshal(map[string]any{"scene": scene, "hostPlayer": host, "requestId": id, "nativeCatalog": native, "activate": false})
	result, err := service.Handle(ctx, "project_prepare", raw)
	if err != nil {
		return err
	}
	return json.NewEncoder(os.Stdout).Encode(result)
}

func runAgentBuildProjectCLI(args []string) error {
	var project, editor, id, scenes string
	var development bool
	flags := flag.NewFlagSet("build-project", flag.ContinueOnError)
	flags.StringVar(&project, "project", "", "explicit Unity project")
	flags.StringVar(&editor, "editor", defaultUnityEditor(), "real Unity Editor backend")
	flags.StringVar(&id, "request-id", "", "durable build identity")
	flags.StringVar(&scenes, "scenes", "", "comma-separated saved Assets scenes")
	flags.BoolVar(&development, "development", false, "Unity Development build")
	if err := flags.Parse(args); err != nil {
		return err
	}
	if flags.NArg() != 0 || project == "" || scenes == "" {
		return errors.New("build-project requires --project and --scenes")
	}
	project, err := filepath.Abs(project)
	if err != nil {
		return err
	}
	project, err = filepath.EvalSymlinks(project)
	if err != nil {
		return err
	}
	editor, err = filepath.EvalSymlinks(editor)
	if err != nil {
		return err
	}
	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()
	service := &agentProjectService{project: project, editor: editor, lifetime: ctx}
	defer service.Close()
	raw, _ := json.Marshal(map[string]any{"scenes": strings.Split(scenes, ","), "development": development, "requestId": id})
	result, err := service.Handle(ctx, "project_build", raw)
	if err != nil {
		return err
	}
	if err = json.NewEncoder(os.Stdout).Encode(result); err != nil {
		return err
	}
	if reply, ok := result.(map[string]any); ok && reply["success"] == false {
		return fmt.Errorf("Unity Editor build did not complete successfully; see returned receipt")
	}
	return nil
}
