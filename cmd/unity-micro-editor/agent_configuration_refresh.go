package main

import (
	"context"
	"encoding/json"
	"errors"
	"io/fs"
	"os"
	"path/filepath"
	"strings"

	goproject "unitymicroeditor/internal/project"
	"unitymicroeditor/internal/agentcontrol"
)

func agentRequiresNewGraph(message string) bool {
	for _, text := range []string{"regenerate source graph", "fresh managed graph", "outside the prepared managed graph"} {
		if strings.Contains(message, text) {
			return true
		}
	}
	return false
}

type agentConfigurationSelection struct {
	Scene, Baseline string
}

func agentManagedConfigurationPath(path string) bool {
	path = strings.ToLower(path)
	if filepath.Ext(path) == ".meta" {
		path = strings.TrimSuffix(path, ".meta")
	}
	switch filepath.Ext(path) {
	case ".cs", ".asmdef", ".asmref", ".rsp", ".dll":
		return true
	}
	return filepath.Base(path) == "package.json"
}

// Only declared public inputs participate: authored roots, the selected scene,
// and registered package sources from the active preparation's verified graph.
// Missing declared inputs have an identity too, so repairing a failed preparation
// cannot remain stuck on a prior durable job after a source is added or removed.
func agentConfigurationRevision(project string, selections ...agentConfigurationSelection) (string, error) {
	hashes := map[string]string{}
	fileHashes := map[string]string{}
	trackFile := func(path, key string, optional bool) error {
		if hash := fileHashes[path]; hash != "" {
			hashes[key] = hash
			return nil
		}
		if err := agentCanonical(path); err != nil {
			return err
		}
		if _, err := os.Lstat(path); errors.Is(err, os.ErrNotExist) && optional {
			hashes[key] = "absent"
			fileHashes[path] = "absent"
			return nil
		} else if err != nil {
			return err
		}
		hash, err := goFileHash(path)
		if err != nil {
			return err
		}
		hashes[key] = hash
		fileHashes[path] = hash
		return nil
	}
	for _, folder := range []string{"Assets", "Packages", "ProjectSettings"} {
		if err := agentCanonical(filepath.Join(project, folder)); err != nil {
			return "", err
		}
		if err := filepath.WalkDir(filepath.Join(project, folder), func(path string, entry fs.DirEntry, err error) error {
			if err != nil {
				return err
			}
			if entry.IsDir() {
				return nil
			}
			if entry.Type()&os.ModeSymlink != 0 {
				return errors.New("configuration inventory contains a symlink: " + path)
			}
			tracked := folder == "ProjectSettings" || folder == "Packages" || agentManagedConfigurationPath(path)
			if !tracked {
				return nil
			}
			logical, err := filepath.Rel(project, path)
			if err != nil {
				return err
			}
			return trackFile(path, filepath.ToSlash(logical), false)
		}); err != nil {
			return "", err
		}
	}
	for _, selection := range selections {
		if selection.Scene != "" {
			if !strings.HasPrefix(selection.Scene, "Assets/") || filepath.ToSlash(filepath.Clean(selection.Scene)) != selection.Scene || filepath.Ext(selection.Scene) != ".unity" {
				return "", errors.New("configuration recovery requires an explicit authored Assets scene")
			}
			path := filepath.Join(project, filepath.FromSlash(selection.Scene))
			if err := trackFile(path, selection.Scene, false); err != nil {
				return "", err
			}
			if err := trackFile(path+".meta", selection.Scene+".meta", true); err != nil {
				return "", err
			}
		}
		if selection.Baseline == "" {
			continue
		}
		if !filepath.IsAbs(selection.Baseline) || !goproject.IsOwnedCachePath(project, selection.Baseline) {
			return "", errors.New("configuration recovery baseline is outside owned project caches")
		}
		var baseline struct {
			Project  string            `json:"project"`
			Compiler goCompileManifest `json:"compiler"`
		}
		if err := agentReadJSON(selection.Baseline, 32<<20, &baseline); err != nil {
			return "", err
		}
		if baseline.Project != project || baseline.Compiler.Project != "" && baseline.Compiler.Project != project {
			return "", errors.New("configuration recovery baseline belongs to another project")
		}
		inputs := baseline.Compiler.CompilerInputs
		var graph goCompilerGraphExport
		graphPath := filepath.Join(filepath.Dir(selection.Baseline), "graph.json")
		if err := agentReadJSON(graphPath, 32<<20, &graph); err == nil {
			if graph.SchemaVersion != 1 || graph.Project != project || len(graph.SourceRoots) > 4096 {
				return "", errors.New("configuration recovery public graph identity is invalid")
			}
			for path := range graph.CompilerInputs {
				if inputs == nil {
					inputs = map[string]string{}
				}
				inputs[path] = graph.CompilerInputs[path]
			}
		} else if !errors.Is(err, os.ErrNotExist) {
			return "", err
		}
		registered := map[string]string{}
		for _, root := range graph.SourceRoots {
			parts := strings.Split(root.Path, "/")
			if len(parts) != 2 || parts[0] != "Packages" || parts[1] == "" || parts[1] == "." || parts[1] == ".." || filepath.ToSlash(filepath.Clean(root.Path)) != root.Path || !filepath.IsAbs(root.Source) || filepath.Clean(root.Source) != root.Source || root.Source == string(filepath.Separator) {
				return "", errors.New("invalid registered public package root")
			}
			if previous := registered[root.Path]; previous != "" {
				if previous != root.Source {
					return "", errors.New("conflicting registered public package roots")
				}
				continue
			}
			if withinDirectory(root.Source, filepath.Join(project, "Library")) && !withinDirectory(root.Source, filepath.Join(project, "Library/PackageCache")) || withinDirectory(root.Source, filepath.Join(project, "Temp")) {
				return "", errors.New("registered package source crosses a private project cache")
			}
			if err := agentCanonical(root.Source); err != nil {
				return "", err
			}
			registered[root.Path] = root.Source
			hashes["source-root:"+root.Path] = root.Source
			if info, err := os.Lstat(root.Source); errors.Is(err, os.ErrNotExist) {
				hashes["source-root-state:"+root.Path] = "absent"
				continue
			} else if err != nil {
				return "", err
			} else if !info.IsDir() {
				return "", errors.New("registered public package source is not a directory")
			}
			if err := filepath.WalkDir(root.Source, func(path string, entry fs.DirEntry, err error) error {
				if err != nil {
					return err
				}
				if entry.Type()&os.ModeSymlink != 0 {
					return errors.New("registered package configuration contains a symlink: " + path)
				}
				if entry.IsDir() {
					if path != root.Source {
						switch entry.Name() {
						case ".git", ".svn", "Library", "Temp", "obj":
							return filepath.SkipDir
						}
					}
					return nil
				}
				if !agentManagedConfigurationPath(path) {
					return nil
				}
				logical, err := filepath.Rel(root.Source, path)
				if err != nil {
					return err
				}
				return trackFile(path, "package:"+root.Path+"/"+filepath.ToSlash(logical), false)
			}); err != nil {
				return "", err
			}
		}
		for path := range inputs {
			if !agentManagedConfigurationPath(path) {
				continue
			}
			if !filepath.IsAbs(path) || filepath.Clean(path) != path {
				return "", errors.New("public compiler input is not absolute and normalized")
			}
			// Generated assemblies and compiler caches are outputs. Registered
			// PackageCache source files remain public package inputs.
			if withinDirectory(path, filepath.Join(project, "Library")) || withinDirectory(path, filepath.Join(project, "Temp")) {
				allowed := false
				for _, source := range registered {
					if withinDirectory(path, source) {
						allowed = true
						break
					}
				}
				if !allowed {
					continue
				}
			}
			if err := trackFile(path, "compiler-input:"+path, true); err != nil {
				return "", err
			}
		}
	}
	data, err := json.Marshal(hashes)
	if err != nil {
		return "", err
	}
	return bytesSHA(data), nil
}

