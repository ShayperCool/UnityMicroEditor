package main

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path"
	"path/filepath"
	"sort"
	"strings"

	goproject "unitymicroeditor/project"
)

type ProjectImportSource struct {
	Path string `json:"path"`
	GUID string `json:"guid"`
}

type ProjectImportContext struct {
	SchemaVersion int                   `json:"schemaVersion"`
	SourceProject string                `json:"sourceProject"`
	Project       string                `json:"project"`
	UnityVersion  string                `json:"unityVersion"`
	Assets        []ProjectImportSource `json:"assets"`
	SourceHashes  map[string]string     `json:"sourceHashes"`
	SourceBytes   int64                 `json:"sourceBytes"`
}

// resolveProjectImportSource accepts physical source assets and resolved UPM
// source packages. PackageCache content is input, never a seeded import cache.
func resolveProjectImportSource(project, logical string) (string, error) {
	if _, err := projectRegistryPath(logical); err != nil && !strings.HasPrefix(logical, "ProjectSettings/") {
		return "", err
	}
	filename := filepath.Join(project, filepath.FromSlash(logical))
	if _, err := os.Lstat(filename); errors.Is(err, os.ErrNotExist) && strings.HasPrefix(logical, "Packages/") {
		parts := strings.Split(logical, "/")
		if len(parts) >= 3 {
			candidates, err := filepath.Glob(filepath.Join(project, "Library", "PackageCache", parts[1]+"@*"))
			if err != nil || len(candidates) != 1 {
				return "", fmt.Errorf("package source %s is unresolved or ambiguous", parts[1])
			}
			filename = filepath.Join(candidates[0], filepath.FromSlash(strings.Join(parts[2:], "/")))
		}
	}
	canonical, err := filepath.EvalSymlinks(filename)
	if err != nil {
		return "", fmt.Errorf("resolve source %s: %w", logical, err)
	}
	if !withinDirectory(canonical, project) {
		return "", fmt.Errorf("source file resolves outside source project: %s", logical)
	}
	info, err := os.Stat(canonical)
	if err != nil || !info.Mode().IsRegular() || info.Size() > 1024*1024*1024 {
		return "", fmt.Errorf("source file is not a bounded regular file: %s", logical)
	}
	return canonical, nil
}

func copyProjectImportSource(ctx context.Context, source, destination string) (string, int64, error) {
	if err := ctx.Err(); err != nil {
		return "", 0, err
	}
	input, err := os.Open(source)
	if err != nil {
		return "", 0, err
	}
	defer input.Close()
	before, err := input.Stat()
	if err != nil || !before.Mode().IsRegular() || before.Size() > 1024*1024*1024 {
		return "", 0, errors.New("project source file is outside import bounds")
	}
	if err := os.MkdirAll(filepath.Dir(destination), 0700); err != nil {
		return "", 0, err
	}
	output, err := os.OpenFile(destination, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0600)
	if err != nil {
		return "", 0, err
	}
	defer output.Close()
	hasher := sha256.New()
	buffer := make([]byte, 128*1024)
	var copied int64
	for {
		if err := ctx.Err(); err != nil {
			return "", copied, err
		}
		n, readErr := input.Read(buffer)
		if n > 0 {
			if _, err := output.Write(buffer[:n]); err != nil {
				return "", copied, err
			}
			hasher.Write(buffer[:n])
			copied += int64(n)
		}
		if readErr == io.EOF {
			break
		}
		if readErr != nil {
			return "", copied, readErr
		}
		if copied > 1024*1024*1024 {
			return "", copied, errors.New("source file grew beyond import bounds")
		}
	}
	after, err := input.Stat()
	current, pathErr := os.Stat(source)
	if err != nil || pathErr != nil || !os.SameFile(before, current) || before.Size() != copied ||
		before.Size() != after.Size() || before.ModTime() != after.ModTime() || before.ModTime() != current.ModTime() {
		return "", copied, errors.New("project source changed while copying")
	}
	if err := output.Close(); err != nil {
		return "", copied, err
	}
	if err := os.Chtimes(destination, before.ModTime(), before.ModTime()); err != nil {
		return "", copied, err
	}
	return hex.EncodeToString(hasher.Sum(nil)), copied, nil
}

