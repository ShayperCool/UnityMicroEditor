package packages

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"sort"
	"strings"
)

const maxJSON = 16 * 1024 * 1024

func hashBytes(v []byte) string { sum := sha256.Sum256(v); return hex.EncodeToString(sum[:]) }
func canonical(path string) (string, error) {
	absolute, err := filepath.Abs(path)
	if err != nil {
		return "", err
	}
	return filepath.EvalSymlinks(absolute)
}
func boundedRead(path string, max int64) ([]byte, error) {
	info, err := os.Lstat(path)
	if err != nil {
		return nil, err
	}
	if !info.Mode().IsRegular() || info.Size() > max {
		return nil, fmt.Errorf("not a bounded regular file: %s", path)
	}
	f, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	data, err := io.ReadAll(io.LimitReader(f, max+1))
	if err != nil {
		return nil, err
	}
	if int64(len(data)) > max {
		return nil, errors.New("file grew beyond read bound")
	}
	return data, nil
}
func readJSON(path string, target any) (string, error) {
	data, err := boundedRead(path, maxJSON)
	if err != nil {
		return "", err
	}
	if err = json.Unmarshal(data, target); err != nil {
		return "", fmt.Errorf("%s: %w", path, err)
	}
	return hashBytes(data), nil
}
func treeHash(ctx context.Context, root string, maxBytes int64, maxFiles int) (string, error) {
	return packageTreeHash(ctx, root, maxBytes, maxFiles, false)
}

// UPM materializes builtin packages in PackageCache, adding _fingerprint only
// to package.json. Canonicalizing that one JSON property lets us verify all
// source/resource bytes against the selected engine without trusting folders.
func engineComparableHash(ctx context.Context, root string, maxBytes int64, maxFiles int) (string, error) {
	return packageTreeHash(ctx, root, maxBytes, maxFiles, true)
}