// A watch rejection keeps the old Player. A configuration-specific rejection
// queues a full public graph/host preparation; activation happens only after it
// has compiled and validated. Ordinary C# compile errors retain existing behavior.
func (s *agentProjectService) recoverConfiguration(ctx context.Context, name string, raw json.RawMessage, result any, originalErr error) (any, error) {
	if name != "session_control" {
		return result, originalErr
	}
	var input struct {
		Action string `json:"action"`
	}
	if json.Unmarshal(raw, &input) != nil || input.Action != "wait_update" {
		return result, originalErr
	}
	state, ok := result.(agentcontrol.State)
	if !ok || state.Phase != "ready" || !agentRequiresNewGraph(state.UpdateError) || errors.Is(originalErr, context.Canceled) || errors.Is(originalErr, context.DeadlineExceeded) {
		return result, originalErr
	}
	s.stateMu.RLock()
	scene, baseline := s.scene, s.baseline
	s.stateMu.RUnlock()
	if scene == "" {
		return result, originalErr
	}
	revision, err := agentConfigurationRevision(s.project, agentConfigurationSelection{Scene: scene, Baseline: baseline})
	if err != nil {
		return result, err
	}
	identity := "config-" + bytesSHA([]byte(scene + "\n" + revision))[:40]
	arguments, _ := json.Marshal(map[string]any{"scene": scene, "requestId": identity, "activate": true})
	preparation, err := s.Handle(ctx, "project_prepare", arguments)
	return map[string]any{"status": "result_pending", "phase": "preparing_configuration", "currentPlayer": state, "preparation": preparation, "configurationRevision": revision}, err
}
