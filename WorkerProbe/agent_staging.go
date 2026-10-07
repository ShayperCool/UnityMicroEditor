package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"unitymicroeditor/project/authoring"
)

type agentSourceSnapshot struct {
	Project string                       `json:"project"`
	Inputs  []authoring.PublicationInput `json:"inputs"`
	Files   map[string]string            `json:"files"`
}

func agentPhaseID(id, phase string) string { return bytesSHA([]byte(id))[:32] + "-" + phase }

func snapshotAgentSource(ctx context.Context, project string) (agentSourceSnapshot, error) {
	result := agentSourceSnapshot{Project: project, Files: map[string]string{}}
	store, err := authoring.NewStore(project)
	if err != nil {
		return result, err
	}
	var sources []string
	for _, folder := range []string{"Assets", "Packages", "ProjectSettings"} {
		err := filepath.WalkDir(filepath.Join(project, folder), func(path string, entry fs.DirEntry, walkErr error) error {
			if walkErr != nil {
				return walkErr
			}
			if err := ctx.Err(); err != nil {
				return err
			}
			if entry.Type()&os.ModeSymlink != 0 {
				return errors.New("staging refuses authored source symlinks: " + path)
			}
			if entry.IsDir() {
				return nil
			}
			info, err := entry.Info()
			if err != nil {
				return err
			}
			if !info.Mode().IsRegular() {
				return errors.New("staging source is not a regular file: " + path)
			}
			logical, err := filepath.Rel(project, path)
			if err != nil {
				return err
			}
			logical = filepath.ToSlash(logical)
			hash, err := goFileHash(path)
			if err != nil {
				return err
			}
			result.Files[logical] = hash
			if !strings.HasSuffix(logical, ".meta") {
				sources = append(sources, logical)
			}
			return nil
		})
		if err != nil {
			return result, err
		}
	}
	if len(sources) > 4096 {
		return result, agentFailure("project_too_large", "staged publication currently supports at most 4096 source dependencies")
	}
	sort.Strings(sources)
	for _, path := range sources {
		revision, err := store.DependencyRevision(ctx, path)
		if err != nil {
			return result, err
		}
		result.Inputs = append(result.Inputs, authoring.PublicationInput{Path: path, ExpectedRevision: revision})
	}
	return result, nil
}

func verifyAgentSnapshot(ctx context.Context, snapshot agentSourceSnapshot) error {
	current, err := snapshotAgentSource(ctx, snapshot.Project)
	if err != nil {
		return err
	}
	encoded, _ := json.Marshal(snapshot.Files)
	latest, _ := json.Marshal(current.Files)
	if !bytes.Equal(encoded, latest) {
		return agentFailure("revision_conflict", "authored source inventory changed while Editor prepared staged content")
	}
	return nil
}

