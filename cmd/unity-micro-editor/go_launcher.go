package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"strings"
	"syscall"
	"time"

	goproject "unitymicroeditor/internal/project"
)

type launchOptions struct {
	Project, Baseline, Output, Editor string
	Scene, RawRoutes                  string
	Prepared                          string
	ContentScenes, ContentPrefabs     stringListFlag
	BuiltinShaders                    stringListFlag
	GameArguments                     []string
	Watch, Interactive, PrepareOnly   bool
	Control                           bool
	Frames, Width, Height, FPS        int
}

type stringListFlag []string

func (values *stringListFlag) String() string { return strings.Join(*values, ",") }
func (values *stringListFlag) Set(value string) error {
	if strings.TrimSpace(value) == "" {
		return errors.New("empty content source")
	}
	*values = append(*values, value)
	return nil
}

type baselineBuild struct {
	Project      string `json:"project"`
	Manifest     string `json:"manifest"`
	Player       string `json:"player"`
	NativeImport struct {
		Catalog string `json:"catalog"`
	} `json:"nativeImport"`
}

func runGoCLI(args []string) error {
	var opts launchOptions
	flags := flag.NewFlagSet("run", flag.ContinueOnError)
	flags.StringVar(&opts.Project, "project", ".", "source project")
	flags.StringVar(&opts.Baseline, "baseline", "", "validated native host preparation current.json")
	flags.StringVar(&opts.Output, "output", "", "owned Go session cache beneath project Temp")
	flags.StringVar(&opts.Editor, "editor", defaultUnityEditor(), "matching Unity SDK, used for native import and C# compiler")
	flags.StringVar(&opts.Scene, "scene", "", "authored Assets/...unity scene, reusing the prepared host")
	flags.StringVar(&opts.Prepared, "prepared", "", "immutable launch descriptor produced from prepared source/content manifests")
	flags.Var(&opts.ContentScenes, "content-scene", "runtime scene GUID=Assets/...unity; repeat for additional scenes")
	flags.Var(&opts.ContentPrefabs, "content-prefab", "runtime-addressed Assets/...prefab; repeat for additional prefabs")
	flags.Var(&opts.BuiltinShaders, "builtin-shader", "verified retained native built-in Shader snapshot JSON")
	flags.BoolVar(&opts.Watch, "watch", false, "watch sources and update the running Player")
	flags.BoolVar(&opts.Interactive, "interactive", false, "keep the Player window open")
	flags.BoolVar(&opts.PrepareOnly, "prepare-only", false, "prepare immutable source/content/managed manifests without starting a Player")
	flags.BoolVar(&opts.Control, "control", false, "enable revision-checked agent commands in the interactive Player")
	flags.IntVar(&opts.Frames, "frames", 120, "frames in finite verification mode")
	flags.IntVar(&opts.Width, "width", 960, "window width")
	flags.IntVar(&opts.Height, "height", 540, "window height")
	flags.IntVar(&opts.FPS, "fps", 60, "frame cap")
	runnerArgs, gameArgs := splitGameArguments(args)
	opts.GameArguments = gameArgs
	if err := validateGameArguments(gameArgs); err != nil {
		return err
	}
	if err := flags.Parse(runnerArgs); err != nil {
		return err
	}
	if flags.NArg() != 0 || opts.Frames <= 0 || opts.FPS <= 0 || opts.Width < 1 || opts.Height < 1 {
		return errors.New("invalid run arguments")
	}
	if opts.Watch {
		opts.Interactive = true
	}
	if opts.Control && (!opts.Interactive || opts.PrepareOnly) {
		return errors.New("--control requires an interactive Player run")
	}
	project, err := filepath.Abs(opts.Project)
	if err != nil {
		return err
	}
	project, err = filepath.EvalSymlinks(project)
	if err != nil {
		return err
	}
	opts.Project = project
	if opts.Baseline == "" {
		opts.Baseline = filepath.Join(project, "Temp/UnityPlay/hybrid-sample/current.json")
	}
	if opts.Output == "" {
		opts.Output = filepath.Join(project, "Temp/UnityPlay/go-project")
	}
	opts.Output, err = filepath.Abs(opts.Output)
	if err != nil {
		return err
	}
	output, err := canonicalDestination(opts.Output)
	if err != nil {
		return err
	}
	if !goproject.IsOwnedCachePath(project, output) {
		return errors.New("Go session output must stay under project Temp or Library/UnityMicroEditor")
	}
	opts.Output = output
	if opts.Prepared != "" {
		if opts.Watch || opts.PrepareOnly || opts.Scene != "" || len(opts.ContentScenes) != 0 || len(opts.ContentPrefabs) != 0 || len(opts.BuiltinShaders) != 0 {
			return errors.New("--prepared is a launch mode; it cannot be combined with source preparation or --watch")
		}
		return runPreparedGoPlayer(opts)
	}
	var baseline baselineBuild
	if err := readJSONFile(opts.Baseline, &baseline); err != nil {
		return fmt.Errorf("read baseline: %w", err)
	}
	if baseline.Project != project || baseline.Manifest == "" || baseline.Player == "" {
		return errors.New("baseline is not a validated preparation for this project")
	}
	if err := os.MkdirAll(output, 0700); err != nil {
		return err
	}
	lock, err := lockGoSession(output)
	if err != nil {
		return err
	}
	defer lock.Close()
	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()
	started := time.Now()
	var watcher *projectWatcher
	if opts.Watch {
		watcher, err = newProjectWatcher(ctx, project)
		if err != nil {
			return err
		}
		defer watcher.Close()
	}
	stagedPlayer, err := stageGoHost(ctx, opts, baseline)
	if err != nil {
		return err
	}
	baseline.Player = stagedPlayer
	compiler, err := newGoCompiler(project, opts.Baseline, stagedPlayer, opts.Editor, filepath.Join(output, "compiler"))
	if err != nil {
		return err
	}
	initialCompile, err := compiler.Compile(ctx, nil)
	if err != nil {
		return err
	}
	if initialCompile != nil {
		baseline.Player = initialCompile.Player
	}
	scriptTypes, err := compiler.ScriptTypeNames(ctx)
	if err != nil {
		return err
	}
	session, err := goproject.NewSession(goproject.Options{Project: project, Manifest: baseline.Manifest, NativeCatalog: baseline.NativeImport.Catalog, Scene: opts.Scene, ExtraAssets: opts.ContentPrefabs, ScriptTypes: scriptTypes})
	if err != nil {
		return err
	}
	assemblies, err := playerAssemblyPaths(baseline.Player, baseline.Manifest)
	if err != nil {
		return err
	}
	var managed struct {
		Initializers []map[string]any `json:"runtimeInitializers"`
	}
	if err := readJSONFile(baseline.Manifest, &managed); err != nil {
		return err
	}
	session.SetManagedBindings(assemblies, nil, managed.Initializers)
	if initialCompile != nil {
		applyCompiledBindings(session, initialCompile, scriptTypes)
	}
	if err := applyGoBuiltinShaders(session, opts); err != nil {
		return err
	}
	nativeImporter, err := newNativeProjectImporter(opts, session)
	if err != nil {
		return err
	}
	prepared, err := nativeImporter.Prepare(ctx)
	if err != nil {
		return err
	}
	contentImporters := make(map[string]*nativeProjectImporter)
	if len(opts.ContentScenes) != 0 {
		opts.RawRoutes, err = prepareGoContentRoutes(ctx, opts, baseline, initialCompile, managed.Initializers, scriptTypes, contentImporters, nil)
		if err != nil {
			return err
		}
	}
	if opts.PrepareOnly {
		emitEvent(map[string]any{"event": "prepared", "player": baseline.Player, "manifest": prepared.Manifest, "contentRoutes": opts.RawRoutes, "prepareSeconds": prepared.ElapsedSeconds, "elapsedSeconds": time.Since(started).Seconds()})
		return nil
	}
	runDirectory := filepath.Join(output, "runs", randomID())
	if err := os.MkdirAll(runDirectory, 0700); err != nil {
		return err
	}
	request := filepath.Join(runDirectory, "reload-request.json")
	player, err := startGoPlayer(ctx, opts, baseline.Player, prepared.Manifest, runDirectory, request)
	if err != nil {
		return err
	}
	defer func() {
		if player != nil {
			player.stop()
		}
	}()
	if err := player.waitReady(ctx, 45*time.Second); err != nil {
		return err
	}
	emitEvent(map[string]any{"event": "ready", "pid": player.command.Process.Pid, "manifest": prepared.Manifest, "prepareSeconds": prepared.ElapsedSeconds, "startupSeconds": time.Since(started).Seconds(), "report": player.report})
	if !opts.Watch {
		select {
		case err := <-player.done:
			player.exited = true
			if err != nil {
				return err
			}
			return requirePlayerReport(player.report)
		case <-ctx.Done():
			return ctx.Err()
		}
	}
	var pendingCompile *compileUpdate
	activeCompile := initialCompile
	managedRejected := false
	for {
		select {
		case <-ctx.Done():
			return nil
		case err := <-player.done:
			player.exited = true
			if err != nil {
				return err
			}
			return requirePlayerReport(player.report)
		case change := <-watcher.events:
			if change.err != nil {
				return change.err
			}
			paths := []string{change.path}
			changedAt := change.at
			debounce := time.NewTimer(35 * time.Millisecond)
		collect:
			for {
				select {
				case next := <-watcher.events:
					if next.err != nil {
						return next.err
					}
					paths = append(paths, next.path)
				case <-debounce.C:
					break collect
				case <-ctx.Done():
					debounce.Stop()
					return nil
				}
			}
			session.Invalidate(paths)
			if err := applyGoBuiltinShaders(session, opts); err != nil {
				emitEvent(map[string]any{"event": "update-rejected", "error": err.Error(), "paths": paths})
				continue
			}
			updateStarted := time.Now()
			compilePaths := paths
			if managedRejected {
				compilePaths = nil
			}
			compiled, err := compiler.Compile(ctx, compilePaths)
			if err != nil {
				managedRejected = true
				emitEvent(map[string]any{"event": "update-rejected", "error": err.Error(), "paths": paths})
				continue
			}
			managedRejected = false
			if compiled != nil {
				pendingCompile = compiled
			}
			if pendingCompile != nil {
				nextScriptTypes, typeErr := compiler.ScriptTypeNames(ctx)
				if typeErr != nil {
					managedRejected = true
					emitEvent(map[string]any{"event": "update-rejected", "error": typeErr.Error(), "paths": paths})
					continue
				}
				scriptTypes = nextScriptTypes
				applyCompiledBindings(session, pendingCompile, scriptTypes)
			}
			next, err := nativeImporter.Prepare(ctx)
			if err != nil {
				emitEvent(map[string]any{"event": "update-rejected", "error": err.Error(), "paths": paths})
				continue
			}
			nextPlayer := player.command.Path
			nextCompile := activeCompile
			if pendingCompile != nil {
				nextPlayer, nextCompile = pendingCompile.Player, pendingCompile
			}
			nextRoutes := opts.RawRoutes
			if len(opts.ContentScenes) != 0 {
				nextBaseline := baseline
				nextBaseline.Player = nextPlayer
				nextRoutes, err = prepareGoContentRoutes(ctx, opts, nextBaseline, nextCompile, managed.Initializers, scriptTypes, contentImporters, paths)
				if err != nil {
					emitEvent(map[string]any{"event": "update-rejected", "error": err.Error(), "paths": paths})
					continue
				}
			}
			routesEqual, err := contentRoutesRuntimeEqual(opts.RawRoutes, nextRoutes)
			if err != nil {
				emitEvent(map[string]any{"event": "update-rejected", "error": err.Error(), "paths": paths})
				continue
			}
			if pendingCompile != nil || !routesEqual {
				// Mono cannot unload or replace an already loaded assembly identity.
				// Additive content routes are also fixed for one Player lifetime.
				// Publish a new immutable revision after stopping our instance.
				opts.RawRoutes = nextRoutes
				player.stop()
				runDirectory = filepath.Join(output, "runs", randomID())
				if err := os.MkdirAll(runDirectory, 0700); err != nil {
					return err
				}
				request = filepath.Join(runDirectory, "reload-request.json")
				player, err = startGoPlayer(ctx, opts, nextPlayer, next.Manifest, runDirectory, request)
				if err != nil {
					return err
				}
				if err := player.waitReady(ctx, 45*time.Second); err != nil {
					return err
				}
				event := map[string]any{"event": "content-updated", "paths": paths, "manifest": next.Manifest, "contentRoutes": nextRoutes, "sourceToReadySeconds": time.Since(changedAt).Seconds(), "pid": player.command.Process.Pid, "report": player.report}
				if pendingCompile != nil {
					event["event"] = "code-updated"
					event["sourceHashes"] = pendingCompile.SourceHashes
					event["compiledAssemblies"] = pendingCompile.CompiledAssemblies
					event["declarationChanged"] = pendingCompile.DeclarationChanged
					event["compileSeconds"] = pendingCompile.ElapsedSeconds
					activeCompile = pendingCompile
				}
				emitEvent(event)
				prepared = next
				pendingCompile = nil
				continue
			}
			same, err := sceneRuntimeEqual(prepared.Manifest, next.Manifest)
			if err != nil {
				return err
			}
			if same {
				emitEvent(map[string]any{"event": "unchanged", "paths": paths, "prepareSeconds": next.ElapsedSeconds, "pid": player.command.Process.Pid, "revision": prepared.Revision})
				continue
			}
			data, err := os.ReadFile(next.Manifest)
			if err != nil {
				return err
			}
			id := randomID()
			reload := map[string]any{"schemaVersion": 1, "requestId": id, "manifest": next.Manifest, "manifestSha256": bytesSHA(data), "revision": next.Revision, "requestedUtcUnixMilliseconds": time.Now().UnixMilli(), "sourceChangedUtcUnixMilliseconds": changedAt.UnixMilli()}
			patch, patchHash, err := makeScenePatch(prepared.Manifest, next.Manifest)
			if err != nil {
				return err
			}
			if patch != "" {
				reload["patch"] = patch
				reload["patchSha256"] = patchHash
			}
			if err := stageNativeResources(next.Manifest, runDirectory); err != nil {
				return err
			}
			if err := atomicJSON(request, reload); err != nil {
				return err
			}
			ack, err := waitReloadAck(ctx, request+".ack.json", id, 20*time.Second)
			if err != nil {
				emitEvent(map[string]any{"event": "update-rejected", "error": err.Error(), "ack": ack})
				if ack != nil && ack["restartRequired"] == false && ack["loadedRevision"] == prepared.Revision {
					continue
				}
				return fmt.Errorf("live Player revision is uncertain or requires a restart: %w", err)
			}
			if ack["loadedRevision"] != next.Revision || ack["restartRequired"] != false || ack["pid"] != float64(player.command.Process.Pid) {
				return errors.New("Player acknowledged an incompatible live revision or process")
			}
			emitEvent(map[string]any{"event": "updated", "paths": paths, "prepareSeconds": next.ElapsedSeconds, "elapsedSeconds": time.Since(updateStarted).Seconds(), "sourceToAckSeconds": time.Since(changedAt).Seconds(), "ack": ack, "cache": next.Cache})
			prepared = next
		}
	}
}

