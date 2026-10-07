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
	"unitymicroeditor/internal/editorbridge"
)

// Native bootstrap captures engine facts only. Target SDK assemblies and the
// playable baseline are always produced by the independent source coordinator.
func (s *agentProjectService) prepareEngineBootstrap(ctx context.Context, args map[string]any) (agentPreparation, error) {
	var result agentPreparation
	id, scene, host := agentString(args, "requestId"), agentString(args, "scene"), agentString(args, "hostPlayer")
	if !agentOperationID.MatchString(id) || !strings.HasPrefix(scene, "Assets/") || filepath.ToSlash(filepath.Clean(scene)) != scene || filepath.Ext(scene) != ".unity" {
		return result, agentFailure("invalid_request", "engine bootstrap requires a stable requestId and explicit Assets/...unity scene")
	}
	directory := filepath.Join(s.project, "Library/UnityMicroEditor/preparations", id)
	request := map[string]any{"schemaVersion": 2, "backend": "native_engine_bootstrap", "project": s.project, "scene": scene,
		"hostPlayer": host, "editor": s.editor, "nativeCatalog": agentString(args, "nativeCatalog")}
	if err := agentImmutableJSON(filepath.Join(directory, "request.json"), request); err != nil {
		return result, err
	}
	resultPath := filepath.Join(directory, "result.json")
	if err := agentReadJSON(resultPath, 1<<20, &result); err == nil {
		return result, validateAgentEngineBootstrap(ctx, s.project, scene, id, directory, result)
	} else if !errors.Is(err, os.ErrNotExist) {
		return result, err
	}
	client, err := s.editorClient(ctx)
	if err != nil {
		return result, err
	}
	response, callErr := client.Do(ctx, editorbridge.Request{ID: agentPhaseID(id, "initial-refresh"), Action: "refresh", Arguments: json.RawMessage(`{}`)})
	if _, err = agentRequireResponse(response, callErr); err != nil {
		return result, err
	}
	hostEvidence := ""
	if host == "" {
		var evidence map[string]any
		host, evidence, err = prepareAgentHost(ctx, s, id)
		if err != nil {
			return result, err
		}
		hostEvidence = filepath.Join(directory, "host-evidence.json")
		if err := agentImmutableJSON(hostEvidence, evidence); err != nil {
			return result, err
		}
		client, err = s.editorClient(ctx)
		if err != nil {
			return result, err
		}
	}
	response, callErr = client.Do(ctx, editorbridge.Request{ID: agentPhaseID(id, "refresh"), Action: "refresh", Arguments: json.RawMessage(`{}`)})
	if _, err = agentRequireResponse(response, callErr); err != nil {
		return result, err
	}
	response, callErr = client.Do(ctx, editorbridge.Request{ID: agentPhaseID(id, "graph"), Action: "export_graph", Arguments: json.RawMessage(`{}`)})
	graph, err := agentRequireResponse(response, callErr)
	if err != nil {
		return result, err
	}
	var graphValue any
	if err := json.Unmarshal(graph, &graphValue); err != nil {
		return result, err
	}
	graphPath := filepath.Join(directory, "graph.json")
	if err := agentImmutableJSON(graphPath, graphValue); err != nil {
		return result, err
	}
	native := agentString(args, "nativeCatalog")
	if native == "" {
		candidate := filepath.Join(directory, "native-catalog.json")
		if _, err := os.Stat(candidate); err == nil {
			native = candidate
		} else if !errors.Is(err, os.ErrNotExist) {
			return result, err
		} else {
			needed, err := agentMayNeedNative(s.project)
			if err != nil {
				return result, err
			}
			if needed {
				native, err = s.prepareNative(ctx, scene, id, directory)
			} else {
				native, err = agentIdentityCatalog(s.project, graphPath, directory)
			}
			if err != nil {
				return result, err
			}
		}
	}
	files, err := agentEngineBootstrapFiles(ctx, s.project, host, graphPath, native, hostEvidence)
	if err != nil {
		return result, err
	}
	artifacts := filepath.Join(directory, "artifacts.json")
	if err := agentImmutableJSON(artifacts, files); err != nil {
		return result, err
	}
	result = agentPreparation{SchemaVersion: 1, RequestID: id, Project: s.project, Scene: scene, HostPlayer: host, NativeCatalog: native,
		HostEvidence: hostEvidence, ReferenceGraph: graphPath, ArtifactReceipt: artifacts,
		Backends: map[string]string{"preparation": "native_engine_bootstrap", "nativeBootstrap": "unity_editor_public_api_once"}}
	if err := agentImmutableJSON(resultPath, result); err != nil {
		return result, err
	}
	return result, nil
}

