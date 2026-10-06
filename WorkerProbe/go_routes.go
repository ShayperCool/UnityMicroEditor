package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strings"

	goproject "unitymicroeditor/project"
)

var sceneGUID = regexp.MustCompile(`^[0-9a-f]{32}$`)
var contentRouteSHA = regexp.MustCompile(`^[0-9a-f]{64}$`)

type rawSceneRoute struct {
	GUID     string `json:"guid"`
	Manifest string `json:"manifest"`
	SHA256   string `json:"sha256"`
}

type rawContentRoutes struct {
	SchemaVersion int             `json:"schemaVersion"`
	Project       string          `json:"project"`
	Scenes        []rawSceneRoute `json:"scenes"`
}

func splitGameArguments(args []string) ([]string, []string) {
	for index, value := range args {
		if value == "--" {
			return args[:index], append([]string(nil), args[index+1:]...)
		}
	}
	return args, nil
}

func validateGameArguments(args []string) error {
	for _, value := range args {
		if strings.HasPrefix(value, "--unity-cli-") {
			return errors.New("game arguments must not override the runner's --unity-cli-* protocol")
		}
		if strings.ContainsRune(value, '\x00') {
			return errors.New("game arguments contain NUL")
		}
	}
	return nil
}

func parseContentScene(value string) (string, string, error) {
	guid, path, exists := strings.Cut(value, "=")
	if !exists || !sceneGUID.MatchString(guid) || !strings.HasPrefix(path, "Assets/") || filepath.ToSlash(filepath.Clean(path)) != path || filepath.Ext(path) != ".unity" {
		return "", "", fmt.Errorf("content scene must be lowercase GUID=Assets/...unity: %s", value)
	}
	return guid, path, nil
}

func prepareGoContentRoutes(ctx context.Context, opts launchOptions, baseline baselineBuild, compiled *compileUpdate, initializers []map[string]any, scriptTypes map[string]string, importers map[string]*nativeProjectImporter, changedPaths []string) (string, error) {
	routes := rawContentRoutes{SchemaVersion: 1, Project: opts.Project}
	if importers == nil {
		importers = make(map[string]*nativeProjectImporter)
	}
	seen := map[string]bool{}
	assemblies, err := playerAssemblyPaths(baseline.Player, baseline.Manifest)
	if err != nil {
		return "", err
	}
	for _, declaration := range opts.ContentScenes {
		guid, scene, err := parseContentScene(declaration)
		if err != nil {
			return "", err
		}
		if seen[guid] {
			return "", fmt.Errorf("duplicate content scene GUID %s", guid)
		}
		seen[guid] = true
		metadataBytes, err := os.ReadFile(filepath.Join(opts.Project, filepath.FromSlash(scene)) + ".meta")
		if err != nil {
			return "", err
		}
		metadata, err := registryMetaGUID(metadataBytes)
		if err != nil || metadata != guid {
			return "", fmt.Errorf("content scene GUID disagrees with authored metadata: %s", scene)
		}
		importer := importers[guid]
		if importer == nil {
			// Global runtime-addressed prefabs belong to the root session. Follow
			// each additive scene's authored references without duplicating them.
			session, err := goproject.NewSession(goproject.Options{Project: opts.Project, Manifest: baseline.Manifest, NativeCatalog: baseline.NativeImport.Catalog, Scene: scene, ScriptTypes: scriptTypes})
			if err != nil {
				return "", err
			}
			routeOptions := opts
			routeOptions.Scene = scene
			routeOptions.Output = filepath.Join(opts.Output, "content", guid)
			importer, err = newNativeProjectImporter(routeOptions, session)
			if err != nil {
				return "", err
			}
			importers[guid] = importer
		} else if importer.opts.Project != opts.Project || importer.opts.Scene != scene {
			return "", errors.New("a cached content route cannot switch source projects or scenes")
		}
		// Reuse parsed YAML and validated native entries. Metadata watcher
		// events still invalidate the GUID index, and Prepare checks sources.
		importer.session.Invalidate(changedPaths)
		importer.session.SetManagedBindings(assemblies, scriptTypes, initializers)
		if compiled != nil {
			applyCompiledBindings(importer.session, compiled)
		}
		if err := applyGoBuiltinShaders(importer.session, opts); err != nil {
			return "", err
		}
		prepared, err := importer.Prepare(ctx)
		if err != nil {
			return "", fmt.Errorf("prepare content scene %s: %w", scene, err)
		}
		hash, err := goFileHash(prepared.Manifest)
		if err != nil {
			return "", err
		}
		routes.Scenes = append(routes.Scenes, rawSceneRoute{guid, prepared.Manifest, hash})
	}
	return publishGoContentRoutes(opts.Output, routes)
}