func normalizeStagedPackagePaths(source, stage string) error {
	path := filepath.Join(stage, "Packages/manifest.json")
	data, err := readAgentFile(path, 1<<20)
	if err != nil {
		return err
	}
	var manifest map[string]any
	if err = json.Unmarshal(data, &manifest); err != nil {
		return err
	}
	dependencies, ok := manifest["dependencies"].(map[string]any)
	if !ok {
		return errors.New("staged package manifest lacks dependencies")
	}
	changed := false
	for name, value := range dependencies {
		location, ok := value.(string)
		if !ok || !strings.HasPrefix(location, "file:") {
			continue
		}
		physical := strings.TrimPrefix(location, "file:")
		if filepath.IsAbs(physical) {
			continue
		}
		canonical, err := filepath.EvalSymlinks(filepath.Join(source, "Packages", physical))
		if err != nil {
			return err
		}
		dependencies[name] = "file:" + filepath.ToSlash(canonical)
		changed = true
	}
	if !changed {
		return nil
	}
	encoded, err := json.MarshalIndent(manifest, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(path, append(encoded, '\n'), 0600)
}

func cloneAgentStaging(ctx context.Context, source, directory string) (string, agentSourceSnapshot, error) {
	stage := filepath.Join(directory, "project")
	if err := agentCanonical(stage); err != nil {
		return "", agentSourceSnapshot{}, err
	}
	var snapshot agentSourceSnapshot
	marker := filepath.Join(directory, "clone-complete.json")
	if err := agentReadJSON(marker, 8<<20, &snapshot); err == nil {
		if info, err := os.Lstat(stage); err != nil || !info.IsDir() {
			return "", snapshot, errors.New("completed staging project is missing or not a regular directory")
		}
		if snapshot.Project != source {
			return "", snapshot, errors.New("staging cache belongs to another project")
		}
		return stage, snapshot, nil
	} else if !errors.Is(err, os.ErrNotExist) {
		return "", snapshot, err
	}
	if _, err := os.Lstat(stage); !errors.Is(err, os.ErrNotExist) {
		return "", snapshot, agentFailure("staging_incomplete", "staging directory exists without a completed snapshot; inspect it and use a new operationId")
	}
	var err error
	snapshot, err = snapshotAgentSource(ctx, source)
	if err != nil {
		return "", snapshot, err
	}
	if err = os.MkdirAll(stage, 0700); err != nil {
		return "", snapshot, err
	}
	if _, err = goCompilerCommand(ctx, "", "cp", "-a", "--reflink=auto", "--", filepath.Join(source, "Assets"), filepath.Join(source, "Packages"), filepath.Join(source, "ProjectSettings"), stage); err != nil {
		return "", snapshot, err
	}
	if err = normalizeStagedPackagePaths(source, stage); err != nil {
		return "", snapshot, err
	}
	if err = installAgentPackage(stage); err != nil {
		return "", snapshot, err
	}
	// A copied staging-derived target can carry a prior tool marker. Replace
	// only the marker inside this newly owned clone; retain the source bytes.
	stagingMarker := filepath.Join(stage, "ProjectSettings/UnityMicroEditorStaging.json")
	if _, copied := snapshot.Files["ProjectSettings/UnityMicroEditorStaging.json"]; copied {
		if err = agentCanonical(stagingMarker); err != nil {
			return "", snapshot, err
		}
		if err = os.Remove(stagingMarker); err != nil {
			return "", snapshot, err
		}
	}
	if err = agentImmutableJSON(filepath.Join(stage, "ProjectSettings/UnityMicroEditorStaging.json"), map[string]any{"schemaVersion": 1, "sourceProject": source}); err != nil {
		return "", snapshot, err
	}
	if err = verifyAgentSnapshot(ctx, snapshot); err != nil {
		return "", snapshot, err
	}
	if err = agentImmutableJSON(marker, snapshot); err != nil {
		return "", snapshot, err
	}
	return stage, snapshot, nil
}

func (s *agentProjectService) prefabEdit(ctx context.Context, args map[string]any) (any, error) {
	id := agentString(args, "operationId")
	if !agentOperationID.MatchString(id) {
		return nil, agentFailure("invalid_request", "source prefab authoring requires operationId of at most 64 ASCII letters/digits/hyphens")
	}
	encoded, err := json.Marshal(args)
	if err != nil {
		return nil, err
	}
	decoder := json.NewDecoder(bytes.NewReader(encoded))
	decoder.UseNumber()
	var request authoring.PrefabEditRequest
	if err := decoder.Decode(&request); err != nil {
		return nil, agentFailure("invalid_request", "invalid source prefab arguments: "+err.Error())
	}
	var profileSHA string
	if request.Action == "add_component" {
		name, err := componentDefaultsName(request.ComponentType)
		if err != nil {
			return nil, err
		}
		profile, hash, err := componentDefaultsProfile(ctx, s.project, s.editor)
		if err != nil {
			return nil, err
		}
		catalog, err := readAgentDefaultsCatalog(s.project, profile, hash)
		if err != nil {
			return nil, err
		}
		template := catalog.Templates[name]
		request.ComponentType, request.DefaultsTemplate, profileSHA = name, &template, hash
		request.BeforePublish = s.componentDefaultsGuard(ctx, hash)
	} else if request.DefaultsTemplate != nil {
		return nil, agentFailure("invalid_request", "defaultsTemplate is supplied by the versioned component cache, not the caller")
	}
	store, err := authoring.NewStore(s.project)
	if err != nil {
		return nil, err
	}
	// Previously accepted Editor-backed plans remain replayable through the
	// same source journal. This migration path never creates a legacy plan or
	// starts an Editor; it requires the existing immutable request receipt.
	directory := filepath.Join(s.project, "Library/UnityMicroEditor/authoring-stages", bytesSHA([]byte(id)))
	var legacy authoring.StagedPublicationRequest
	if err := agentReadJSON(filepath.Join(directory, "publication.json"), 80<<20, &legacy); err == nil {
		if legacy.OperationID != id {
			return nil, agentFailure("invalid_plan", "legacy publication belongs to another operation")
		}
		requestPath := filepath.Join(directory, "request.json")
		if _, err := os.Lstat(requestPath); err != nil {
			return nil, agentFailure("invalid_plan", "legacy publication has no immutable request receipt")
		}
		if err := agentImmutableJSON(requestPath, map[string]any{"schemaVersion": 1, "project": s.project, "arguments": args}); err != nil {
			return nil, err
		}
		result, err := store.PublishStaged(ctx, legacy)
		return map[string]any{"backend": "raw_source", "planOrigin": "unity_editor_staged_legacy", "operationId": id, "publication": result, "editorStarted": false}, err
	} else if !errors.Is(err, os.ErrNotExist) {
		return nil, err
	}
	result, err := store.PrefabEdit(ctx, request)
	response := map[string]any{"backend": result.Backend, "operationId": result.OperationID, "action": result.Action,
		"publication": result.Publication, "createdIds": result.CreatedIDs, "generatedPath": result.GeneratedPath, "editorStarted": false}
	if profileSHA != "" {
		response["defaultsBackend"], response["profileSha256"] = "versioned_component_cache", profileSHA
	}
	return response, err
}

func validateAgentStagedMetadata(snapshot agentSourceSnapshot, stage string, newAssets map[string]bool) error {
	for logical, expected := range snapshot.Files {
		if !strings.HasPrefix(logical, "Assets/") || !strings.HasSuffix(logical, ".meta") {
			continue
		}
		path := filepath.Join(stage, filepath.FromSlash(logical))
		if err := agentCanonical(path); err != nil {
			return err
		}
		actual, err := goFileHash(path)
		if err != nil || actual != expected {
			return agentFailure("invalid_identity", "Editor changed or removed existing authored metadata: "+logical)
		}
	}
	return filepath.WalkDir(filepath.Join(stage, "Assets"), func(path string, entry fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if entry.IsDir() || !strings.HasSuffix(path, ".meta") {
			return nil
		}
		logical, err := filepath.Rel(stage, path)
		if err != nil {
			return err
		}
		logical = filepath.ToSlash(logical)
		if _, exists := snapshot.Files[logical]; exists {
			return nil
		}
		if !newAssets[strings.TrimSuffix(logical, ".meta")] {
			return agentFailure("invalid_identity", "Editor generated orphan or unplanned metadata: "+logical)
		}
		return nil
	})
}
