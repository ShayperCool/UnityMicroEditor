package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"os"
	"os/signal"
	"path/filepath"
	"strings"
	"syscall"

	goproject "unitymicroeditor/internal/project"
	"unitymicroeditor/internal/agentcontrol"
	"unitymicroeditor/internal/mcpserver"
)

func runCompilerPrepareCLI(args []string) error {
	var project, editor, player, graph, output string
	flags := flag.NewFlagSet("compiler-prepare", flag.ContinueOnError)
	flags.StringVar(&project, "project", "", "explicit Unity project root")
	flags.StringVar(&editor, "editor", defaultUnityEditor(), "matching installed Unity SDK")
	flags.StringVar(&player, "player", "", "compatible native Mono host Player")
	flags.StringVar(&graph, "graph", "", "public Editor compilation graph export")
	flags.StringVar(&output, "output", "", "new compiler preparation JSON under project Temp or Library/UnityMicroEditor")
	if err := flags.Parse(args); err != nil {
		return err
	}
	if flags.NArg() != 0 || project == "" || player == "" || graph == "" || output == "" {
		return errors.New("compiler-prepare requires --project, --player, --graph and --output")
	}
	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()
	result, err := prepareGoCompilerGraph(ctx, project, editor, player, graph, output)
	if err != nil {
		return err
	}
	return json.NewEncoder(os.Stdout).Encode(result)
}

func runAgentMCPCLI(args []string) error {
	var options mcpserver.Options
	var runtime agentcontrol.Options
	var host, engineProfilePath string
	var noEditor bool
	flags := flag.NewFlagSet("mcp", flag.ContinueOnError)
	flags.StringVar(&options.Project, "project", "", "explicit Unity project root")
	flags.StringVar(&options.Transport, "transport", "stdio", "stdio or http")
	flags.StringVar(&options.Listen, "listen", "127.0.0.1:8091", "loopback HTTP listener")
	flags.StringVar(&options.TokenFile, "token-file", "", "optional local bearer token file")
	flags.StringVar(&runtime.Baseline, "baseline", "", "validated prepared project current.json")
	flags.StringVar(&runtime.Output, "output", "", "owned runtime session output under project Temp or Library/UnityMicroEditor")
	flags.StringVar(&runtime.Editor, "editor", defaultUnityEditor(), "matching installed Unity SDK")
	flags.StringVar(&runtime.Scene, "scene", "", "authored scene selected for the runtime session")
	flags.StringVar(&host, "host-player", "", "optional compatible host; absent means automatic package-aware Editor host build")
	flags.StringVar(&engineProfilePath, "engine-profile", "", "optional cached native engine/host profile for our source graph")
	flags.BoolVar(&noEditor, "no-editor", false, "forbid main Editor execution; require prepared native profiles/defaults/imports")
	flags.IntVar(&runtime.FPS, "fps", 30, "runtime frame cap")
	if err := flags.Parse(args); err != nil {
		return err
	}
	if flags.NArg() != 0 || options.Project == "" {
		return errors.New("mcp requires an explicit --project")
	}
	project, err := filepath.Abs(options.Project)
	if err != nil {
		return err
	}
	options.Project, runtime.Project = project, project
	runtime.Runner, err = os.Executable()
	if err != nil {
		return err
	}
	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()
	controller, err := agentcontrol.New(ctx, runtime)
	if err != nil {
		return err
	}
	defer controller.Close()
	editor, err := filepath.EvalSymlinks(runtime.Editor)
	if err != nil {
		return err
	}
	service := &agentProjectService{project: project, editor: editor, host: host, scene: runtime.Scene, baseline: runtime.Baseline, controller: controller, lifetime: ctx, Async: true,
		engineProfilePath: engineProfilePath, noEditor: noEditor}
	defer service.Close()
	options.Runtime = service.Runtime
	options.ProjectActions = service.Handle
	options.Backends = map[string]any{"sourceAuthoring": "go_source", "runtime": "unity_mono_player", "compiler": "dotnet_sdk", "projectPreparation": "go_authored_source", "projectBuild": "unity_editor_proxy", "prefabApplyRevertUnpack": "raw_source", "componentDefaults": "versioned_component_cache", "mainEditorAllowed": !noEditor, "generalEditorExtensions": false, "editModeRunner": false}
	return mcpserver.RunOptions(ctx, options)
}

