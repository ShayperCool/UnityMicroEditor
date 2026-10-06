package main

import (
	"bytes"
	"context"
	"encoding/binary"
	"encoding/hex"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"time"

	goproject "unitymicroeditor/project"
	"unitymicroeditor/worker/runtimeartifact"
)

type warmNativeArtifact struct {
	SourcePath           string                            `json:"sourcePath"`
	GUID                 string                            `json:"guid"`
	SourceSHA256         string                            `json:"sourceSha256"`
	MetaSHA256           string                            `json:"metaSha256"`
	SourceNative         string                            `json:"sourceNativeHash"`
	MetaNative           string                            `json:"metaNativeHash"`
	ImportResult         string                            `json:"importResultId"`
	EnvironmentSHA256    string                            `json:"environmentSha256"`
	SettingsSourceSHA256 string                            `json:"settingsSourceSha256"`
	ConfigSourceSHA256   map[string]string                 `json:"configSourceSha256,omitempty"`
	Primary              ArtifactExtraction                `json:"primary"`
	Resources            []goproject.NativeContentResource `json:"resources,omitempty"`
	Metadata             runtimeartifact.Metadata          `json:"runtimeMetadata"`
}

// extractWarmNativeUI is an explicitly limited cache path. It accepts only the
// exact source/meta revision recorded in both SourceAssetDB and the current
// default import's ArtifactDB dependency tuple. Newly authored or changed files
// must use an importer; stale Library contents never become a runtime fallback.
func extractWarmNativeUI(ctx context.Context, project, logical, output string) (warmNativeArtifact, error) {
	var result warmNativeArtifact
	if err := ctx.Err(); err != nil {
		return result, err
	}
	profile := strings.TrimPrefix(strings.ToLower(filepath.Ext(logical)), ".")
	texture := profile == "png" || profile == "jpg" || profile == "jpeg"
	if profile != "uxml" && profile != "uss" && profile != "ttf" && profile != "otf" && profile != "shader" && !texture {
		return result, errors.New("warm native UI profile is unsupported")
	}
	source, err := resolveProjectImportSource(project, logical)
	if err != nil {
		return result, err
	}
	if _, err := resolveProjectImportSource(project, logical+".meta"); err != nil {
		return result, err
	}
	var databaseSnapshots = map[string]os.FileInfo{}
	for _, name := range []string{"SourceAssetDB", "ArtifactDB", "DataStore/uds.db"} {
		path := filepath.Join(project, "Library", name)
		info, err := os.Stat(path)
		if err != nil || !info.Mode().IsRegular() {
			return result, errors.New("warm UI database snapshot is unavailable")
		}
		databaseSnapshots[path] = info
	}
	data, err := os.ReadFile(source)
	if err != nil || len(data) > maximumWarmSourceBytes {
		return result, errors.New("warm UI source exceeds bounds")
	}
	meta, err := os.ReadFile(source + ".meta")
	if err != nil || len(meta) > 1<<20 {
		return result, errors.New("warm UI metadata exceeds bounds")
	}
	guid, err := registryMetaGUID(meta)
	if err != nil {
		return result, err
	}
	wire, _ := sourceRegistryGUID(guid)
	sourceHash, metaHash := nativeSourceHash(data), nativeSourceHash(meta)
	result = warmNativeArtifact{SourcePath: logical, GUID: guid, SourceSHA256: bytesSHA(data), MetaSHA256: bytesSHA(meta), SourceNative: hex.EncodeToString(sourceHash[:]), MetaNative: hex.EncodeToString(metaHash[:])}
	paths, err := ReadLMDBTable(filepath.Join(project, "Library/SourceAssetDB"), "PathToGuid")
	if err != nil {
		return result, err
	}
	verifiedSource := false
	for _, record := range paths {
		if !bytes.Equal(record.Key, []byte(strings.ToLower(logical)+"\x00")) {
			continue
		}
		if len(record.Value) != 48 || !bytes.Equal(record.Value[:16], wire) || !bytes.Equal(record.Value[16:32], metaHash[:]) || !bytes.Equal(record.Value[32:48], sourceHash[:]) {
			return result, fmt.Errorf("warm UI SourceAssetDB revision differs from current source/meta: cached=%x, current source=%x meta=%x", record.Value, sourceHash, metaHash)
		}
		verifiedSource = true
	}
	if !verifiedSource {
		return result, errors.New("warm UI source has no exact SourceAssetDB identity")
	}
	artifactPath := filepath.Join(project, "Library/ArtifactDB")
	revisions, err := ReadLMDBTable(artifactPath, "CurrentRevisions")
	if err != nil {
		return result, err
	}
	var address, importResult, revisionKey []byte
	for _, record := range revisions {
		if len(record.Value) != 56 || !bytes.Equal(record.Value[:16], wire) || binary.LittleEndian.Uint32(record.Value[16:20]) != ^uint32(0) || !bytes.Equal(record.Value[24:40], make([]byte, 16)) {
			continue
		}
		if importResult != nil {
			return result, errors.New("warm UI has ambiguous current default imports")
		}
		address, importResult, revisionKey = bytes.Clone(record.Value[:40]), bytes.Clone(record.Value[40:56]), bytes.Clone(record.Key)
	}
	if importResult == nil {
		return result, errors.New("warm UI has no current default importer result")
	}
	result.ImportResult = hex.EncodeToString(importResult)
	stats, err := ReadLMDBTable(artifactPath, "ImportResultIDToImportStats")
	if err != nil {
		return result, err
	}
	verifiedEngine := false
	for _, record := range stats {
		if bytes.Equal(record.Key, importResult) && bytes.Contains(record.Value, []byte("6000.6.3f1 (45d8eee7de74)\x00")) {
			verifiedEngine = true
		}
	}
	if !verifiedEngine {
		return result, errors.New("warm UI import was not produced by the exact pinned Unity installation")
	}
	indexes, err := ReadLMDBTable(artifactPath, "ImportAddressToImportResultIDs")
	if err != nil {
		return result, err
	}
	verifiedIndex := false
	for _, record := range indexes {
		if !bytes.Equal(record.Key, revisionKey) {
			continue
		}
		if len(record.Value) < 48 || !bytes.Equal(record.Value[:40], address) {
			return result, errors.New("warm UI import address index does not match current revision")
		}
		ids, _, err := blobArray(record.Value, 40, 16)
		if err != nil {
			return result, err
		}
		for index := 0; index < len(ids); index += 16 {
			if bytes.Equal(ids[index:index+16], importResult) {
				verifiedIndex = true
			}
		}
	}
	if !verifiedIndex {
		return result, errors.New("warm UI current import result is absent from its import address index")
	}
	dependencies, err := ReadLMDBTable(artifactPath, "ImportResultIDToArtifactDependencies")
	if err != nil {
		return result, err
	}
	tuple := append(bytes.Clone(wire), sourceHash[:]...)
	tuple = append(tuple, metaHash[:]...)
	verifiedDependencies := false
	var nativeDependencyProof []byte
	for _, record := range dependencies {
		if bytes.Equal(record.Key, importResult) {
			if !bytes.Contains(record.Value, tuple) {
				return result, errors.New("warm UI importer dependencies do not prove current source/meta hashes")
			}
			verifiedDependencies = true
			nativeDependencyProof = record.Value
		}
	}
	if !verifiedDependencies {
		return result, errors.New("warm UI import has no source dependency proof")
	}
	metadata, err := ReadLMDBTable(artifactPath, "ImportResultIDToArtifactMetaInfo")
	if err != nil {
		return result, err
	}
	var primary, resourceFile *ArtifactFile
	for _, record := range metadata {
		if !bytes.Equal(record.Key, importResult) {
			continue
		}
		decoded, err := decodeArtifactMetaInfo(record.Value)
		if err != nil {
			return result, err
		}
		if !bytes.Equal(decoded.Address[:40], address) {
			return result, errors.New("warm UI artifact metadata address does not match current import")
		}
		for _, file := range decoded.Files {
			if texture && file.Name == ".resource" {
				if resourceFile != nil || file.Type != 0 || len(file.InlineData) != 0 {
					return result, errors.New("warm texture has an ambiguous resource artifact")
				}
				owned := file
				resourceFile = &owned
				continue
			}
			if file.Name != "" {
				return result, errors.New("warm UI streamed artifacts are unsupported")
			}
			if primary != nil || file.Type != 0 || len(file.InlineData) != 0 {
				return result, errors.New("warm UI has an ambiguous or unsupported primary artifact")
			}
			copy := file
			primary = &copy
		}
	}
	if primary == nil {
		return result, errors.New("warm UI has no current serialized primary artifact")
	}
	store, err := OpenUDSArtifactStore(filepath.Join(project, "Library/DataStore"))
	if err != nil {
		return result, err
	}
	input, proof, err := store.Read(primary.Hash[:])
	if err != nil {
		return result, err
	}
	result.Primary = proof
	var runtime []byte
	var runtimeMetadata runtimeartifact.Metadata
	if profile == "shader" {
		runtime, runtimeMetadata, err = runtimeartifact.AdaptShader(input)
	} else if texture {
		runtime, runtimeMetadata, err = runtimeartifact.AdaptTexture(input, profile)
	} else {
		runtime, runtimeMetadata, err = runtimeartifact.AdaptUI(input, profile)
	}
	if err != nil {
		return result, fmt.Errorf("adapt exact-source warm UI %s: %w", logical, err)
	}
	result.Metadata = runtimeMetadata
	if runtimeMetadata.Texture != nil && runtimeMetadata.Texture.Format == 25 {
		if err := verifyWarmBC7Dependency(project, nativeDependencyProof); err != nil {
			return result, err
		}
	}
	var resourceBytes []byte
	if len(runtimeMetadata.Resources) != 0 {
		if len(runtimeMetadata.Resources) != 1 || resourceFile == nil {
			return result, errors.New("warm texture streamed payload has no exact native artifact")
		}
		resource := runtimeMetadata.Resources[0]
		if resource.VirtualPath != "VirtualArtifacts/Primary/"+guid+".resource" {
			return result, errors.New("warm texture resource alias does not match the source GUID")
		}
		content, proof, err := store.Read(resourceFile.Hash[:])
		if err != nil {
			return result, err
		}
		if resource.Offset > proof.Bytes || resource.Size > proof.Bytes-resource.Offset {
			return result, errors.New("warm texture stream exceeds resource artifact bounds")
		}
		resourceBytes = content
		result.Resources = []goproject.NativeContentResource{{Path: output + ".resource", VirtualPath: resource.VirtualPath, SHA256: proof.SHA256}}
	} else if resourceFile != nil {
		return result, errors.New("warm texture has an unused resource artifact")
	}
	// Validate source snapshots again after every database/artifact read.
	sha, err := goFileHash(source)
	if err != nil || sha != result.SourceSHA256 {
		return result, errors.New("warm UI source changed during artifact extraction")
	}
	sha, err = goFileHash(source + ".meta")
	if err != nil || sha != result.MetaSHA256 {
		return result, errors.New("warm UI metadata changed during artifact extraction")
	}
	if err := ctx.Err(); err != nil {
		return result, err
	}
	for path, before := range databaseSnapshots {
		after, err := os.Stat(path)
		if err != nil || !os.SameFile(before, after) || before.Size() != after.Size() || before.ModTime() != after.ModTime() {
			return result, errors.New("warm UI database changed during artifact validation")
		}
	}
	if err := os.MkdirAll(filepath.Dir(output), 0700); err != nil {
		return result, err
	}
	if _, err := os.Lstat(output); !errors.Is(err, os.ErrNotExist) {
		return result, errors.New("warm native UI output must be new")
	}
	canonical, err := canonicalDestination(output)
	if err != nil || canonical != output || withinDirectory(output, filepath.Join(project, "Library")) {
		return result, errors.New("warm UI output must be independent of Library without symlink ancestors")
	}
	if err := os.WriteFile(output, runtime, 0600); err != nil {
		return result, err
	}
	if len(resourceBytes) != 0 {
		if _, err := os.Lstat(output + ".resource"); !errors.Is(err, os.ErrNotExist) {
			return result, errors.New("warm texture resource output must be new")
		}
		if err := os.WriteFile(output+".resource", resourceBytes, 0600); err != nil {
			return result, err
		}
	}
	return result, nil
}

