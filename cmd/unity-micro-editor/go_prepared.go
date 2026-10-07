package main

import (
	"context"
	"errors"
	"fmt"
	"os"
	"os/signal"
	"path/filepath"
	"strings"
	"syscall"
	"time"

	goproject "unitymicroeditor/internal/project"
)

type preparedLaunch struct {
	SchemaVersion       int               `json:"schemaVersion"`
	Project             string            `json:"project"`
	Player              string            `json:"player"`
	PlayerSHA256        string            `json:"playerSha256"`
	Manifest            string            `json:"manifest"`
	ManifestSHA256      string            `json:"manifestSha256"`
	ContentRoutes       string            `json:"contentRoutes"`
	ContentRoutesSHA256 string            `json:"contentRoutesSha256"`
	AssemblyHashes      map[string]string `json:"assemblyHashes"`
}

type preparedScene struct {
	SchemaVersion int      `json:"schemaVersion"`
	Supported     bool     `json:"supported"`
	Project       string   `json:"projectRoot"`
	UnityVersion  string   `json:"unityVersion"`
	Assemblies    []string `json:"assemblies"`
	Compiler      struct {
		NativeOnly     bool   `json:"nativeOnly"`
		NativeSlotsSHA string `json:"nativeSlotsSha256"`
	} `json:"compiler"`
	Dependencies []struct {
		Path       string `json:"path"`
		SourcePath string `json:"sourcePath"`
		SHA256     string `json:"sha256"`
	} `json:"sourceDependencies"`
	ImportedPacks []struct {
		Path   string `json:"bundlePath"`
		SHA256 string `json:"sha256"`
	} `json:"importedAssetPacks"`
	NativeContent []struct {
		GUID         string                              `json:"guid"`
		Path         string                              `json:"path"`
		SHA256       string                              `json:"sha256"`
		Objects      []goproject.NativeContentObject     `json:"objects"`
		Dependencies []goproject.NativeContentDependency `json:"dependencies"`
		Resources    []struct {
			Path   string `json:"path"`
			SHA256 string `json:"sha256"`
		} `json:"resources"`
	} `json:"nativeContentFiles"`
	CompiledShaderSnapshots []goproject.CompiledShaderSnapshot `json:"compiledShaderSnapshots"`
	BuiltinShaderSnapshots  []goproject.RetainedBuiltinShader  `json:"builtinShaderSnapshots"`
}

func requirePreparedFile(path, expected string, observed map[string]string) error {
	if !filepath.IsAbs(path) || !validSHA256(expected) {
		return fmt.Errorf("prepared artifact needs an absolute path and exact SHA256: %s", path)
	}
	if previous, exists := observed[path]; exists {
		if previous != expected {
			return fmt.Errorf("prepared artifacts disagree on content: %s", path)
		}
		return nil
	}
	actual, err := goFileHash(path)
	if err != nil {
		return fmt.Errorf("read prepared artifact %s: %w", path, err)
	}
	if actual != expected {
		return fmt.Errorf("prepared artifact changed: %s (hash %s, expected %s)", path, actual, expected)
	}
	observed[path] = expected
	return nil
}

func validSHA256(value string) bool {
	if len(value) != 64 {
		return false
	}
	for _, character := range value {
		if character < '0' || character > '9' {
			if character < 'a' || character > 'f' {
				return false
			}
		}
	}
	return true
}

func resolvePreparedSource(project, path string) (string, error) {
	if filepath.IsAbs(path) {
		return path, nil
	}
	if path == "" || filepath.ToSlash(filepath.Clean(path)) != path || strings.HasPrefix(path, "../") {
		return "", fmt.Errorf("prepared source path is invalid: %s", path)
	}
	if strings.HasPrefix(path, "Packages/") {
		return resolveProjectImportSource(project, path)
	}
	return filepath.Join(project, filepath.FromSlash(path)), nil
}

