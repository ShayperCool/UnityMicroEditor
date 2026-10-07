package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"

	goproject "unitymicroeditor/project"
	"unitymicroeditor/worker/internal/editorbridge"
)

func agentSetting(project, path string) (map[string]any, error) {
	data, err := readAgentFile(filepath.Join(project, path), 32<<20)
	if errors.Is(err, os.ErrNotExist) {
		return map[string]any{}, nil
	}
	if err != nil {
		return nil, err
	}
	documents, err := goproject.ParseUnityYAML(data)
	if err != nil {
		return nil, err
	}
	if len(documents) != 1 {
		return nil, errors.New("project setting requires exactly one authored document: " + path)
	}
	return documents[0].Data, nil
}

func agentFileID(value any) string {
	if value == nil {
		return "0"
	}
	return fmt.Sprint(value)
}
func agentReference(value any) map[string]any {
	reference, _ := value.(map[string]any)
	return map[string]any{"guid": agentString(reference, "guid"), "fileId": agentFileID(reference["fileID"]), "type": reference["type"]}
}

func agentProjectReferences(project string) (map[string]any, []any, []any, []string, error) {
	graphics, err := agentSetting(project, "ProjectSettings/GraphicsSettings.asset")
	if err != nil {
		return nil, nil, nil, nil, err
	}
	quality, err := agentSetting(project, "ProjectSettings/QualitySettings.asset")
	if err != nil {
		return nil, nil, nil, nil, err
	}
	player, err := agentSetting(project, "ProjectSettings/ProjectSettings.asset")
	if err != nil {
		return nil, nil, nil, nil, err
	}
	pipeline := agentReference(graphics["m_CustomRenderPipeline"])
	levels, _ := quality["m_QualitySettings"].([]any)
	defaultLevels, _ := quality["m_PerPlatformDefaultQuality"].(map[string]any)
	index := 0
	if quality["m_CurrentQuality"] != nil {
		fmt.Sscan(fmt.Sprint(quality["m_CurrentQuality"]), &index)
	}
	if defaultLevels["Standalone"] != nil {
		fmt.Sscan(fmt.Sprint(defaultLevels["Standalone"]), &index)
	}
	if len(levels) > 0 {
		if index < 0 || index >= len(levels) {
			return nil, nil, nil, nil, errors.New("Standalone quality index is outside authored settings")
		}
		level, _ := levels[index].(map[string]any)
		candidate := agentReference(level["customRenderPipeline"])
		if candidate["fileId"] != "0" {
			pipeline = candidate
		}
	}
	global := []any{}
	settingsMap, _ := graphics["m_RenderPipelineGlobalSettingsMap"].(map[string]any)
	globalTypes := make([]string, 0, len(settingsMap))
	for typeName := range settingsMap {
		globalTypes = append(globalTypes, typeName)
	}
	sort.Strings(globalTypes)
	for _, typeName := range globalTypes {
		value := settingsMap[typeName]
		reference := agentReference(value)
		if reference["fileId"] != "0" {
			global = append(global, map[string]any{"pipelineTypeName": typeName, "guid": reference["guid"], "fileId": reference["fileId"]})
		}
	}
	preloads := []any{}
	values, _ := player["preloadedAssets"].([]any)
	for _, value := range values {
		preloads = append(preloads, agentReference(value))
	}
	seen := map[string]bool{}
	guids := []string{}
	include := func(reference map[string]any) {
		guid := agentString(reference, "guid")
		if guid != "" && reference["fileId"] != "0" && guid != goproject.BuiltinResourceGUID && guid != "0000000000000000e000000000000000" && !seen[guid] {
			seen[guid] = true
			guids = append(guids, guid)
		}
	}
	include(pipeline)
	for _, value := range global {
		include(value.(map[string]any))
	}
	for _, value := range preloads {
		include(value.(map[string]any))
	}
	return pipeline, global, preloads, guids, nil
}