const maximumWarmSourceBytes = 64 << 20

func (importer *nativeProjectImporter) importWarmUI(ctx context.Context, logical string, visiting map[string]bool) error {
	if err := importer.requireCurrentEnvironment(); err != nil {
		return err
	}
	if visiting[logical] {
		return errors.New("warm native UI dependency cycle: " + logical)
	}
	visiting[logical] = true
	defer delete(visiting, logical)
	source, err := resolveProjectImportSource(importer.opts.Project, logical)
	if err != nil {
		return err
	}
	sha, err := goFileHash(source)
	if err != nil {
		return err
	}
	metaSHA, err := goFileHash(source + ".meta")
	if err != nil {
		return err
	}
	signature := bytesSHA([]byte("native-ui-v2\x00" + importer.environmentSHA256 + "\x00" + logical + "\x00" + sha + "\x00" + metaSHA))
	configuration, err := warmTextureConfiguration(importer.opts.Project, logical)
	if err != nil {
		return err
	}
	if len(configuration) != 0 {
		signature = bytesSHA([]byte("native-ui-v3\x00" + signature + "\x00" + configuration["ProjectSettings/EditorSettings.asset"]))
	}
	cachePath := filepath.Join(importer.opts.Output, "native/cache", signature+".json")
	var cached goproject.NativeReplacement
	if readJSONFile(cachePath, &cached) == nil && cached.SourcePath == logical && cached.SourceSHA256 == sha && cached.MetaSHA256 == metaSHA && cached.EnvironmentSHA256 == importer.environmentSHA256 && validateNativeConfiguration(importer.opts.Project, logical, cached.ConfigSourceSHA256) == nil {
		for _, dep := range cached.Content.Dependencies {
			if !dep.Global {
				if _, ok := importer.entries[dep.GUID]; !ok {
					path, err := warmNativeGUIDPath(importer.opts.Project, dep.GUID)
					if err != nil {
						return err
					}
					if err := importer.importWarmUI(ctx, path, visiting); err != nil {
						return err
					}
				}
			}
		}
		cached, err = importer.bindNativeUIReferences(ctx, cached)
		if err != nil {
			return err
		}
		if err := importer.session.ReplaceNativeContent(cached); err == nil {
			if err := importer.requireCurrentEnvironment(); err != nil {
				return err
			}
			importer.entries[cached.GUID] = cached
			if err := importer.publishIndex(); err != nil {
				return err
			}
			emitEvent(map[string]any{"event": "native-cache-hit", "path": logical, "guid": cached.GUID})
			return nil
		}
	}
	started := time.Now()
	root := filepath.Join(importer.opts.Output, "native/imports", randomID())
	filename := filepath.Join(root, "runtime.bin")
	proof, err := extractWarmNativeUI(ctx, importer.opts.Project, logical, filename)
	if err != nil {
		return fmt.Errorf("exact-source warm native UI %s: %w", logical, err)
	}
	if err := importer.requireCurrentEnvironment(); err != nil {
		return err
	}
	settings, err := readWorkerEnvironment(importer.opts.Project)
	if err != nil {
		return err
	}
	proof.EnvironmentSHA256, proof.SettingsSourceSHA256 = importer.environmentSHA256, settings.SettingsSHA256
	proof.ConfigSourceSHA256 = configuration
	if err := atomicJSON(filepath.Join(root, "warm-artifact-proof.json"), proof); err != nil {
		return err
	}
	replacement := goproject.NativeReplacement{GUID: proof.GUID, SourcePath: logical, SourceSHA256: sha, MetaSHA256: metaSHA,
		EnvironmentSHA256:  importer.environmentSHA256,
		ConfigSourceSHA256: configuration,
		Content:            goproject.NativeContentFile{GUID: proof.GUID, Path: filename, SHA256: proof.Metadata.OutputSHA256}}
	replacement.Content.Resources = proof.Resources
	for _, object := range proof.Metadata.Objects {
		replacement.Content.Objects = append(replacement.Content.Objects, goproject.NativeContentObject{FileID: object.FileID, TypeName: object.TypeName})
	}
	for _, dependency := range proof.Metadata.ExternalReferences {
		global := dependency.GUID == "0000000000000000f000000000000000" || dependency.GUID == "0000000000000000e000000000000000"
		if !global {
			if _, ok := importer.entries[dependency.GUID]; !ok {
				path, err := warmNativeGUIDPath(importer.opts.Project, dependency.GUID)
				if err != nil {
					return err
				}
				if err := importer.importWarmUI(ctx, path, visiting); err != nil {
					return err
				}
			}
		}
		replacement.Content.Dependencies = append(replacement.Content.Dependencies, goproject.NativeContentDependency{GUID: dependency.GUID, Global: global})
	}
	if err := importer.requireCurrentEnvironment(); err != nil {
		return err
	}
	replacement, err = importer.bindNativeUIReferences(ctx, replacement)
	if err != nil {
		return err
	}
	if err := importer.session.ReplaceNativeContent(replacement); err != nil {
		return err
	}
	importer.entries[replacement.GUID] = replacement
	if err := os.MkdirAll(filepath.Dir(cachePath), 0700); err != nil {
		return err
	}
	if err := atomicJSON(cachePath, replacement); err != nil {
		return err
	}
	if err := importer.publishIndex(); err != nil {
		return err
	}
	emitEvent(map[string]any{"event": "native-warm-artifact", "path": logical, "guid": replacement.GUID, "elapsedSeconds": time.Since(started).Seconds(), "proof": filepath.Join(root, "warm-artifact-proof.json")})
	return nil
}

func warmNativeGUIDPath(project, guid string) (string, error) {
	wire, err := sourceRegistryGUID(guid)
	if err != nil {
		return "", err
	}
	records, err := ReadLMDBTable(filepath.Join(project, "Library/SourceAssetDB"), "GuidToPath")
	if err != nil {
		return "", err
	}
	for _, record := range records {
		if bytes.Equal(record.Key, wire) {
			if len(record.Value) == 0 || record.Value[len(record.Value)-1] != 0 || bytes.IndexByte(record.Value[:len(record.Value)-1], 0) >= 0 {
				return "", errors.New("warm native GUID path is malformed")
			}
			logical := string(record.Value[:len(record.Value)-1])
			if _, err := projectRegistryPath(logical); err != nil {
				return "", err
			}
			return logical, nil
		}
	}
	return "", errors.New("warm native dependency has no project source identity: " + guid)
}