func packageTreeHash(ctx context.Context, root string, maxBytes int64, maxFiles int, normalizeManifest bool) (string, error) {
	var names []string
	var total int64
	err := filepath.WalkDir(root, func(path string, d os.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if err = ctx.Err(); err != nil {
			return err
		}
		if path == root {
			return nil
		}
		if d.Name() == ".git" && d.IsDir() {
			return filepath.SkipDir
		}
		if d.Type()&os.ModeSymlink != 0 {
			return fmt.Errorf("package symlink unsupported: %s", path)
		}
		if d.IsDir() {
			return nil
		}
		info, err := d.Info()
		if err != nil {
			return err
		}
		if !info.Mode().IsRegular() {
			return fmt.Errorf("nonregular package entry: %s", path)
		}
		total += info.Size()
		if total > maxBytes {
			return errors.New("package tree exceeds byte bound")
		}
		names = append(names, path)
		if len(names) > maxFiles {
			return errors.New("package tree exceeds file bound")
		}
		return nil
	})
	if err != nil {
		return "", err
	}
	sort.Strings(names)
	h := sha256.New()
	var actualTotal int64
	for _, path := range names {
		if err := ctx.Err(); err != nil {
			return "", err
		}
		rel, _ := filepath.Rel(root, path)
		if normalizeManifest && rel == "package.json" {
			data, err := boundedRead(path, maxJSON)
			if err != nil {
				return "", err
			}
			var value map[string]any
			if err = json.Unmarshal(data, &value); err != nil {
				return "", err
			}
			delete(value, "_fingerprint")
			data, err = json.Marshal(value)
			if err != nil {
				return "", err
			}
			fmt.Fprintf(h, "%s\x00%s\n", filepath.ToSlash(rel), hashBytes(data))
			continue
		}
		f, err := os.Open(path)
		if err != nil {
			return "", err
		}
		fileHash := sha256.New()
		n, copyErr := io.Copy(fileHash, io.LimitReader(f, maxBytes-actualTotal+1))
		closeErr := f.Close()
		if copyErr != nil {
			return "", copyErr
		}
		actualTotal += n
		if actualTotal > maxBytes {
			return "", errors.New("package file grew beyond byte bound")
		}
		if closeErr != nil {
			return "", closeErr
		}
		fmt.Fprintf(h, "%s\x00%x\n", filepath.ToSlash(rel), fileHash.Sum(nil))
	}
	return hex.EncodeToString(h.Sum(nil)), nil
}
func manifestAt(root string) (packageManifest, string, error) {
	var m packageManifest
	h, err := readJSON(filepath.Join(root, "package.json"), &m)
	if err != nil {
		return m, "", err
	}
	if !packageNamePattern.MatchString(m.Name) {
		return m, "", fmt.Errorf("invalid package name %q", m.Name)
	}
	if _, err = parseVersion(m.Version); err != nil {
		return m, "", err
	}
	if m.Dependencies == nil {
		m.Dependencies = map[string]string{}
	}
	for name, value := range m.Dependencies {
		if !packageNamePattern.MatchString(name) {
			return m, "", fmt.Errorf("invalid dependency %q", name)
		}
		if value == "" {
			return m, "", fmt.Errorf("empty package dependency %s", name)
		}
	}
	return m, h, nil
}
func inside(root, path string) bool {
	rel, err := filepath.Rel(root, path)
	return err == nil && rel != ".." && !strings.HasPrefix(rel, ".."+string(filepath.Separator)) && !filepath.IsAbs(rel)
}
func ensureCache(project, requested string) (string, error) {
	owned := filepath.Join(project, "Library", "UnityMicroEditor", "packages")
	path := owned
	if requested != "" {
		absolute, err := filepath.Abs(requested)
		if err != nil {
			return "", err
		}
		path = absolute
		if !inside(owned, path) {
			return "", errors.New("package cache must be inside owned Library/UnityMicroEditor/packages")
		}
	}
	// Refuse symlinks in existing ancestors before creating directories.
	for current := path; inside(project, current) && current != project; current = filepath.Dir(current) {
		info, err := os.Lstat(current)
		if err == nil && info.Mode()&os.ModeSymlink != 0 {
			return "", fmt.Errorf("cache ancestor is a symlink: %s", current)
		}
		if err != nil && !os.IsNotExist(err) {
			return "", err
		}
	}
	if err := os.MkdirAll(path, 0700); err != nil {
		return "", err
	}
	return path, nil
}
func atomicJSON(path string, value any) error {
	data, err := json.MarshalIndent(value, "", "  ")
	if err != nil {
		return err
	}
	return atomicBytes(path, append(data, '\n'))
}
func atomicBytes(path string, data []byte) error {
	if err := os.MkdirAll(filepath.Dir(path), 0700); err != nil {
		return err
	}
	f, err := os.CreateTemp(filepath.Dir(path), ".publish-")
	if err != nil {
		return err
	}
	name := f.Name()
	defer os.Remove(name)
	if _, err = f.Write(data); err != nil {
		f.Close()
		return err
	}
	if err = f.Sync(); err != nil {
		f.Close()
		return err
	}
	if err = f.Close(); err != nil {
		return err
	}
	return os.Rename(name, path)
}
func validateSnapshot(ctx context.Context, s *Snapshot) error {
	if s.SchemaVersion != 1 || !filepath.IsAbs(s.Project) {
		return errors.New("invalid package snapshot schema/project")
	}
	// Embedded directories can be added without changing the project manifest.
	// Every embedded package is a root dependency, so an old snapshot must reject
	// an added, removed, or retargeted root rather than omit it from compilation.
	expectedEmbedded := map[string]string{}
	for _, p := range s.Packages {
		if p.CommitPath != "" {
			if p.Source != "git" || !commitPattern.MatchString(p.Revision) {
				return errors.New("invalid materialized Git commit proof")
			}
			h, err := treeHash(ctx, p.CommitPath, 2<<30, 200000)
			if err != nil {
				return err
			}
			if h != p.CommitContentHash {
				return fmt.Errorf("verified Git commit content changed: %s", p.Name)
			}
		}
		if p.ProfilePath != "" {
			profile, err := canonical(p.ProfilePath)
			if err != nil || profile != p.ProfilePath {
				return fmt.Errorf("builtin profile path changed: %s", p.Name)
			}
			h, err := treeHash(ctx, profile, 2<<30, 200000)
			if err != nil {
				return err
			}
			if h != p.ProfileContentHash {
				return fmt.Errorf("builtin engine profile content changed: %s", p.Name)
			}
		}
		if p.Source == "embedded" {
			expectedEmbedded[p.Name] = p.Path
		}
	}
	entries, err := os.ReadDir(filepath.Join(s.Project, "Packages"))
	if err != nil {
		return err
	}
	for _, entry := range entries {
		if !entry.IsDir() {
			continue
		}
		root := filepath.Join(s.Project, "Packages", entry.Name())
		m, _, err := manifestAt(root)
		if os.IsNotExist(err) {
			continue
		}
		if err != nil {
			return err
		}
		physical, err := canonical(root)
		if err != nil {
			return err
		}
		if expectedEmbedded[m.Name] != physical {
			return fmt.Errorf("embedded package inventory changed: %s", m.Name)
		}
		delete(expectedEmbedded, m.Name)
	}
	if len(expectedEmbedded) != 0 {
		return errors.New("embedded package removed")
	}
	for path, expected := range s.Inputs {
		if err := ctx.Err(); err != nil {
			return err
		}
		if expected == "absent" {
			if _, err := os.Lstat(path); os.IsNotExist(err) {
				continue
			}
			return fmt.Errorf("previously absent package input changed: %s", path)
		}
		info, err := os.Lstat(path)
		if err != nil {
			return err
		}
		if !info.Mode().IsRegular() || info.Size() > 2<<30 {
			return fmt.Errorf("package input is not bounded regular file: %s", path)
		}
		file, err := os.Open(path)
		if err != nil {
			return err
		}
		h := sha256.New()
		n, copyErr := io.Copy(h, io.LimitReader(file, 2<<30+1))
		closeErr := file.Close()
		if copyErr != nil {
			return copyErr
		}
		if closeErr != nil {
			return closeErr
		}
		if n > 2<<30 {
			return errors.New("package input grew beyond bounds")
		}
		if hex.EncodeToString(h.Sum(nil)) != expected {
			return fmt.Errorf("package input changed: %s", path)
		}
	}
	for _, p := range s.Packages {
		if p.Source == "local" && p.Proof != "verified_local_archive" {
			root := strings.TrimPrefix(p.Requested, "file:")
			if root == p.Requested {
				return errors.New("local package request is not an explicit file path")
			}
			if !filepath.IsAbs(root) {
				root = filepath.Join(s.Project, "Packages", filepath.FromSlash(root))
			}
			physical, err := canonical(root)
			if err != nil {
				return err
			}
			if physical != p.Path {
				return fmt.Errorf("local package path changed: %s", p.Name)
			}
		}
		m, mh, err := manifestAt(p.Path)
		if err != nil {
			return err
		}
		if m.Name != p.Name || m.Version != p.Version || mh != p.ManifestHash || p.LogicalRoot != "Packages/"+p.Name {
			return fmt.Errorf("resolved package identity changed: %s", p.Name)
		}
		h, err := treeHash(ctx, p.Path, 2<<30, 200000)
		if err != nil {
			return err
		}
		if h != p.ContentHash {
			return fmt.Errorf("resolved package content changed: %s", p.Name)
		}
	}
	return nil
}
func (r *resolver) checkCacheBudget(extra int64) error {
	if extra < 0 || extra > r.options.MaxCacheBytes {
		return errors.New("package cache cannot reserve requested resource budget")
	}
	var total int64
	err := filepath.WalkDir(r.cache, func(path string, d os.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if err = r.ctx.Err(); err != nil {
			return err
		}
		if d.Type()&os.ModeSymlink != 0 {
			return errors.New("package cache symlink unsupported")
		}
		if !d.IsDir() {
			info, err := d.Info()
			if err != nil {
				return err
			}
			total += info.Size()
			if total > r.options.MaxCacheBytes-extra {
				return errors.New("package cache exceeds resource budget")
			}
		}
		return nil
	})
	return err
}