// First source preparation consumes an explicit, verified SDK/compiler baseline.
// It never invokes historical Python or regenerates authored game assets.
func runAgentProjectPrepareCLI(args []string) error {
	var project, editor, compilerPath, scene, output, sceneProof, nativeCatalog string
	flags := flag.NewFlagSet("prepare-project", flag.ContinueOnError)
	flags.StringVar(&project, "project", "", "explicit Unity project root")
	flags.StringVar(&editor, "editor", defaultUnityEditor(), "matching installed Unity SDK")
	flags.StringVar(&compilerPath, "compiler", "", "compiler-prepare result JSON")
	flags.StringVar(&scene, "scene", "", "authored Assets/...unity scene")
	flags.StringVar(&output, "output", "", "new current.json under project Temp or Library/UnityMicroEditor")
	flags.StringVar(&sceneProof, "scene-proof", "", "public Editor lighting proof bound to this authored scene")
	flags.StringVar(&nativeCatalog, "native-catalog", "", "verified public native asset catalog for this project; reuse without Editor import")
	if err := flags.Parse(args); err != nil {
		return err
	}
	if flags.NArg() != 0 || project == "" || compilerPath == "" || scene == "" || output == "" {
		return errors.New("prepare-project requires --project, --compiler, --scene and --output")
	}
	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()
	if err := prepareAgentProjectWithNative(ctx, project, editor, compilerPath, scene, output, sceneProof, nativeCatalog); err != nil {
		return err
	}
	data, err := os.ReadFile(output)
	if err != nil {
		return err
	}
	_, err = os.Stdout.Write(append(data, '\n'))
	return err
}

func prepareAgentProject(ctx context.Context, project, editor, compilerPath, scene, output, sceneProof string) error {
	return prepareAgentProjectWithNative(ctx, project, editor, compilerPath, scene, output, sceneProof, "")
}

