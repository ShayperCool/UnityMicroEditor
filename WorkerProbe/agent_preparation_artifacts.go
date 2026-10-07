package main

import (
	"context"
	"errors"
	"io/fs"
	"os"
	"path/filepath"

	goproject "unitymicroeditor/project"
)

type agentPreparationArtifacts struct {
	SchemaVersion int               `json:"schemaVersion"`
	Project       string            `json:"project"`
	Baseline      string            `json:"baseline"`
	PlayerRoot    string            `json:"playerRoot"`
	Files         map[string]string `json:"files"`
}

// Freeze generated output, separately from mutable authored source. Validation
// is read-only and runs before a cached preparation can stop an accepted Player.
func captureAgentPreparationArtifacts(ctx context.Context, project, baseline, output string) (string, error) {
	var descriptor struct {
		Player       string            `json:"player"`
		Manifest     string            `json:"manifest"`
		Compiler     goCompileManifest `json:"compiler"`
		NativeImport struct {
			Catalog string `json:"catalog"`
		} `json:"nativeImport"`
	}
	if err := agentReadJSON(baseline, 32<<20, &descriptor); err != nil {
		return "", err
	}
	receipt := agentPreparationArtifacts{SchemaVersion: 1, Project: project, Baseline: baseline,
		PlayerRoot: filepath.Dir(descriptor.Player), Files: map[string]string{}}
	add := func(path, expected string) error {
		if err := ctx.Err(); err != nil {
			return err
		}
		if !filepath.IsAbs(path) || !goproject.IsOwnedCachePath(project, path) {
			return errors.New("preparation output is outside owned caches: " + path)
		}
		if err := agentCanonical(path); err != nil {
			return err
		}
		if info, err := os.Lstat(path); err != nil || !info.Mode().IsRegular() {
			return errors.New("preparation output is not a regular file: " + path)
		}
		hash, err := goFileHash(path)
		if err != nil {
			return err
		}
		if expected != "" && hash != expected {
			return errors.New("preparation output differs from its declared hash: " + path)
		}
		if old := receipt.Files[path]; old != "" && old != hash {
			return errors.New("preparation output changed during inventory: " + path)
		}
		receipt.Files[path] = hash
		if len(receipt.Files) > 8192 {
			return errors.New("preparation output inventory exceeds 8192 files")
		}
		return nil
	}
	if err := add(baseline, ""); err != nil {
		return "", err
	}
	if err := add(descriptor.Manifest, ""); err != nil {
		return "", err
	}
	if descriptor.NativeImport.Catalog != "" {
		if err := add(descriptor.NativeImport.Catalog, ""); err != nil {
			return "", err
		}
	}
	if err := filepath.WalkDir(receipt.PlayerRoot, func(path string, entry fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if entry.Type()&os.ModeSymlink != 0 {
			return errors.New("preparation output contains a symlink: " + path)
		}
		if entry.IsDir() {
			return nil
		}
		return add(path, "")
	}); err != nil {
		return "", err
	}
	for _, assembly := range descriptor.Compiler.Assemblies {
		if err := add(assembly.Path, assembly.SHA256); err != nil {
			return "", err
		}
		if assembly.AssemblyInputHash != "" || descriptor.Compiler.Backend == "go_authored_source" {
			accepted, locator, err := goCompilerManifest(baseline)
			if err != nil {
				return "", err
			}
			configuration, err := agentPreparationAssemblyDescriptor(locator, assembly, accepted)
			if err != nil {
				return "", err
			}
			if err := add(configuration, ""); err != nil {
				return "", err
			}
		}
	}
	var manifest preparedScene
	if err := agentReadJSON(descriptor.Manifest, 32<<20, &manifest); err != nil {
		return "", err
	}
	for _, pack := range manifest.ImportedPacks {
		path, err := resolvePreparedSource(project, pack.Path)
		if err != nil {
			return "", err
		}
		if err := add(path, pack.SHA256); err != nil {
			return "", err
		}
	}
	for _, content := range manifest.NativeContent {
		path, err := resolvePreparedSource(project, content.Path)
		if err != nil {
			return "", err
		}
		if err := add(path, content.SHA256); err != nil {
			return "", err
		}
		for _, resource := range content.Resources {
			path, err := resolvePreparedSource(project, resource.Path)
			if err != nil {
				return "", err
			}
			if err := add(path, resource.SHA256); err != nil {
				return "", err
			}
		}
	}
	if err := agentImmutableJSON(output, receipt); err != nil {
		return "", err
	}
	return output, nil
}

