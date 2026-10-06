package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"

	goproject "unitymicroeditor/project"
	"unitymicroeditor/worker/runtimeartifact"
)

const nativeImportBatchSize = 32

func coldNativeProfile(logical string) bool {
	switch strings.ToLower(filepath.Ext(logical)) {
	case ".fbx", ".wav", ".png":
		return true
	}
	return false
}

func (importer *nativeProjectImporter) refreshPendingNative(ctx context.Context) error {
	started := time.Now()
	discovered, err := importer.session.PendingNativeImportSources()
	if err != nil {
		return err
	}
	unique := make(map[string]bool)
	for _, logical := range append(importer.pendingNativePaths, discovered...) {
		unique[logical] = true
	}
	paths := make([]string, 0, len(unique))
	for logical := range unique {
		paths = append(paths, logical)
	}
	sort.Strings(paths)
	importer.pendingNativePaths = paths
	emitEvent(map[string]any{"event": "native-discovery", "assetCount": len(paths), "elapsedSeconds": time.Since(started).Seconds()})
	var coldPaths []string
	for _, logical := range paths {
		warm, err := nativePNGNeedsWarm(importer.opts.Project, logical)
		if err != nil {
			return err
		}
		if warm {
			if err := importer.importWarmUI(ctx, logical, map[string]bool{}); err != nil {
				return err
			}
		} else {
			coldPaths = append(coldPaths, logical)
		}
	}
	paths = coldPaths
	importer.pendingNativePaths = paths
	for len(paths) > 0 {
		count := nativeImportBatchSize
		if len(paths) < count {
			count = len(paths)
		}
		if err := importer.importColdBatch(ctx, paths[:count]); err != nil {
			return err
		}
		paths = paths[count:]
		importer.pendingNativePaths = paths
	}
	return nil
}

type nativeColdInput struct {
	path, guid, sourceSHA, metaSHA, cachePath string
}

func (importer *nativeProjectImporter) coldInput(logical string) (nativeColdInput, error) {
	var result nativeColdInput
	if !coldNativeProfile(logical) {
		return result, errors.New("unsupported native cold profile: " + logical)
	}
	source, err := resolveProjectImportSource(importer.opts.Project, logical)
	if err != nil {
		return result, err
	}
	sha, err := goFileHash(source)
	if err != nil {
		return result, err
	}
	meta, err := os.ReadFile(source + ".meta")
	if err != nil {
		return result, err
	}
	guid, err := registryMetaGUID(meta)
	if err != nil {
		return result, err
	}
	metaSHA := bytesSHA(meta)
	signature := bytesSHA([]byte("native-runtime-v3\x00" + importer.environmentSHA256 + "\x00" + logical + "\x00" + sha + "\x00" + metaSHA))
	return nativeColdInput{logical, guid, sha, metaSHA, filepath.Join(importer.opts.Output, "native/cache", signature+".json")}, nil
}

func (importer *nativeProjectImporter) importColdBatch(ctx context.Context, logicals []string) error {
	if err := importer.requireCurrentEnvironment(); err != nil {
		return err
	}
	if len(logicals) == 0 || len(logicals) > nativeImportBatchSize {
		return errors.New("native cold import batch exceeds supported asset count")
	}
	inputs := make(map[string]nativeColdInput, len(logicals))
	guids := make(map[string]bool, len(logicals))
	for _, logical := range logicals {
		input, err := importer.coldInput(logical)
		if err != nil {
			return err
		}
		if _, duplicate := inputs[logical]; duplicate || guids[input.guid] {
			return errors.New("native cold import batch has a duplicate path or GUID")
		}
		inputs[logical], guids[input.guid] = input, true
	}
	root := filepath.Join(importer.opts.Output, "native/imports", randomID())
	if err := os.MkdirAll(filepath.Dir(root), 0700); err != nil {
		return err
	}
	importContext, cancel := context.WithTimeout(ctx, 45*time.Second)
	defer cancel()
	report, err := runProjectImport(importContext, options{Project: importer.opts.Project, Editor: importer.opts.Editor, Output: root, Timeout: 45 * time.Second}, logicals)
	if report != nil {
		if writeErr := atomicJSON(filepath.Join(root, "result.json"), report); writeErr != nil {
			return errors.Join(err, writeErr)
		}
	}
	if err != nil {
		return err
	}
	return importer.adoptColdReport(ctx, report, root, filepath.Join(root, "result.json"), inputs, logicals)
}