func agentMayNeedNative(project string) (bool, error) {
	needed := false
	native := map[string]bool{".fbx": true, ".obj": true, ".dae": true, ".blend": true, ".wav": true, ".ogg": true, ".mp3": true, ".aiff": true, ".aif": true, ".flac": true, ".anim": true, ".controller": true, ".overridecontroller": true, ".mat": true, ".shader": true, ".shadergraph": true, ".compute": true, ".inputactions": true, ".ttf": true, ".otf": true, ".mixer": true, ".png": true, ".jpg": true, ".jpeg": true, ".bmp": true, ".tga": true, ".tif": true, ".tiff": true, ".gif": true, ".dds": true, ".psd": true, ".exr": true, ".hdr": true, ".spriteatlas": true}
	err := filepath.WalkDir(filepath.Join(project, "Assets"), func(path string, entry fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if entry.Type()&os.ModeSymlink != 0 {
			return errors.New("native preparation refuses authored symlinks")
		}
		if !entry.IsDir() && native[strings.ToLower(filepath.Ext(path))] {
			needed = true
		}
		return nil
	})
	_, _, _, settings, settingsErr := agentProjectReferences(project)
	if err == nil {
		err = settingsErr
	}
	return needed || len(settings) > 0, err
}

func (s *agentProjectService) prepareNative(ctx context.Context, scene, id, directory string) (string, error) {
	ownedIdleSource := false
	if s.client != nil {
		connection, err := s.client.OwnProcessOrAttach(ctx)
		if err != nil {
			return "", err
		}
		if connection.Owned {
			// Copy a completed importer cache only after its owned Editor exits.
			// Never snapshot an interactive or foreign Editor's live AssetDB.
			if err = s.client.Close(); err != nil {
				return "", err
			}
			s.client = nil
			ownedIdleSource = true
		}
	}
	stage, snapshot, err := cloneAgentStaging(ctx, s.project, filepath.Join(directory, "native-stage"))
	if err != nil {
		return "", err
	}
	if ownedIdleSource {
		if err = seedAgentNativeLibrary(ctx, s.project, stage); err != nil {
			return "", err
		}
	}
	_, _, _, settings, err := agentProjectReferences(s.project)
	if err != nil {
		return "", err
	}
	client, err := editorbridge.New(s.lifetime, editorbridge.Options{Project: stage, Editor: s.editor, StartupTimeout: 10 * time.Minute})
	if err != nil {
		return "", err
	}
	defer client.Close()
	arguments, _ := json.Marshal(map[string]any{"scenes": []string{scene}, "settingsGuids": settings, "assetPaths": []string{}})
	response, callErr := client.Do(ctx, editorbridge.Request{ID: agentPhaseID(id, "native"), Action: "native_prepare", Arguments: arguments})
	data, err := agentRequireResponse(response, callErr)
	if err != nil {
		if response.Error != nil && strings.Contains(response.Error.Message, "No native assets were found") {
			return "", nil
		}
		return "", err
	}
	if err = client.Close(); err != nil {
		return "", err
	}
	if err = verifyAgentSnapshot(ctx, snapshot); err != nil {
		return "", err
	}
	var result map[string]any
	if err = json.Unmarshal(data, &result); err != nil {
		return "", err
	}
	catalogPath := agentString(result, "catalogPath")
	if !goproject.IsOwnedCachePath(s.project, catalogPath) {
		return "", errors.New("native exporter catalog is outside owned project cache")
	}
	var catalog map[string]any
	if err = agentReadJSON(catalogPath, 32<<20, &catalog); err != nil {
		return "", err
	}
	if catalog["schemaVersion"] != float64(1) || catalog["success"] != true || catalog["engineVersion"] != fixtureUnityVersion || catalog["containsScenes"] != false || catalog["sceneLoader"] != "raw" || catalog["cookedRegistryVerified"] != true {
		return "", errors.New("native catalog lacks the pinned public runtime/cooked registry contract")
	}
	sourceRoots := map[string]string{}
	roots, _ := catalog["sourceRoots"].([]any)
	for _, value := range roots {
		root, _ := value.(map[string]any)
		logical, physical := agentString(root, "path"), agentString(root, "source")
		if !strings.HasPrefix(logical, "Packages/") || filepath.ToSlash(filepath.Clean(logical)) != logical || !filepath.IsAbs(physical) {
			return "", errors.New("invalid native package source root")
		}
		if withinDirectory(physical, stage) {
			relative, err := filepath.Rel(stage, physical)
			if err != nil {
				return "", err
			}
			physical = filepath.Join(s.project, relative)
		}
		canonical, err := filepath.EvalSymlinks(physical)
		if err != nil {
			return "", fmt.Errorf("native package root missing from authored target: %s: %w", logical, err)
		}
		root["source"] = canonical
		sourceRoots[logical] = canonical
	}
	dependencies, _ := catalog["sourceDependencies"].([]any)
	for _, value := range dependencies {
		dependency, _ := value.(map[string]any)
		logical := agentString(dependency, "path")
		var physical string
		if filepath.ToSlash(filepath.Clean(logical)) != logical || filepath.IsAbs(logical) {
			return "", errors.New("invalid native dependency path")
		}
		if strings.HasPrefix(logical, "Assets/") || strings.HasPrefix(logical, "ProjectSettings/") {
			physical = filepath.Join(s.project, filepath.FromSlash(logical))
		} else if strings.HasPrefix(logical, "Packages/") {
			parts := strings.SplitN(logical, "/", 3)
			if len(parts) != 3 || sourceRoots[parts[0]+"/"+parts[1]] == "" {
				return "", errors.New("native dependency lacks an exact package root")
			}
			physical = filepath.Join(sourceRoots[parts[0]+"/"+parts[1]], filepath.FromSlash(parts[2]))
		} else {
			return "", errors.New("native dependency is outside authored/package roots")
		}
		hash, err := goFileHash(physical)
		if err != nil || hash != agentString(dependency, "sha256") {
			return "", fmt.Errorf("native source dependency changed: %s", logical)
		}
		dependency["physicalPath"] = physical
		if strings.HasPrefix(logical, "Packages/") {
			dependency["sourcePath"] = physical
		}
	}
	packs, _ := catalog["packs"].([]any)
	if len(packs) == 0 {
		assets, _ := catalog["assets"].([]any)
		if catalog["nativeAssetsPresent"] != false || len(assets) != 0 {
			return "", errors.New("native catalog has no verified packs")
		}
	}
	for _, value := range packs {
		pack, _ := value.(map[string]any)
		path := agentString(pack, "bundlePath")
		if !goproject.IsOwnedCachePath(s.project, path) {
			return "", errors.New("native bundle is outside owned persistent cache")
		}
		hash, err := goFileHash(path)
		if err != nil || hash != agentString(pack, "sha256") {
			return "", errors.New("native bundle changed after verified registry readback")
		}
	}
	catalog["project"] = s.project
	catalog["backend"] = "unity_editor_public_api"
	catalog["importProject"] = stage
	destination := filepath.Join(directory, "native-catalog.json")
	if err = agentImmutableJSON(destination, catalog); err != nil {
		return "", err
	}
	return destination, nil
}