func prepareAgentProjectWithNative(ctx context.Context, project, editor, compilerPath, scene, output, sceneProof, nativeCatalog string) error {
	var err error
	project, err = filepath.EvalSymlinks(project)
	if err != nil {
		return err
	}
	project, err = filepath.Abs(project)
	if err != nil {
		return err
	}
	compilerPath, err = filepath.Abs(compilerPath)
	if err != nil {
		return err
	}
	output, err = filepath.Abs(output)
	if err != nil {
		return err
	}
	canonical, err := canonicalDestination(output)
	if err != nil || canonical != output || !goproject.IsOwnedCachePath(project, output) {
		return errors.New("prepared current.json must be a canonical file under project Temp or Library/UnityMicroEditor")
	}
	if _, err := os.Lstat(output); !errors.Is(err, os.ErrNotExist) {
		return errors.New("prepared output already exists")
	}
	if !strings.HasPrefix(scene, "Assets/") || filepath.ToSlash(filepath.Clean(scene)) != scene || filepath.Ext(scene) != ".unity" {
		return errors.New("scene must be an authored Assets/...unity path")
	}
	contents, err := os.ReadFile(filepath.Join(project, filepath.FromSlash(scene)))
	if err != nil {
		return err
	}
	if err := validateAgentSceneLighting(contents, project, scene, sceneProof); err != nil {
		return err
	}
	documents, _ := goproject.ParseUnityYAML(contents)
	verifiedEmptyBuiltinLighting := false
	for _, document := range documents {
		if document.ClassID == 157 {
			if reference, ok := document.Data["m_LightingDataAsset"].(map[string]any); ok && reference["guid"] == goproject.BuiltinResourceGUID {
				verifiedEmptyBuiltinLighting = true // validated identity and public proof above
			}
		}
	}
	var preparation goCompilerPreparationResult
	if err := readJSONFile(compilerPath, &preparation); err != nil {
		return err
	}
	if preparation.SchemaVersion != 1 || preparation.Compiler.Project != project {
		return errors.New("compiler preparation belongs to another project")
	}
	if err := os.MkdirAll(filepath.Dir(output), 0700); err != nil {
		return err
	}
	staging, err := os.MkdirTemp(filepath.Dir(output), ".source-preparation-")
	if err != nil {
		return err
	}
	published := false
	defer func() {
		if !published {
			os.RemoveAll(staging)
		}
	}()
	compiler, err := newGoCompiler(project, compilerPath, preparation.Player, editor, filepath.Join(staging, "compiler-cache"))
	if err != nil {
		return err
	}
	activePlayer := preparation.Player
	compiled, err := compiler.Compile(ctx, nil)
	if err != nil {
		return err
	}
	if compiled != nil {
		activePlayer = compiled.Player
	}
	types, err := compiler.ScriptTypeNames(ctx)
	if err != nil {
		return err
	}
	acceptedCompiler, err := compiler.PreparedManifest(ctx)
	if err != nil {
		return err
	}
	assemblies := make([]string, 0, len(acceptedCompiler.Assemblies))
	initializers := []runtimeInitializer{}
	for _, assembly := range acceptedCompiler.Assemblies {
		assemblies = append(assemblies, assembly.Path)
		initializers = append(initializers, assembly.LifecycleBarrier.RuntimeInitializers...)
	}
	seed := map[string]any{"schemaVersion": 1, "supported": true, "unityVersion": fixtureUnityVersion, "projectRoot": project,
		"scenePath": scene, "sceneName": strings.TrimSuffix(filepath.Base(scene), ".unity"), "objects": []any{}, "assets": []any{},
		"assemblies": assemblies, "sourceDependencies": []any{}, "lifecycleBarrierVersion": 1, "lifecycleBarrierAssemblies": assemblies,
		"scriptableObjectBarrierVersion": 1, "runtimeInitializers": initializers, "emptyBuiltinLightingDataVerified": verifiedEmptyBuiltinLighting}
	if compiler.manifest.NativeOnly {
		seed["compiler"] = map[string]any{"nativeOnly": true, "nativeSlotsSha256": compiler.manifest.NativeSlotsSHA}
	}
	seedPath := filepath.Join(staging, "source-seed.json")
	if err := attachAgentNativeSeed(seed, project, nativeCatalog); err != nil {
		return err
	}
	if err := atomicJSON(seedPath, seed); err != nil {
		return err
	}
	session, err := goproject.NewSession(goproject.Options{Project: project, Manifest: seedPath, ScriptTypes: types, NativeCatalog: nativeCatalog})
	if err != nil {
		return err
	}
	prepared, err := session.Prepare(filepath.Join(staging, "source"))
	if err != nil {
		return err
	}
	var manifestSources struct {
		Dependencies []struct {
			Path   string `json:"path"`
			SHA256 string `json:"sha256"`
		} `json:"sourceDependencies"`
	}
	if err := readJSONFile(prepared.Manifest, &manifestSources); err != nil {
		return err
	}
	matched := false
	for _, dependency := range manifestSources.Dependencies {
		if dependency.Path == scene && dependency.SHA256 == bytesSHA(contents) {
			matched = true
		}
	}
	latestContents, err := os.ReadFile(filepath.Join(project, filepath.FromSlash(scene)))
	if err != nil {
		return err
	}
	if !matched || bytesSHA(latestContents) != bytesSHA(contents) {
		return errors.New("authored scene changed after its preparation/lighting proof; export a fresh proof and retry")
	}
	if err := validateAgentSceneLighting(latestContents, project, scene, sceneProof); err != nil {
		return err
	}
	preparationBackend := "public-editor-graph-sdk-raw-source"
	if acceptedCompiler.Backend == "go_authored_source" {
		preparationBackend = "go-source-graph-sdk-raw-source"
	}
	current := map[string]any{"schemaVersion": 1, "project": project, "manifest": prepared.Manifest, "player": activePlayer, "compiler": acceptedCompiler,
		"nativeImport": map[string]any{"catalog": nativeCatalog}, "preparation": preparationBackend}
	if err := publishAgentBaseline(output, current); err != nil {
		return err
	}
	published = true
	return nil
}