func (importer *nativeProjectImporter) adoptColdReport(ctx context.Context, report *probeReport, root, reportPath string, inputs map[string]nativeColdInput, logicals []string) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	environment, err := readWorkerEnvironment(importer.opts.Project)
	if err != nil || report == nil || report.Environment == nil || *report.Environment != *environment || report.MainEditorStarted || report.WorkerExit != "exit 0" {
		return errors.New("native batch report does not prove the current standalone worker environment")
	}
	content, ok := report.Content.(projectImportContent)
	if !ok || !report.Success || !content.SourceUnchanged || len(content.Assets) != len(inputs) {
		return errors.New("native worker produced no complete validated project batch")
	}
	var replacements []goproject.NativeReplacement
	for _, asset := range content.Assets {
		input, expected := inputs[asset.Path]
		if !expected || asset.GUID != input.guid {
			return errors.New("native cold batch result differs from its source path/GUID")
		}
		delete(inputs, asset.Path)
		filename := filepath.Join(root, "runtime", asset.GUID+".bin")
		replacement, err := importer.adaptColdAsset(asset, input, filename)
		if err != nil {
			return err
		}
		replacements = append(replacements, replacement)
	}
	if len(inputs) != 0 {
		return errors.New("native cold batch omitted an input asset")
	}
	if err := importer.requireCurrentEnvironment(); err != nil {
		return err
	}
	ordered, err := orderNativeBatch(replacements, importer.entries)
	if err != nil {
		return err
	}
	for _, replacement := range ordered {
		if err := importer.session.ReplaceNativeContent(replacement); err != nil {
			return err
		}
		importer.entries[replacement.GUID] = replacement
		input, err := importer.coldInput(replacement.SourcePath)
		if err != nil || input.sourceSHA != replacement.SourceSHA256 || input.metaSHA != replacement.MetaSHA256 {
			return errors.New("native cold batch source changed before cache publication")
		}
		if err := os.MkdirAll(filepath.Dir(input.cachePath), 0700); err != nil {
			return err
		}
		if err := atomicJSON(input.cachePath, replacement); err != nil {
			return err
		}
	}
	if err := importer.publishIndex(); err != nil {
		return err
	}
	emitEvent(map[string]any{"event": "native-imported", "assetCount": len(ordered), "paths": logicals, "elapsedSeconds": report.ElapsedSeconds, "workerPeakRssKiB": report.WorkerPeakRSSKiB, "report": reportPath})
	return nil
}