// CreateProjectImportContext materializes selected exact source/meta bytes in a
// new owned context. It runs no Editor and copies no SourceAssetDB/ArtifactDB,
// assemblies, imported Library contents or asset bundles.
func CreateProjectImportContext(ctx context.Context, sourceProject, output string, assets []string) (ProjectImportContext, error) {
	var result ProjectImportContext
	if len(assets) == 0 || len(assets) > 4096 {
		return result, errors.New("project import requires a bounded nonempty asset set")
	}
	sourceProject, err := filepath.Abs(sourceProject)
	if err != nil {
		return result, err
	}
	sourceProject, err = filepath.EvalSymlinks(sourceProject)
	if err != nil {
		return result, err
	}
	output, err = filepath.Abs(output)
	if err != nil {
		return result, err
	}
	canonical, err := canonicalDestination(output)
	if err != nil {
		return result, err
	}
	if output != canonical || !goproject.IsOwnedCachePath(sourceProject, output) {
		return result, errors.New("project import output must be a new directory inside source project Temp or Library/UnityMicroEditor without symlink ancestors")
	}
	if _, err := os.Lstat(output); !errors.Is(err, os.ErrNotExist) {
		return result, errors.New("project import output already exists")
	}
	// Validate source identities before creating output or native processes.
	var selected []ProjectImportSource
	sourceFiles := make(map[string]string)
	seen := make(map[string]bool)
	guidPaths := make(map[string]string)
	for _, asset := range assets {
		root, err := projectRegistryPath(asset)
		if err != nil || asset == root || strings.HasSuffix(asset, ".meta") {
			return result, fmt.Errorf("invalid source asset %s", asset)
		}
		if seen[strings.ToLower(asset)] {
			return result, fmt.Errorf("source asset repeated or has a case collision: %s", asset)
		}
		seen[strings.ToLower(asset)] = true
		filename, err := resolveProjectImportSource(sourceProject, asset)
		if err != nil {
			return result, err
		}
		metaName, err := resolveProjectImportSource(sourceProject, asset+".meta")
		if err != nil {
			return result, err
		}
		metaInfo, err := os.Stat(metaName)
		if err != nil || metaInfo.Size() > 16*1024*1024 {
			return result, errors.New("source .meta exceeds import bounds")
		}
		meta, err := os.ReadFile(metaName)
		if err != nil {
			return result, err
		}
		guid, err := registryMetaGUID(meta)
		if err != nil {
			return result, fmt.Errorf("%s: %w", asset, err)
		}
		if previous, ok := guidPaths[guid]; ok {
			return result, fmt.Errorf("source GUID is shared by %s and %s", previous, asset)
		}
		guidPaths[guid] = asset
		selected = append(selected, ProjectImportSource{Path: asset, GUID: guid})
		sourceFiles[asset], sourceFiles[asset+".meta"] = filename, metaName
		for folder := path.Dir(asset); folder != "Assets" && folder != "Packages" && folder != "."; folder = path.Dir(folder) {
			folderMeta, err := resolveProjectImportSource(sourceProject, folder+".meta")
			if err == nil {
				sourceFiles[folder+".meta"] = folderMeta
			} else if !errors.Is(err, os.ErrNotExist) {
				return result, err
			}
			if folder == root {
				break
			}
		}
	}
	versionPath, err := resolveProjectImportSource(sourceProject, "ProjectSettings/ProjectVersion.txt")
	if err != nil {
		return result, err
	}
	versionInfo, err := os.Stat(versionPath)
	if err != nil || versionInfo.Size() > 64*1024 {
		return result, errors.New("source ProjectVersion.txt exceeds supported bounds")
	}
	version, err := os.ReadFile(versionPath)
	matchedVersion := false
	for _, line := range strings.Split(string(version), "\n") {
		if strings.TrimSpace(line) == "m_EditorVersion: "+fixtureUnityVersion {
			matchedVersion = true
		}
	}
	if err != nil || !matchedVersion {
		return result, errors.New("source project Unity version differs from supported worker installation")
	}
	sourceFiles["ProjectSettings/ProjectVersion.txt"] = versionPath
	// These native settings files contain source configuration, never
	// pre-imported binary state. Importer settings remain in exact .meta.
	for _, settings := range []string{"AudioManager.asset", "EditorSettings.asset", "ProjectSettings.asset", "Physics2DSettings.asset", "DynamicsManager.asset", "TagManager.asset", "TimeManager.asset"} {
		logical := "ProjectSettings/" + settings
		if filename, err := resolveProjectImportSource(sourceProject, logical); err == nil {
			sourceFiles[logical] = filename
		} else if !errors.Is(err, os.ErrNotExist) {
			return result, err
		}
	}
	if err := ctx.Err(); err != nil {
		return result, err
	}
	if err := os.MkdirAll(filepath.Dir(output), 0700); err != nil {
		return result, err
	}
	if err := os.Mkdir(output, 0700); err != nil {
		return result, err
	}
	project := filepath.Join(output, "project")
	for _, directory := range []string{"Assets", "Packages", "ProjectSettings", "Library", "Temp", "Logs"} {
		if err := os.MkdirAll(filepath.Join(project, directory), 0700); err != nil {
			return result, err
		}
	}
	result = ProjectImportContext{SchemaVersion: 1, SourceProject: sourceProject, Project: project,
		UnityVersion: fixtureUnityVersion, Assets: selected, SourceHashes: make(map[string]string)}
	var logicalPaths []string
	for logical := range sourceFiles {
		logicalPaths = append(logicalPaths, logical)
	}
	sort.Strings(logicalPaths)
	for _, logical := range logicalPaths {
		filename := sourceFiles[logical]
		hash, size, err := copyProjectImportSource(ctx, filename, filepath.Join(project, filepath.FromSlash(logical)))
		if err != nil {
			return result, err
		}
		result.SourceHashes[filename] = hash
		result.SourceBytes += size
	}
	if err := os.WriteFile(filepath.Join(project, "Packages", "manifest.json"), []byte("{\"dependencies\":{}}\n"), 0600); err != nil {
		return result, err
	}
	encoded, err := json.MarshalIndent(result, "", "  ")
	if err != nil {
		return result, err
	}
	if err := os.WriteFile(filepath.Join(output, "project-context.json"), append(encoded, '\n'), 0600); err != nil {
		return result, err
	}
	return result, nil
}

func VerifyProjectImportSources(ctx context.Context, prepared ProjectImportContext) error {
	buffer := make([]byte, 128*1024)
	for filename, expected := range prepared.SourceHashes {
		if err := ctx.Err(); err != nil {
			return err
		}
		file, err := os.Open(filename)
		if err != nil {
			return err
		}
		hasher := sha256.New()
		_, readErr := io.CopyBuffer(hasher, io.LimitReader(file, 1024*1024*1024+1), buffer)
		closeErr := file.Close()
		if readErr != nil || closeErr != nil {
			return errors.Join(readErr, closeErr)
		}
		if hex.EncodeToString(hasher.Sum(nil)) != expected {
			return fmt.Errorf("source project file changed during import: %s", filename)
		}
	}
	return ctx.Err()
}