func validatePreparedScene(launch preparedLaunch, path, expected string, observed map[string]string) (preparedScene, error) {
	var scene preparedScene
	if err := requirePreparedFile(path, expected, observed); err != nil {
		return scene, err
	}
	if err := readJSONFile(path, &scene); err != nil {
		return scene, err
	}
	if scene.SchemaVersion != 1 || !scene.Supported || scene.Project != launch.Project || scene.UnityVersion != "6000.6.3f1" {
		return scene, errors.New("prepared manifest does not match the supported source project/engine")
	}
	if scene.Compiler.NativeOnly {
		if len(scene.Assemblies) != 0 || !validSHA256(scene.Compiler.NativeSlotsSHA) || !filepath.IsAbs(launch.Player) {
			return scene, errors.New("native-only prepared manifest requires its verified host assembly registrations and no game assemblies")
		}
		playerData := filepath.Join(filepath.Dir(launch.Player), strings.TrimSuffix(filepath.Base(launch.Player), filepath.Ext(launch.Player))+"_Data")
		registrations := filepath.Join(playerData, "ScriptingAssemblies.json")
		if err := requirePreparedFile(registrations, scene.Compiler.NativeSlotsSHA, observed); err != nil {
			return scene, err
		}
		var slots struct {
			Names []string `json:"names"`
		}
		if err := readJSONFile(registrations, &slots); err != nil {
			return scene, err
		}
		bridge := false
		for _, name := range slots.Names {
			bridge = bridge || name == "Dungeon.CLIPlay.dll"
		}
		if !bridge {
			return scene, errors.New("native-only prepared host has no registered runtime bridge")
		}
	} else if len(scene.Assemblies) == 0 {
		return scene, errors.New("prepared manifest with no game assemblies requires a verified native-only compiler baseline")
	}
	for _, assembly := range scene.Assemblies {
		resolved, err := resolvePreparedSource(launch.Project, assembly)
		if err != nil {
			return scene, err
		}
		if err := requirePreparedFile(resolved, launch.AssemblyHashes[resolved], observed); err != nil {
			return scene, err
		}
	}
	for _, dependency := range scene.Dependencies {
		resolved := dependency.SourcePath
		if resolved == "" {
			var err error
			resolved, err = resolvePreparedSource(launch.Project, dependency.Path)
			if err != nil {
				return scene, err
			}
		}
		if err := requirePreparedFile(resolved, dependency.SHA256, observed); err != nil {
			return scene, err
		}
	}
	for _, snapshot := range scene.CompiledShaderSnapshots {
		if snapshot.SchemaVersion != 1 || snapshot.CompiledSourceRevisionVerified || snapshot.SourcePath == "" {
			return scene, errors.New("prepared compiled Shader snapshot provenance is unsupported")
		}
		if err := goproject.ValidateShaderSourceRequirements(launch.Project, snapshot.SourcePath, snapshot.SourceRequirements, snapshot.MissingIncludes); err != nil {
			return scene, err
		}
	}
	if len(scene.BuiltinShaderSnapshots) > 1 {
		return scene, errors.New("duplicate prepared retained built-in Shader snapshot")
	}
	for _, snapshot := range scene.BuiltinShaderSnapshots {
		if err := verifyBuiltinShaderSnapshot(snapshot); err != nil {
			return scene, err
		}
		found := false
		for _, content := range scene.NativeContent {
			if content.GUID == snapshot.GUID && content.Path == snapshot.Content.Path && content.SHA256 == snapshot.Content.SHA256 &&
				len(content.Objects) == 1 && content.Objects[0].FileID == snapshot.FileID && content.Objects[0].TypeName == "UnityEngine.Shader" && len(content.Resources) == 0 && len(content.Dependencies) == 0 {
				found = true
			}
		}
		if !found {
			return scene, errors.New("prepared retained built-in Shader has no matching content descriptor")
		}
	}
	for _, content := range scene.NativeContent {
		if content.GUID == goproject.BuiltinResourceGUID && len(scene.BuiltinShaderSnapshots) != 1 {
			return scene, errors.New("prepared retained built-in content requires verified Shader snapshot provenance")
		}
	}
	for _, pack := range scene.ImportedPacks {
		resolved, err := resolvePreparedSource(launch.Project, pack.Path)
		if err != nil {
			return scene, err
		}
		if err := requirePreparedFile(resolved, pack.SHA256, observed); err != nil {
			return scene, err
		}
	}
	for _, content := range scene.NativeContent {
		resolved, err := resolvePreparedSource(launch.Project, content.Path)
		if err != nil {
			return scene, err
		}
		if err := requirePreparedFile(resolved, content.SHA256, observed); err != nil {
			return scene, err
		}
		for _, resource := range content.Resources {
			resolved, err := resolvePreparedSource(launch.Project, resource.Path)
			if err != nil {
				return scene, err
			}
			if err := requirePreparedFile(resolved, resource.SHA256, observed); err != nil {
				return scene, err
			}
		}
	}
	return scene, nil
}