// Use the same exact digest/ancestor locations as readGoAssemblyConfiguration.
// This never scans sibling output trees or accepts a nearby descriptor.
func agentPreparationAssemblyDescriptor(baseline string, assembly goCompileAssembly, manifest goCompileManifest) (string, error) {
	if _, _, err := readGoAssemblyConfiguration(baseline, assembly, manifest); err != nil {
		return "", err
	}
	starts := []string{filepath.Dir(baseline)}
	if filepath.IsAbs(assembly.Path) && filepath.Clean(assembly.Path) == assembly.Path {
		starts = append(starts, filepath.Dir(assembly.Path))
	}
	visited := map[string]bool{}
	for _, start := range starts {
		for folder := start; ; folder = filepath.Dir(folder) {
			if visited[folder] {
				break
			}
			visited[folder] = true
			for _, relative := range []string{"compiler-cache/assemblies", "compiler-cache/baseline-assemblies", "baseline-assemblies"} {
				path := filepath.Join(folder, filepath.FromSlash(relative), assembly.AssemblyInputHash, "compile.json")
				if info, err := os.Lstat(path); err == nil {
					if !info.Mode().IsRegular() {
						return "", errors.New("assembly descriptor is not a regular file")
					}
					if err := agentCanonical(path); err != nil {
						return "", err
					}
					return path, nil
				} else if !errors.Is(err, os.ErrNotExist) {
					return "", err
				}
			}
			if filepath.Dir(folder) == folder {
				break
			}
		}
	}
	return "", errors.New("exact assembly configuration cache is missing: " + assembly.Name)
}

func validateAgentPreparationArtifacts(project, baseline, player, path string) error {
	var receipt agentPreparationArtifacts
	if !goproject.IsOwnedCachePath(project, path) {
		return errors.New("preparation artifact receipt is outside owned caches")
	}
	if err := agentReadJSON(path, 4<<20, &receipt); err != nil {
		return err
	}
	if receipt.SchemaVersion != 1 || receipt.Project != project || receipt.Baseline != baseline || receipt.PlayerRoot != filepath.Dir(player) ||
		len(receipt.Files) < 3 || len(receipt.Files) > 8192 || receipt.Files[baseline] == "" || receipt.Files[player] == "" {
		return errors.New("preparation artifact receipt identity/inventory is invalid")
	}
	for file, expected := range receipt.Files {
		if !goproject.IsOwnedCachePath(project, file) {
			return errors.New("preparation artifact is outside owned caches")
		}
		if err := agentCanonical(file); err != nil {
			return err
		}
		if info, err := os.Lstat(file); err != nil || !info.Mode().IsRegular() {
			return errors.New("cached preparation artifact is not a regular file: " + file)
		}
		actual, err := goFileHash(file)
		if err != nil || !validSHA256(expected) || actual != expected {
			return errors.Join(errors.New("cached preparation artifact changed: "+file), err)
		}
	}
	return filepath.WalkDir(receipt.PlayerRoot, func(file string, entry fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if entry.Type()&os.ModeSymlink != 0 {
			return errors.New("cached preparation Player contains a symlink")
		}
		if !entry.IsDir() && receipt.Files[file] == "" {
			return errors.New("cached preparation Player contains an unrecorded file: " + file)
		}
		return nil
	})
}
