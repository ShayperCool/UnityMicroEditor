package packages

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"path/filepath"
)

func resolutionHash(snapshot *Snapshot) string {
	identity := struct {
		Project, Editor, Manifest, Lock string
		Packages                        []Package
		Inputs                          map[string]string
	}{snapshot.Project, snapshot.Editor, snapshot.ManifestHash, snapshot.LockHash, snapshot.Packages, snapshot.Inputs}
	data, _ := json.Marshal(identity)
	return hashBytes(data)
}

// LoadCurrent reads the most recently resolved immutable snapshot, rechecking
// authored hashes and package content without network access or an Editor call.
// If inputs changed, call Resolve again to obtain a new snapshot/proposed lock.
func LoadCurrent(ctx context.Context, project, editor string, options Options) (*Snapshot, error) {
	project, err := canonical(project)
	if err != nil {
		return nil, err
	}
	if editor != "" {
		editor, err = canonical(editor)
		if err != nil {
			return nil, err
		}
	}
	cache, err := ensureCache(project, options.CacheRoot)
	if err != nil {
		return nil, err
	}
	var pointer struct {
		SchemaVersion                                 int `json:"schemaVersion"`
		Project, Editor, ResolutionHash, SnapshotPath string
	}
	if _, err = readJSON(filepath.Join(cache, "current.json"), &pointer); err != nil {
		return nil, err
	}
	if pointer.SchemaVersion != 1 || pointer.Project != project || pointer.Editor != editor || !inside(filepath.Join(cache, "snapshots"), pointer.SnapshotPath) {
		return nil, errors.New("current package snapshot identity/path mismatch")
	}
	canonicalPath, err := canonical(pointer.SnapshotPath)
	if err != nil {
		return nil, err
	}
	if canonicalPath != pointer.SnapshotPath {
		return nil, errors.New("package snapshot symlink unsupported")
	}
	var snapshot Snapshot
	if _, err = readJSON(pointer.SnapshotPath, &snapshot); err != nil {
		return nil, err
	}
	if snapshot.SchemaVersion != 1 || snapshot.Project != project || snapshot.Editor != editor || snapshot.ResolutionHash != pointer.ResolutionHash || snapshot.SnapshotPath != pointer.SnapshotPath {
		return nil, errors.New("package snapshot identity mismatch")
	}
	if len(snapshot.Packages) > 1024 {
		return nil, errors.New("package snapshot exceeds package bound")
	}
	if resolutionHash(&snapshot) != snapshot.ResolutionHash {
		return nil, errors.New("package snapshot content digest mismatch")
	}
	for _, p := range snapshot.Packages {
		canonicalRoot, err := canonical(p.Path)
		if err != nil {
			return nil, err
		}
		if canonicalRoot != p.Path {
			return nil, errors.New("resolved package path changed")
		}
		switch p.Source {
		case "registry":
			if !inside(filepath.Join(project, "Library", "PackageCache"), p.Path) && !inside(filepath.Join(cache, "content"), p.Path) {
				return nil, fmt.Errorf("registry cache outside owned sources: %s", p.Name)
			}
		case "git":
			proofRoot := p.Path
			if p.Proof == "verified_git_cache_copy" {
				if !inside(filepath.Join(project, "Library", "PackageCache"), p.Path) || p.CommitContentHash == "" || !commitPattern.MatchString(p.Revision) {
					return nil, errors.New("Git materialized copy outside explicit project cache")
				}
				proofRoot = p.CommitPath
			}
			if !inside(filepath.Join(cache, "content"), proofRoot) {
				return nil, errors.New("Git package outside verified content cache")
			}
		case "builtin":
			base := filepath.Dir(editor)
			engineRoot := p.Path
			if p.Proof == "verified_engine_cache_copy" {
				if !inside(filepath.Join(project, "Library", "PackageCache"), p.Path) || p.ProfileContentHash == "" {
					return nil, errors.New("builtin materialized copy outside explicit project cache")
				}
				engineRoot = p.ProfilePath
			}
			if !inside(filepath.Join(base, "Data", "Resources", "PackageManager", "BuiltInPackages"), engineRoot) && !inside(filepath.Join(base, "Resources", "PackageManager", "BuiltInPackages"), engineRoot) {
				return nil, errors.New("builtin package outside selected engine profile")
			}
		case "embedded":
			if !inside(filepath.Join(project, "Packages"), p.Path) {
				return nil, errors.New("embedded package outside project Packages")
			}
		case "local": // Explicit local package roots are author-supplied inputs.
		default:
			return nil, fmt.Errorf("unknown package source %q", p.Source)
		}
	}
	if err = snapshot.Validate(ctx); err != nil {
		return nil, err
	}
	return &snapshot, nil
}