func runPreparedGoPlayer(opts launchOptions) error {
	if !filepath.IsAbs(opts.Prepared) {
		return errors.New("--prepared requires an absolute launch descriptor path")
	}
	var launch preparedLaunch
	if err := readJSONFile(opts.Prepared, &launch); err != nil {
		return err
	}
	if launch.SchemaVersion != 1 || launch.Project != opts.Project {
		return errors.New("prepared launch belongs to a different project or schema")
	}
	observed := map[string]string{}
	if err := requirePreparedFile(launch.Player, launch.PlayerSHA256, observed); err != nil {
		return err
	}
	if _, err := validatePreparedScene(launch, launch.Manifest, launch.ManifestSHA256, observed); err != nil {
		return err
	}
	if launch.ContentRoutes != "" {
		if err := requirePreparedFile(launch.ContentRoutes, launch.ContentRoutesSHA256, observed); err != nil {
			return err
		}
		var routes rawContentRoutes
		if err := readJSONFile(launch.ContentRoutes, &routes); err != nil {
			return err
		}
		if routes.SchemaVersion != 1 || routes.Project != launch.Project {
			return errors.New("prepared content routes do not match source project")
		}
		seen := map[string]bool{}
		for _, route := range routes.Scenes {
			if !sceneGUID.MatchString(route.GUID) || seen[route.GUID] {
				return errors.New("prepared content routes contain an invalid or duplicate GUID")
			}
			seen[route.GUID] = true
			if _, err := validatePreparedScene(launch, route.Manifest, route.SHA256, observed); err != nil {
				return err
			}
		}
	}
	if err := os.MkdirAll(opts.Output, 0700); err != nil {
		return err
	}
	lock, err := lockGoSession(opts.Output)
	if err != nil {
		return err
	}
	defer lock.Close()
	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()
	runDirectory := filepath.Join(opts.Output, "runs", randomID())
	if err := os.MkdirAll(runDirectory, 0700); err != nil {
		return err
	}
	if launch.ContentRoutes != "" {
		var routes rawContentRoutes
		if err := readJSONFile(launch.ContentRoutes, &routes); err != nil {
			return err
		}
		for _, route := range routes.Scenes {
			if err := stageNativeResources(route.Manifest, runDirectory); err != nil {
				return err
			}
		}
	}
	opts.RawRoutes = launch.ContentRoutes
	started := time.Now()
	player, err := startGoPlayer(ctx, opts, launch.Player, launch.Manifest, runDirectory, "")
	if err != nil {
		return err
	}
	defer player.stop()
	if err := player.waitReady(ctx, 45*time.Second); err != nil {
		return err
	}
	emitEvent(map[string]any{"event": "ready", "pid": player.command.Process.Pid, "manifest": launch.Manifest, "startupSeconds": time.Since(started).Seconds(), "report": player.report, "prepared": true})
	select {
	case err := <-player.done:
		player.exited = true
		if err != nil {
			return err
		}
		return requirePlayerReport(player.report)
	case <-ctx.Done():
		return nil
	}
}
