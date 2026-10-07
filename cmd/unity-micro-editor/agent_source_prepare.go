package main

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"

	goproject "unitymicroeditor/internal/project"
	unitypackages "unitymicroeditor/internal/project/packages"
)

type agentSourcePreparationSeed struct {
	SchemaVersion  int    `json:"schemaVersion"`
	Scene          string `json:"scene"`
	HostPlayer     string `json:"hostPlayer"`
	NativeCatalog  string `json:"nativeCatalog"`
	EngineProfile  string `json:"engineProfile"`
	ReferenceGraph string `json:"referenceGraph,omitempty"`
}

// Bind caller options separately from resolved defaults. Activating another
// preparation must not change the meaning of a retry with the same request ID.
func (s *agentProjectService) sourcePreparationSeed(args map[string]any, directory string) (agentSourcePreparationSeed, error) {
	var frozen agentSourcePreparationSeed
	request := map[string]any{"schemaVersion": 2, "backend": "go_authored_source", "project": s.project, "editor": s.editor,
		"scene": agentString(args, "scene"), "hostPlayer": agentString(args, "hostPlayer"), "nativeCatalog": agentString(args, "nativeCatalog"),
		"engineProfile": agentString(args, "engineProfile"), "configuredHostPlayer": s.host, "configuredEngineProfile": s.engineProfilePath,
		"mainEditorAllowed": !s.noEditor}
	if err := agentImmutableJSON(filepath.Join(directory, "request.json"), request); err != nil {
		return frozen, err
	}
	seedPath := filepath.Join(directory, "seed.json")
	if err := agentReadJSON(seedPath, 1<<20, &frozen); err == nil {
		if frozen.SchemaVersion != 1 {
			return frozen, agentFailure("invalid_preparation_seed", "source preparation seed schema is invalid")
		}
		return frozen, nil
	} else if !errors.Is(err, os.ErrNotExist) {
		return frozen, err
	}
	previous := s.acceptedPreparationSeed()
	frozen = agentSourcePreparationSeed{SchemaVersion: 1, Scene: agentString(args, "scene"), HostPlayer: agentString(args, "hostPlayer"),
		NativeCatalog: agentString(args, "nativeCatalog"), EngineProfile: agentString(args, "engineProfile")}
	s.stateMu.RLock()
	if frozen.Scene == "" {
		frozen.Scene = s.scene
	}
	s.stateMu.RUnlock()
	if frozen.HostPlayer == "" {
		frozen.HostPlayer = s.host
	}
	if frozen.HostPlayer == "" {
		frozen.HostPlayer = previous.HostPlayer
	}
	if frozen.NativeCatalog == "" {
		frozen.NativeCatalog = previous.NativeCatalog
	}
	if frozen.EngineProfile == "" {
		frozen.EngineProfile = s.engineProfilePath
	}
	if frozen.EngineProfile == "" {
		frozen.EngineProfile = previous.EngineProfile
	}
	if previous.HostPlayer == frozen.HostPlayer {
		if previous.ReferenceGraph != "" {
			frozen.ReferenceGraph = previous.ReferenceGraph
		} else if previous.Baseline != "" && previous.Backends["preparation"] == "unity_editor_public_api" {
			frozen.ReferenceGraph = filepath.Join(filepath.Dir(previous.Baseline), "graph.json")
		}
	}
	if err := agentImmutableJSON(seedPath, frozen); err != nil {
		return frozen, err
	}
	return frozen, nil
}

func (s *agentProjectService) acceptedPreparationSeed() agentPreparation {
	var result agentPreparation
	s.stateMu.RLock()
	baseline := s.baseline
	s.stateMu.RUnlock()
	if baseline == "" || !goproject.IsOwnedCachePath(s.project, baseline) {
		return result
	}
	_ = agentReadJSON(filepath.Join(filepath.Dir(baseline), "result.json"), 1<<20, &result)
	if result.Project != s.project || result.Baseline != baseline {
		return agentPreparation{}
	}
	return result
}