func seedAgentNativeLibrary(ctx context.Context, source, stage string) error {
	library := filepath.Join(stage, "Library")
	marker := filepath.Join(stage, ".micro-editor-library-seed.json")
	var existing map[string]any
	if err := agentReadJSON(marker, 65536, &existing); err == nil {
		if existing["sourceProject"] != source || existing["stageProject"] != stage {
			return errors.New("native Library seed belongs to another project")
		}
		if existing["seedPolicy"] != float64(2) || existing["sourceEditorOwnedAndStopped"] != true {
			return agentFailure("staging_incomplete", "Library seed predates transient cache exclusions; use a new preparation requestId")
		}
		return nil
	} else if !errors.Is(err, os.ErrNotExist) {
		return err
	}
	if _, err := os.Lstat(library); !errors.Is(err, os.ErrNotExist) {
		return agentFailure("staging_incomplete", "native staging Library exists without a completed seed record; use a new preparation requestId")
	}
	if err := agentCanonical(library); err != nil {
		return err
	}
	if err := os.MkdirAll(library, 0700); err != nil {
		return err
	}
	sourceLibrary := filepath.Join(source, "Library")
	entries, err := os.ReadDir(sourceLibrary)
	if err != nil {
		return err
	}
	for _, entry := range entries {
		// Tool stages recurse into this source Library. Compiler/UPM state and
		// process locks carry the source path or stopped process identity. Unity
		// regenerates them for the clone; retain imported assets and shaders.
		if excludeAgentLibrarySeedEntry(entry.Name()) {
			continue
		}
		path := filepath.Join(sourceLibrary, entry.Name())
		if err := agentCanonical(path); err != nil {
			return err
		}
		if _, err := goCompilerCommand(ctx, "", "cp", "-a", "--reflink=auto", "--", path, library); err != nil {
			return err
		}
	}
	return agentImmutableJSON(marker, map[string]any{"schemaVersion": 1, "seedPolicy": 2, "sourceProject": source, "stageProject": stage, "sourceEditorOwnedAndStopped": true, "copyMode": "reflink-auto", "excluded": "Library/UnityMicroEditor", "excludedTransientCaches": []string{"PackageManager", "ScriptAssemblies", "Bee", "*.pid", "*.lock", "*-lock"}})
}