// Route snapshots must remain immutable because a running Player retains both
// their manifests and hashes until its next restart.
func publishGoContentRoutes(output string, routes rawContentRoutes) (string, error) {
	data, err := json.Marshal(routes)
	if err != nil {
		return "", err
	}
	data = append(data, '\n')
	directory := filepath.Join(output, "content-routes")
	if err := os.MkdirAll(directory, 0700); err != nil {
		return "", err
	}
	path := filepath.Join(directory, bytesSHA(data)+".json")
	file, err := os.OpenFile(path, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0600)
	if errors.Is(err, os.ErrExist) {
		previous, readErr := os.ReadFile(path)
		if readErr != nil || !bytes.Equal(previous, data) {
			return "", errors.New("published content routes were modified")
		}
		return path, nil
	}
	if err != nil {
		return "", err
	}
	_, writeErr := file.Write(data)
	closeErr := file.Close()
	if writeErr != nil || closeErr != nil {
		os.Remove(path)
		return "", errors.Join(writeErr, closeErr)
	}
	return path, nil
}

func readVerifiedContentRoutes(path string) (rawContentRoutes, error) {
	var routes rawContentRoutes
	if err := readJSONFile(path, &routes); err != nil {
		return routes, err
	}
	if routes.SchemaVersion != 1 || !filepath.IsAbs(routes.Project) || routes.Scenes == nil {
		return routes, errors.New("invalid content route snapshot")
	}
	seen := map[string]bool{}
	for _, scene := range routes.Scenes {
		if !sceneGUID.MatchString(scene.GUID) || !filepath.IsAbs(scene.Manifest) || !contentRouteSHA.MatchString(scene.SHA256) || seen[scene.GUID] {
			return routes, errors.New("invalid or duplicate content scene route")
		}
		seen[scene.GUID] = true
		actual, err := goFileHash(scene.Manifest)
		if err != nil || actual != scene.SHA256 {
			return routes, fmt.Errorf("published content manifest changed: %s", scene.Manifest)
		}
	}
	return routes, nil
}

// Prepare every content scene after a watched source change. A root scene can
// remain equal while a ship scene, its prefab graph, or native content changes.
func contentRoutesRuntimeEqual(previous, next string) (bool, error) {
	if previous == "" && next == "" {
		return true, nil
	}
	if previous == "" || next == "" {
		return false, nil
	}
	a, err := readVerifiedContentRoutes(previous)
	if err != nil {
		return false, err
	}
	b, err := readVerifiedContentRoutes(next)
	if err != nil {
		return false, err
	}
	if a.Project != b.Project || len(a.Scenes) != len(b.Scenes) {
		return false, nil
	}
	old := make(map[string]rawSceneRoute, len(a.Scenes))
	for _, scene := range a.Scenes {
		old[scene.GUID] = scene
	}
	for _, scene := range b.Scenes {
		before, exists := old[scene.GUID]
		if !exists {
			return false, nil
		}
		equal, err := sceneRuntimeEqual(before.Manifest, scene.Manifest)
		if err != nil || !equal {
			return false, err
		}
	}
	return true, nil
}