func (s *agentProjectService) prepare(ctx context.Context, args map[string]any) (agentPreparation, error) {
	return s.prepareSource(ctx, args)
}

func (s *agentProjectService) prepareSource(ctx context.Context, args map[string]any) (agentPreparation, error) {
	var result agentPreparation
	id := agentString(args, "requestId")
	if id == "" {
		id = agentID()
	}
	if !agentOperationID.MatchString(id) {
		return result, agentFailure("invalid_request", "invalid source preparation requestId")
	}
	directory := filepath.Join(s.project, "Library/UnityMicroEditor/preparations", id)
	seed, err := s.sourcePreparationSeed(args, directory)
	if err != nil {
		return result, err
	}
	scene, host, native, profilePath := seed.Scene, seed.HostPlayer, seed.NativeCatalog, seed.EngineProfile
	if !strings.HasPrefix(scene, "Assets/") || filepath.ToSlash(filepath.Clean(scene)) != scene || filepath.Ext(scene) != ".unity" {
		return result, agentFailure("invalid_request", "source preparation requires an explicit saved Assets/...unity scene")
	}
	resultPath := filepath.Join(directory, "result.json")
	if err := agentReadJSON(resultPath, 1<<20, &result); err == nil {
		if err := validateAgentCachedPreparation(s.project, s.editor, scene, id, directory, result); err != nil {
			return result, err
		}
		return result, nil
	} else if !errors.Is(err, os.ErrNotExist) {
		return result, err
	}
	resolved, err := unitypackages.Resolve(ctx, s.project, s.editor, unitypackages.Options{})
	if err != nil {
		return result, err
	}
	// A pinned native engine/host profile is bootstrapped once through the
	// explicit Editor backend. Subsequent target graphs never come from Editor.
	var bootstrap agentPreparation
	if host == "" {
		if s.noEditor {
			return result, agentFailure("native_host_missing", "no matching reusable Mono host was supplied; main Editor execution is disabled")
		}
		bootstrapArgs := map[string]any{"scene": scene, "requestId": agentPhaseID(id, "engine-profile-bootstrap"), "nativeCatalog": native}
		bootstrap, err = s.prepareEngineBootstrap(ctx, bootstrapArgs)
		if err != nil {
			return result, err
		}
		host, native = bootstrap.HostPlayer, bootstrap.NativeCatalog
		resolved, err = unitypackages.Resolve(ctx, s.project, s.editor, unitypackages.Options{})
		if err != nil {
			return result, err
		}
	}
	if profilePath == "" {
		profilePath, err = agentSourceEngineProfileCachePath(s.project, host)
		if err != nil {
			return result, err
		}
	}
	profile, err := loadGoSourceEngineProfile(ctx, profilePath, s.editor, host)
	if errors.Is(err, os.ErrNotExist) {
		reference := ""
		if bootstrap.ReferenceGraph != "" {
			reference = bootstrap.ReferenceGraph
		} else if seed.ReferenceGraph != "" {
			reference = seed.ReferenceGraph
		}
		if reference == "" {
			if s.noEditor {
				return result, agentFailure("engine_profile_missing", "capture the pinned engine/native host facts once before source-only preparation")
			}
			bootstrap, err = s.prepareEngineBootstrap(ctx, map[string]any{"scene": scene, "hostPlayer": host, "nativeCatalog": native,
				"requestId": agentPhaseID(id, "engine-profile-bootstrap")})
			if err != nil {
				return result, err
			}
			reference, native = bootstrap.ReferenceGraph, bootstrap.NativeCatalog
			resolved, err = unitypackages.Resolve(ctx, s.project, s.editor, unitypackages.Options{})
			if err != nil {
				return result, err
			}
		}
		profile, err = captureGoSourceEngineProfile(ctx, s.project, s.editor, host, reference, profilePath, resolved)
	}
	if err != nil {
		return result, err
	}
	if bootstrap.ReferenceGraph == "" {
		bootstrap, err = recoverAgentEngineBootstrap(ctx, s.project, scene, agentPhaseID(id, "engine-profile-bootstrap"), host)
		if err != nil {
			return result, err
		}
		if native == "" {
			native = bootstrap.NativeCatalog
		}
	}
	defaultsProfile := ""
	if bootstrap.ReferenceGraph != "" {
		// Initial native bootstrap also warms defaults once. Ordinary source-only
		// preparations never start an Editor merely to satisfy component tools.
		defaults, err := s.captureComponentDefaults(ctx)
		if err != nil {
			return result, err
		}
		defaultsProfile = defaults.ProfileSHA256
	}
	options := goSourceGraphOptions{Project: s.project, Editor: s.editor, Player: host, Profile: profile, Packages: resolved}
	graph, err := buildGoSourceGraph(ctx, options)
	if err != nil {
		return result, err
	}
	graphPath := filepath.Join(directory, "graph.json")
	if err := agentImmutableJSON(graphPath, graph); err != nil {
		return result, err
	}
	proofPath := filepath.Join(directory, "scene-proof.json")
	if _, err := buildAgentSourceSceneProof(ctx, s.project, s.editor, scene, proofPath); err != nil {
		return result, err
	}
	compilerPath := filepath.Join(directory, "compiler.json")
	if _, err := os.Stat(compilerPath); errors.Is(err, os.ErrNotExist) {
		if _, err := prepareGoSourceCompilerGraph(ctx, options, compilerPath); err != nil {
			return result, err
		}
	} else if err != nil {
		return result, err
	}
	if native == "" {
		needed, err := agentMayNeedNative(s.project)
		if err != nil {
			return result, err
		}
		if needed {
			if s.noEditor {
				return result, agentFailure("native_catalog_missing", "source graph is prepared, but this content requires a verified native catalog")
			}
			native, err = s.prepareNative(ctx, scene, id, directory)
			if err != nil {
				return result, err
			}
		} else {
			native, err = agentIdentityCatalog(s.project, graphPath, directory)
			if err != nil {
				return result, err
			}
		}
	}
	native, err = freezeAgentSourceCatalog(ctx, s.project, directory, native)
	if err != nil {
		return result, err
	}
	baseline := filepath.Join(directory, "current.json")
	if _, err := os.Stat(baseline); errors.Is(err, os.ErrNotExist) {
		if err := prepareAgentProjectWithNative(ctx, s.project, s.editor, compilerPath, scene, baseline, proofPath, native); err != nil {
			return result, err
		}
	} else if err != nil {
		return result, err
	}
	if err := resolved.Validate(ctx); err != nil {
		return result, err
	}
	artifacts, err := captureAgentPreparationArtifacts(ctx, s.project, baseline, filepath.Join(directory, "artifacts.json"))
	if err != nil {
		return result, err
	}
	result = agentPreparation{SchemaVersion: 1, RequestID: id, Project: s.project, Scene: scene, Baseline: baseline, HostPlayer: host, NativeCatalog: native,
		EngineProfile: profilePath, PackageSnapshot: resolved.SnapshotPath, HostEvidence: bootstrap.HostEvidence, ArtifactReceipt: artifacts, ComponentDefaultsProfile: defaultsProfile,
		Backends: map[string]string{"sceneLoader": "raw_source", "compiler": "dotnet_sdk", "packages": "go_package_resolver",
			"preparation": "go_authored_source", "sceneProof": "go_source_native_resource", "build": "unity_editor_proxy"}}
	result.ReferenceGraph = bootstrap.ReferenceGraph
	if result.ReferenceGraph == "" {
		result.ReferenceGraph = seed.ReferenceGraph
	}
	if bootstrap.ReferenceGraph != "" {
		result.Backends["nativeBootstrap"] = "unity_editor_public_api_once"
	}
	if err := agentImmutableJSON(resultPath, result); err != nil {
		return result, err
	}
	return result, nil
}
