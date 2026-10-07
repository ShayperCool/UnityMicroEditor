package main

import (
	"context"
	"encoding/json"
	"errors"
	"io/fs"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"time"

	"unitymicroeditor/internal/editorbridge"
	goproject "unitymicroeditor/internal/project"
)

type agentHostPreparation struct {
	SchemaVersion int               `json:"schemaVersion"`
	Project       string            `json:"project"`
	Editor        string            `json:"editor"`
	RequestID     string            `json:"requestId"`
	Stage         string            `json:"stageProject"`
	Player        string            `json:"player"`
	SnapshotSHA   string            `json:"sourceSnapshotSha256"`
	Artifacts     map[string]string `json:"artifacts"`
	Evidence      map[string]any    `json:"evidence"`
}

// Automatic host construction runs only in an owned clone. Unity build
// preprocessors can author files (for example Addressables link.xml); none of
// those changes are published to the target. The caller holds the project
// action lock and reacquires its target Editor client after this call.
func prepareAgentHost(ctx context.Context, service *agentProjectService, id string) (host string, evidence map[string]any, err error) {
	if service == nil || !agentOperationID.MatchString(id) || !filepath.IsAbs(service.project) || !filepath.IsAbs(service.editor) {
		return "", nil, agentFailure("invalid_request", "host preparation requires an explicit project, Editor and stable requestId")
	}
	directory := filepath.Join(service.project, "Library/UnityMicroEditor/preparations", id, "host-stage")
	stage := filepath.Join(directory, "project")
	if !goproject.IsOwnedCachePath(service.project, stage) {
		return "", nil, errors.New("host stage is outside owned project caches")
	}
	if err = agentCanonical(stage); err != nil {
		return "", nil, err
	}
	if err = agentImmutableJSON(filepath.Join(directory, "request.json"), map[string]any{
		"schemaVersion": 1, "project": service.project, "editor": service.editor, "requestId": id,
		"stageProject": stage, "action": "build_host", "unityVersion": fixtureUnityVersion, "development": true,
	}); err != nil {
		return "", nil, err
	}

	snapshotPath := filepath.Join(directory, "source-snapshot.json")
	var snapshot agentSourceSnapshot
	if err = agentReadJSON(snapshotPath, 8<<20, &snapshot); errors.Is(err, os.ErrNotExist) {
		snapshot, err = snapshotAgentSource(ctx, service.project)
		if err == nil {
			err = agentImmutableJSON(snapshotPath, snapshot)
		}
	}
	if err != nil {
		return "", nil, err
	}
	if snapshot.Project != service.project || len(snapshot.Files) == 0 {
		return "", nil, errors.New("host source snapshot belongs to another project or is empty")
	}
	if err = verifyAgentSnapshot(ctx, snapshot); err != nil {
		return "", nil, err
	}
	// Source preservation is checked on failures too. A failed or cancelled
	// build must not turn a changed target into a reusable successful host.
	defer func() {
		check, cancel := context.WithTimeout(context.Background(), 2*time.Minute)
		defer cancel()
		if sourceErr := verifyAgentSnapshot(check, snapshot); sourceErr != nil {
			host, evidence, err = "", nil, errors.Join(err, sourceErr)
		}
	}()
	snapshotSHA, err := goFileHash(snapshotPath)
	if err != nil {
		return "", nil, err
	}
	resultPath := filepath.Join(directory, "result.json")
	var cached agentHostPreparation
	if err = agentReadJSON(resultPath, 8<<20, &cached); err == nil {
		if cached.SchemaVersion != 1 || cached.Project != service.project || cached.Editor != service.editor || cached.RequestID != id ||
			cached.Stage != stage || cached.SnapshotSHA != snapshotSHA || cached.Evidence["backend"] != "unity_editor" || cached.Evidence["sourceSnapshotVerified"] != true {
			return "", nil, errors.New("cached host result lacks exact source/stage/Editor provenance")
		}
		artifacts, proofErr := snapshotAgentHostArtifacts(ctx, service.project, stage, agentPhaseID(id, "host"), cached.Player)
		if proofErr != nil {
			return "", nil, proofErr
		}
		if !reflect.DeepEqual(artifacts, cached.Artifacts) {
			return "", nil, errors.New("cached staged host artifacts differ from their confirmed result")
		}
		return cached.Player, cached.Evidence, nil
	} else if !errors.Is(err, os.ErrNotExist) {
		return "", nil, err
	}

	warm := false
	if service.client != nil {
		connection, attachErr := service.client.OwnProcessOrAttach(ctx)
		if attachErr != nil {
			return "", nil, attachErr
		}
		if connection.Owned {
			if err = service.client.Close(); err != nil {
				return "", nil, err
			}
			service.client = nil
			warm = true
			if err = verifyAgentSnapshot(ctx, snapshot); err != nil {
				return "", nil, err
			}
		}
	}
	cloned, cloneSnapshot, err := cloneAgentStaging(ctx, service.project, directory)
	if err != nil {
		return "", nil, err
	}
	if cloned != stage || !reflect.DeepEqual(snapshot.Files, cloneSnapshot.Files) {
		return "", nil, errors.New("host clone no longer matches its immutable authored source snapshot")
	}
	if warm {
		if err = seedAgentNativeLibrary(ctx, service.project, stage); err != nil {
			return "", nil, err
		}
	}
	lifetime := service.lifetime
	if lifetime == nil {
		lifetime = ctx
	}
	client, err := editorbridge.New(lifetime, editorbridge.Options{Project: stage, Editor: service.editor, StartupTimeout: 10 * time.Minute})
	if err != nil {
		return "", nil, err
	}
	defer client.Close()
	response, callErr := client.Do(ctx, editorbridge.Request{ID: agentPhaseID(id, "host-refresh"), Action: "refresh", Arguments: json.RawMessage(`{}`)})
	if _, err = agentRequireResponse(response, callErr); err != nil {
		return "", nil, err
	}
	response, callErr = client.Do(ctx, editorbridge.Request{ID: agentPhaseID(id, "host"), Action: "build_host", Arguments: json.RawMessage(`{}`)})
	data, err := agentRequireResponse(response, callErr)
	if err != nil {
		return "", nil, err
	}
	if response.Project != stage || response.Backend != "unity_editor" || response.UnityVersion != fixtureUnityVersion || response.Editor != service.editor {
		return "", nil, errors.New("host response does not belong to the pinned owned stage/Editor backend")
	}
	var build map[string]any
	if err = json.Unmarshal(data, &build); err != nil {
		return "", nil, err
	}
	if build["backend"] != "unity_editor" || build["buildResult"] != "Succeeded" || build["purpose"] != "raw_host" || build["target"] != "StandaloneLinux64" ||
		build["scriptingBackend"] != "Mono2x" || build["hostDevelopment"] != true || build["development"] != true || build["graphicsApi"] != "OpenGLCore" {
		return "", nil, errors.New("host build lacks its successful pinned native Mono/development profile")
	}
	host = agentString(build, "outputPath")
	artifacts, err := snapshotAgentHostArtifacts(ctx, service.project, stage, agentPhaseID(id, "host"), host)
	if err != nil {
		return "", nil, err
	}
	stagePackageHash, err := goFileHash(filepath.Join(stage, "Packages/manifest.json"))
	if err != nil || build["packageProfile"] != stagePackageHash {
		return "", nil, errors.New("host build package profile differs from its staging manifest")
	}
	if err = client.Close(); err != nil {
		return "", nil, err
	}
	finalArtifacts, err := snapshotAgentHostArtifacts(ctx, service.project, stage, agentPhaseID(id, "host"), host)
	if err != nil {
		return "", nil, err
	}
	if !reflect.DeepEqual(artifacts, finalArtifacts) {
		return "", nil, errors.New("staged host artifacts changed while its owned Editor closed")
	}
	if finalProfile, profileErr := goFileHash(filepath.Join(stage, "Packages/manifest.json")); profileErr != nil || finalProfile != stagePackageHash {
		return "", nil, errors.New("staged host package profile changed while its owned Editor closed")
	}
	if err = verifyAgentSnapshot(ctx, snapshot); err != nil {
		return "", nil, err
	}
	evidence = map[string]any{"backend": "unity_editor", "purpose": "isolated_raw_host", "sourceProject": service.project, "stageProject": stage,
		"receipt": response.Receipt, "requestId": response.RequestID, "sourceSnapshot": snapshotPath, "sourceSnapshotSha256": snapshotSHA,
		"sourceSnapshotVerified": true, "sourcePackageProfile": snapshot.Files["Packages/manifest.json"], "stagePackageProfile": stagePackageHash,
		"warmLibraryFromOwnedStoppedEditor": warm, "build": build}
	record := agentHostPreparation{SchemaVersion: 1, Project: service.project, Editor: service.editor, RequestID: id, Stage: stage, Player: host,
		SnapshotSHA: snapshotSHA, Artifacts: artifacts, Evidence: evidence}
	if err = agentImmutableJSON(resultPath, record); err != nil {
		return "", nil, err
	}
	return host, evidence, nil
}

