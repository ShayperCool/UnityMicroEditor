package packages

import (
	"errors"
	"fmt"
	"path/filepath"
	"sort"
)

func (r *resolver) builtinPackage(name, profilePath, requested string) (Package, error) {
	profile, err := r.fromPath(name, profilePath, "builtin", requested, "engine_builtin")
	if err != nil {
		return Package{}, err
	}
	var paths []string
	for _, path := range r.caches[name][profile.Version] {
		if inside(filepath.Join(r.project, "Library", "PackageCache"), path) {
			paths = append(paths, path)
		}
	}
	if len(paths) == 0 {
		return profile, nil
	}
	profileHash, err := r.comparableHash(profile.Path)
	if err != nil {
		return Package{}, err
	}
	sort.Strings(paths)
	var result Package
	for _, path := range paths {
		candidate, err := r.fromPath(name, path, "builtin", requested, "verified_engine_cache_copy")
		if err != nil {
			return Package{}, err
		}
		h, err := r.comparableHash(candidate.Path)
		if err != nil {
			return Package{}, err
		}
		if candidate.Version != profile.Version || h != profileHash {
			return Package{}, fmt.Errorf("builtin PackageCache content differs from selected engine profile: %s", name)
		}
		if result.Path != "" && result.ContentHash != candidate.ContentHash {
			return Package{}, errors.New("ambiguous materialized builtin package contents")
		}
		result = candidate
	}
	result.ProfilePath = profile.Path
	result.ProfileContentHash = profile.ContentHash
	r.inputs[filepath.Join(profile.Path, "package.json")] = profile.ManifestHash
	return result, nil
}

func (r *resolver) comparableHash(path string) (string, error) {
	if r.comparable == nil {
		r.comparable = map[string]string{}
	}
	if h, ok := r.comparable[path]; ok {
		return h, nil
	}
	h, err := engineComparableHash(r.ctx, path, r.options.MaxExpandedBytes, r.options.MaxFiles)
	if err == nil {
		r.comparable[path] = h
	}
	return h, err
}