func applyCompiledBindings(session *goproject.Session, compiled *compileUpdate, scriptTypes map[string]string) {
	initializers := make([]map[string]any, 0, len(compiled.RuntimeInitializers))
	for _, value := range compiled.RuntimeInitializers {
		initializers = append(initializers, map[string]any{"assembly": value.Assembly, "typeName": value.TypeName, "methodName": value.MethodName, "phase": value.Phase})
	}
	session.SetManagedBindings(compiled.Assemblies, scriptTypes, initializers)
}

type goPlayer struct {
	command       *exec.Cmd
	done          chan error
	report, ready string
	log           *os.File
	exited        bool
}

func startGoPlayer(ctx context.Context, opts launchOptions, path, manifest, runDirectory, request string) (*goPlayer, error) {
	path, err := filepath.Abs(path)
	if err != nil {
		return nil, err
	}
	if err := stageNativeResources(manifest, runDirectory); err != nil {
		return nil, err
	}
	p := &goPlayer{done: make(chan error, 1), report: filepath.Join(runDirectory, "player-report.json"), ready: filepath.Join(runDirectory, "player-report.json.ready.json")}
	p.log, err = os.OpenFile(filepath.Join(runDirectory, "stdout.log"), os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0600)
	if err != nil {
		return nil, err
	}
	args := []string{"-force-glcore", "-screen-width", fmt.Sprint(opts.Width), "-screen-height", fmt.Sprint(opts.Height), "-screen-fullscreen", "0", "-logFile", filepath.Join(runDirectory, "player.log"), "--unity-cli-report", p.report, "--unity-cli-raw-manifest", manifest, "--unity-cli-frames", fmt.Sprint(opts.Frames), "--unity-cli-fps", fmt.Sprint(opts.FPS), "--unity-cli-no-vsync"}
	if opts.RawRoutes != "" {
		args = append(args, "--unity-cli-raw-routes", opts.RawRoutes)
	}
	if opts.Interactive {
		args = append(args, "--unity-cli-interactive")
		if opts.Control {
			args = append(args, "--unity-cli-control", filepath.Join(runDirectory, "control"))
		}
		if opts.Watch {
			args = append(args, "--unity-cli-raw-reload", request)
		}
	} else {
		args = append(args, "-batchmode")
	}
	args = append(args, opts.GameArguments...)
	p.command = exec.Command(path, args...)
	p.command.Dir = runDirectory
	p.command.Stdout = p.log
	p.command.Stderr = p.log
	p.command.SysProcAttr = &syscall.SysProcAttr{Setpgid: true, Pdeathsig: syscall.SIGTERM}
	p.command.Env = os.Environ()
	if os.Getenv("DISPLAY") == "" {
		p.command.Env = append(p.command.Env, "DISPLAY=:0")
	}
	if err := p.command.Start(); err != nil {
		p.log.Close()
		return nil, err
	}
	go func() { p.done <- p.command.Wait() }()
	return p, nil
}
func (p *goPlayer) waitReady(ctx context.Context, timeout time.Duration) error {
	timer := time.NewTimer(timeout)
	defer timer.Stop()
	ticker := time.NewTicker(20 * time.Millisecond)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-timer.C:
			return errors.New("Player readiness timeout; see " + p.report)
		case err := <-p.done:
			p.exited = true
			return fmt.Errorf("Player exited before ready: %v", err)
		case <-ticker.C:
			var ready struct {
				SchemaVersion      int `json:"schemaVersion"`
				PID                int `json:"pid"`
				Errors, Exceptions int
				GraphicsDeviceName string `json:"graphicsDeviceName"`
				GraphicsDeviceType string `json:"graphicsDeviceType"`
				Scene              struct {
					IsLoaded bool `json:"isLoaded"`
				}
			}
			if readJSONFile(p.ready, &ready) == nil {
				if ready.SchemaVersion != 1 || ready.PID != p.command.Process.Pid || !ready.Scene.IsLoaded || ready.Errors != 0 || ready.Exceptions != 0 {
					return fmt.Errorf("invalid Player ready report: pid=%d errors=%d exceptions=%d loaded=%v", ready.PID, ready.Errors, ready.Exceptions, ready.Scene.IsLoaded)
				}
				if err := requireHardwareGraphics(ready.GraphicsDeviceName, ready.GraphicsDeviceType); err != nil {
					return err
				}
				return nil
			}
		}
	}
}
func requireHardwareGraphics(device, kind string) error {
	if strings.TrimSpace(device) == "" || strings.TrimSpace(kind) == "" || strings.EqualFold(kind, "Null") {
		return errors.New("Player did not report an active GPU renderer")
	}
	name := strings.ToLower(device)
	for _, software := range []string{"llvmpipe", "softpipe", "lavapipe", "swiftshader", "software", "gdi generic", "microsoft basic render"} {
		if strings.Contains(name, software) {
			return fmt.Errorf("Player uses a software renderer: %s", device)
		}
	}
	return nil
}
func (p *goPlayer) stop() {
	defer p.log.Close()
	if p.exited {
		return
	}
	select {
	case <-p.done:
		p.exited = true
		return
	default:
	}
	_ = syscall.Kill(-p.command.Process.Pid, syscall.SIGINT)
	select {
	case <-p.done:
		p.exited = true
	case <-time.After(3 * time.Second):
		_ = syscall.Kill(-p.command.Process.Pid, syscall.SIGTERM)
		select {
		case <-p.done:
			p.exited = true
		case <-time.After(2 * time.Second):
			_ = syscall.Kill(-p.command.Process.Pid, syscall.SIGKILL)
			select {
			case <-p.done:
			case <-time.After(time.Second):
			}
		}
	}
}
func requirePlayerReport(path string) error {
	var report struct {
		Errors, Exceptions int
		FatalError         string `json:"fatalError"`
		ExitCode           int    `json:"exitCode"`
	}
	if err := readJSONFile(path, &report); err != nil {
		return err
	}
	if report.Errors != 0 || report.Exceptions != 0 || report.FatalError != "" || report.ExitCode != 0 {
		return fmt.Errorf("Player errors=%d exceptions=%d exit=%d: %s", report.Errors, report.Exceptions, report.ExitCode, report.FatalError)
	}
	return nil
}
func waitReloadAck(ctx context.Context, path, id string, timeout time.Duration) (map[string]any, error) {
	timer := time.NewTimer(timeout)
	defer timer.Stop()
	ticker := time.NewTicker(10 * time.Millisecond)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return nil, ctx.Err()
		case <-timer.C:
			return nil, errors.New("Player reload acknowledgement timed out")
		case <-ticker.C:
			var ack map[string]any
			if readJSONFile(path, &ack) == nil && ack["requestId"] == id {
				if ack["success"] != true {
					return ack, fmt.Errorf("Player rejected revision: %v", ack["fatalError"])
				}
				return ack, nil
			}
		}
	}
}
func readJSONFile(path string, value any) error {
	data, err := os.ReadFile(path)
	if err != nil {
		return err
	}
	return json.Unmarshal(data, value)
}
func atomicJSON(path string, value any) error {
	data, err := json.Marshal(value)
	if err != nil {
		return err
	}
	temporary := path + "." + randomID() + ".tmp"
	if err := os.WriteFile(temporary, append(data, '\n'), 0600); err != nil {
		return err
	}
	if err := os.Rename(temporary, path); err != nil {
		os.Remove(temporary)
		return err
	}
	return nil
}
func randomID() string { return fmt.Sprintf("%x-%x", time.Now().UnixNano(), os.Getpid()) }
func emitEvent(value any) {
	data, err := json.Marshal(value)
	if err == nil {
		fmt.Println(string(data))
	}
}

func relevantSource(path string) bool {
	switch strings.ToLower(filepath.Ext(path)) {
	case ".unity", ".prefab", ".asset", ".cs", ".dll", ".rsp", ".meta", ".asmdef", ".asmref", ".json", ".fbx", ".wav", ".png", ".mat", ".controller", ".anim", ".ogg", ".jpg", ".jpeg", ".shader", ".shadergraph", ".hlsl", ".cginc", ".compute", ".uxml", ".uss", ".inputactions":
		return true
	case ".mp3", ".aif", ".aiff", ".flac", ".obj", ".blend", ".dae", ".tga", ".psd", ".bmp", ".tif", ".tiff", ".hdr", ".exr", ".ttf", ".otf", ".bytes", ".txt", ".mp4", ".mov":
		// Unsupported native profiles must also reach Prepare so that a changed
		// dependency is rejected explicitly rather than silently kept stale.
		return true
	}
	return false
}