func snapshotAgentHostArtifacts(ctx context.Context, project, stage, phase, player string) (map[string]string, error) {
	directory := filepath.Join(stage, "Builds/UnityMicroEditor", phase)
	if player != filepath.Join(directory, "Project.x86_64") || !goproject.IsOwnedCachePath(project, player) {
		return nil, errors.New("host Player is outside its exact owned stage build output")
	}
	if err := agentCanonical(player); err != nil {
		return nil, err
	}
	if info, err := os.Lstat(player); err != nil || !info.Mode().IsRegular() || info.Size() == 0 || info.Mode().Perm()&0111 == 0 {
		return nil, errors.New("staged host Player must be a nonempty executable regular file")
	}
	result := map[string]string{}
	err := filepath.WalkDir(directory, func(path string, entry fs.DirEntry, walkErr error) error {
		if walkErr != nil {
			return walkErr
		}
		if err := ctx.Err(); err != nil {
			return err
		}
		if entry.Type()&os.ModeSymlink != 0 {
			return errors.New("staged host artifacts contain a symlink")
		}
		if entry.IsDir() {
			return nil
		}
		if len(result) >= 65536 {
			return errors.New("staged host exceeds its artifact inventory bound")
		}
		if info, err := entry.Info(); err != nil || !info.Mode().IsRegular() {
			return errors.New("staged host artifact is not a regular file")
		}
		logical, err := filepath.Rel(directory, path)
		if err != nil || strings.HasPrefix(logical, "..") {
			return errors.New("staged host artifact escaped its owned output")
		}
		hash, err := goFileHash(path)
		if err != nil {
			return err
		}
		result[filepath.ToSlash(logical)] = hash
		return nil
	})
	if err != nil {
		return nil, err
	}
	if result["Project_Data/Managed/UnityMicroEditor.dll"] == "" || result["Project_Data/ScriptingAssemblies.json"] == "" {
		return nil, errors.New("staged host lacks its native runtime bridge/Mono registrations")
	}
	var slots struct {
		Names []string `json:"names"`
	}
	if err := agentReadJSON(filepath.Join(directory, "Project_Data/ScriptingAssemblies.json"), 2<<20, &slots); err != nil {
		return nil, err
	}
	for _, name := range slots.Names {
		if name == "UnityMicroEditor.dll" {
			return result, nil
		}
	}
	return nil, errors.New("staged host has no registered native runtime bridge slot")
}