func (importer *nativeProjectImporter) adaptColdAsset(asset projectImportedAsset, input nativeColdInput, filename string) (goproject.NativeReplacement, error) {
	var result goproject.NativeReplacement
	var primary *projectImportedFile
	for index := range asset.Files {
		if asset.Files[index].Name == "" {
			if primary != nil {
				return result, errors.New("native asset has multiple serialized outputs")
			}
			primary = &asset.Files[index]
		}
	}
	if primary == nil {
		return result, errors.New("native asset has no serialized primary output")
	}
	data, err := os.ReadFile(primary.Path)
	if err != nil || bytesSHA(data) != primary.SHA256 {
		return result, errors.New("native primary artifact changed")
	}
	profile := strings.TrimPrefix(strings.ToLower(filepath.Ext(input.path)), ".")
	var output []byte
	var metadata runtimeartifact.Metadata
	if profile == "png" {
		output, metadata, err = runtimeartifact.AdaptTexture(data, profile)
	} else {
		output, metadata, err = runtimeartifact.Adapt(data, profile)
	}
	if err != nil {
		return result, fmt.Errorf("adapt native %s: %w", input.path, err)
	}
	if profile == "fbx" && len(asset.Files) != 1 {
		return result, errors.New("streamed FBX resources are not supported by the runtime adapter")
	}
	if err := os.MkdirAll(filepath.Dir(filename), 0700); err != nil {
		return result, err
	}
	if err := os.WriteFile(filename, output, 0600); err != nil {
		return result, err
	}
	result = goproject.NativeReplacement{GUID: asset.GUID, SourcePath: input.path, SourceSHA256: input.sourceSHA, MetaSHA256: input.metaSHA, EnvironmentSHA256: importer.environmentSHA256,
		Content: goproject.NativeContentFile{GUID: asset.GUID, Path: filename, SHA256: metadata.OutputSHA256}}
	result.ConfigSourceSHA256, err = warmTextureConfiguration(importer.opts.Project, input.path)
	if err != nil {
		return result, err
	}
	for _, object := range metadata.Objects {
		result.Content.Objects = append(result.Content.Objects, goproject.NativeContentObject{FileID: object.FileID, TypeName: object.TypeName})
	}
	for _, dependency := range metadata.ExternalReferences {
		global := dependency.GUID == "0000000000000000f000000000000000" || dependency.GUID == "0000000000000000e000000000000000"
		result.Content.Dependencies = append(result.Content.Dependencies, goproject.NativeContentDependency{GUID: dependency.GUID, Global: global})
	}
	for _, resource := range metadata.Resources {
		if resource.VirtualPath != "VirtualArtifacts/Primary/"+asset.GUID+".resource" {
			return result, errors.New("native resource alias differs from source GUID")
		}
		var found *projectImportedFile
		for index := range asset.Files {
			if asset.Files[index].Name == ".resource" {
				if found != nil {
					return result, errors.New("multiple native resource outputs")
				}
				found = &asset.Files[index]
			}
		}
		if found == nil || resource.Offset > found.Bytes || resource.Size > found.Bytes-resource.Offset {
			return result, errors.New("native resource file is missing or has invalid bounds")
		}
		data, err := os.ReadFile(found.Path)
		if err != nil || uint64(len(data)) != found.Bytes || bytesSHA(data) != found.SHA256 {
			return result, errors.New("native resource content changed")
		}
		result.Content.Resources = append(result.Content.Resources, goproject.NativeContentResource{Path: found.Path, VirtualPath: resource.VirtualPath, SHA256: found.SHA256})
	}
	encoded, err := json.Marshal(metadata)
	if err != nil {
		return result, err
	}
	var graph struct {
		ModelGraph []goproject.NativeModelGraphObject `json:"modelGraph"`
	}
	if err := json.Unmarshal(encoded, &graph); err != nil {
		return result, err
	}
	result.ModelGraph, result.ModelGraphSHA256 = graph.ModelGraph, metadata.ModelTopologySHA256
	return importer.retainNativeModelMeshCPU(result)
}

func orderNativeBatch(entries []goproject.NativeReplacement, loaded map[string]goproject.NativeReplacement) ([]goproject.NativeReplacement, error) {
	byGUID := make(map[string]goproject.NativeReplacement)
	for _, entry := range entries {
		if _, duplicate := byGUID[entry.GUID]; duplicate {
			return nil, errors.New("duplicate GUID in native adapted batch")
		}
		byGUID[entry.GUID] = entry
	}
	var ordered []goproject.NativeReplacement
	visiting, visited := map[string]bool{}, map[string]bool{}
	var visit func(string) error
	visit = func(guid string) error {
		if visited[guid] {
			return nil
		}
		entry, exists := byGUID[guid]
		if !exists {
			if _, exists := loaded[guid]; exists {
				return nil
			}
			return errors.New("native batch dependency has no imported runtime descriptor: " + guid)
		}
		if visiting[guid] {
			return errors.New("native batch dependency cycle: " + guid)
		}
		visiting[guid] = true
		for _, dependency := range entry.Content.Dependencies {
			if !dependency.Global {
				if err := visit(dependency.GUID); err != nil {
					return err
				}
			}
		}
		delete(visiting, guid)
		visited[guid] = true
		ordered = append(ordered, entry)
		return nil
	}
	guids := make([]string, 0, len(byGUID))
	for guid := range byGUID {
		guids = append(guids, guid)
	}
	sort.Strings(guids)
	for _, guid := range guids {
		if err := visit(guid); err != nil {
			return nil, err
		}
	}
	return ordered, nil
}