func excludeAgentLibrarySeedEntry(name string) bool {
	switch name {
	case "UnityMicroEditor", "PackageManager", "ScriptAssemblies", "Bee":
		return true
	}
	return strings.HasSuffix(name, ".pid") || strings.HasSuffix(name, ".lock") || strings.HasSuffix(name, "-lock")
}

func attachAgentNativeSeed(seed map[string]any, project, catalogPath string) error {
	if catalogPath == "" {
		return nil
	}
	var catalog map[string]any
	if err := agentReadJSON(catalogPath, 32<<20, &catalog); err != nil {
		return err
	}
	pipeline, global, preloads, _, err := agentProjectReferences(project)
	if err != nil {
		return err
	}
	seed["importedAssetCatalog"] = catalogPath
	seed["importedAssetPacks"] = catalog["packs"]
	seed["importedAssetBindings"] = catalog["deferredBindings"]
	seed["renderPipelineGuid"] = pipeline["guid"]
	seed["renderPipelineFileId"] = pipeline["fileId"]
	seed["renderPipelineGlobalSettings"] = global
	seed["preloadedAssets"] = preloads
	// Project settings references must join scene references in the source
	// asset closure before manifest publication.
	selected := map[string]bool{}
	add := func(value map[string]any) {
		guid := agentString(value, "guid")
		fileId := agentString(value, "fileId")
		if guid != "" && fileId != "0" {
			selected[guid+":"+fileId] = true
		}
	}
	add(pipeline)
	for _, value := range global {
		add(value.(map[string]any))
	}
	for _, value := range preloads {
		add(value.(map[string]any))
	}
	assets := []any{}
	catalogAssets, _ := catalog["assets"].([]any)
	for _, value := range catalogAssets {
		asset, _ := value.(map[string]any)
		if selected[agentString(asset, "guid")+":"+agentString(asset, "fileId")] {
			assets = append(assets, value)
			delete(selected, agentString(asset, "guid")+":"+agentString(asset, "fileId"))
		}
	}
	// Remaining preloads can be source prefabs or game ScriptableObjects. The
	// same source asset closure resolves them; no native identity is invented.
	seed["assets"] = assets
	return nil
}

func agentIdentityCatalog(project, graphPath, directory string) (string, error) {
	var graph goCompilerGraphExport
	if err := agentReadJSON(graphPath, 32<<20, &graph); err != nil {
		return "", err
	}
	if graph.Project != project || graph.SchemaVersion != 1 {
		return "", errors.New("source identity graph belongs to another project")
	}
	if len(graph.SourceRoots) == 0 {
		return "", nil
	}
	for _, root := range graph.SourceRoots {
		if !strings.HasPrefix(root.Path, "Packages/") || filepath.ToSlash(filepath.Clean(root.Path)) != root.Path || !filepath.IsAbs(root.Source) {
			return "", errors.New("invalid registered package root in public graph")
		}
		if canonical, err := filepath.EvalSymlinks(root.Source); err != nil || canonical != root.Source {
			return "", errors.New("registered package root is unavailable or not canonical")
		}
	}
	catalog := map[string]any{"schemaVersion": 1, "project": project, "engineVersion": fixtureUnityVersion, "backend": "unity_editor_public_identity", "sourceRoots": graph.SourceRoots, "sourceDependencies": []any{}, "assets": []any{}, "packs": []any{}, "modelPrefabs": []any{}, "deferredBindings": []any{}, "nativeAssetsPresent": false, "containsScenes": false, "sceneLoader": "raw", "cookedRegistryVerified": true, "success": true}
	path := filepath.Join(directory, "package-identities.json")
	if err := agentImmutableJSON(path, catalog); err != nil {
		return "", err
	}
	return path, nil
}