func agentEngineBootstrapFiles(ctx context.Context, project, host, graph, catalog, evidence string) (map[string]string, error) {
	result := map[string]string{}
	add := func(path string, suppliedInput bool) error {
		if err := ctx.Err(); err != nil {
			return err
		}
		if !filepath.IsAbs(path) || (!suppliedInput && !goproject.IsOwnedCachePath(project, path)) || agentCanonical(path) != nil {
			return errors.New("engine bootstrap artifact is not canonical or its generated output is outside owned caches")
		}
		if info, err := os.Lstat(path); err != nil || !info.Mode().IsRegular() {
			return errors.New("engine bootstrap artifact is not a regular file: " + path)
		}
		if len(result) >= 8192 {
			return errors.New("engine bootstrap artifact inventory exceeds 8192 files")
		}
		hash, err := goFileHash(path)
		if err == nil {
			result[path] = hash
		}
		return err
	}
	// Explicit host/catalog inputs may live outside the target cache. This
	// inventory only reads them; generated graph and receipt writes stay owned.
	if err := add(host, true); err != nil {
		return nil, err
	}
	if err := filepath.WalkDir(filepath.Dir(host), func(path string, entry fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if entry.Type()&os.ModeSymlink != 0 {
			return errors.New("engine bootstrap host contains a symlink")
		}
		if entry.IsDir() {
			return nil
		}
		return add(path, true)
	}); err != nil {
		return nil, err
	}
	for _, file := range []struct {
		path          string
		suppliedInput bool
	}{{graph, false}, {catalog, true}, {evidence, false}} {
		if file.path != "" {
			if err := add(file.path, file.suppliedInput); err != nil {
				return nil, err
			}
		}
	}
	return result, nil
}

func validateAgentEngineBootstrap(ctx context.Context, project, scene, id, directory string, result agentPreparation) error {
	if result.SchemaVersion != 1 || result.Project != project || result.Scene != scene || result.RequestID != id || result.Baseline != "" ||
		result.ReferenceGraph != filepath.Join(directory, "graph.json") || result.ArtifactReceipt != filepath.Join(directory, "artifacts.json") ||
		result.Backends["preparation"] != "native_engine_bootstrap" || result.NativeCatalog == "" {
		return errors.New("cached native engine bootstrap has an invalid identity")
	}
	var accepted map[string]string
	if err := agentReadJSON(result.ArtifactReceipt, 4<<20, &accepted); err != nil {
		return err
	}
	current, err := agentEngineBootstrapFiles(ctx, project, result.HostPlayer, result.ReferenceGraph, result.NativeCatalog, result.HostEvidence)
	if err != nil {
		return err
	}
	if len(accepted) != len(current) || len(current) < 3 {
		return errors.New("cached native engine bootstrap artifact inventory changed")
	}
	for path, hash := range current {
		if accepted[path] != hash {
			return errors.New("cached native engine bootstrap artifact changed: " + path)
		}
	}
	return nil
}

// Profile capture can finish before defaults warming fails. A retry must keep
// that request's native-bootstrap obligation even when the profile now exists.
func recoverAgentEngineBootstrap(ctx context.Context, project, scene, id, host string) (agentPreparation, error) {
	var result agentPreparation
	directory := filepath.Join(project, "Library/UnityMicroEditor/preparations", id)
	if err := agentReadJSON(filepath.Join(directory, "result.json"), 1<<20, &result); errors.Is(err, os.ErrNotExist) {
		return agentPreparation{}, nil
	} else if err != nil {
		return result, err
	}
	if result.HostPlayer != host {
		return result, errors.New("cached native bootstrap host differs from this source preparation")
	}
	return result, validateAgentEngineBootstrap(ctx, project, scene, id, directory, result)
}

// Unity's launch executable is shared by different builds. Include the exact
// host location and compiled-source proof rather than using that executable's
// digest as the identity of every host in this project.
func agentSourceEngineProfileCachePath(project, host string) (string, error) {
	if !filepath.IsAbs(host) || agentCanonical(host) != nil {
		return "", errors.New("engine profile requires a canonical absolute host")
	}
	hostData, err := readAgentFile(host, 32<<20)
	if err != nil {
		return "", err
	}
	proofPath := filepath.Join(filepath.Dir(goSourceManagedDirectory(host)), "UnityMicroEditorSourceProfile.json")
	proofData, err := readAgentFile(proofPath, 32<<20)
	if err != nil {
		return "", errors.Join(errors.New("host lacks its compiled-source profile proof"), err)
	}
	identity, err := json.Marshal(map[string]string{"host": host, "executableSHA256": bytesSHA(hostData), "sourceProofSHA256": bytesSHA(proofData)})
	if err != nil {
		return "", err
	}
	return filepath.Join(project, "Library/UnityMicroEditor/engine-profiles", bytesSHA(identity), "profile.json"), nil
}
