// Package packages resolves explicit Unity project package inputs without
// invoking the Editor or changing authored manifests/locks.
package packages

import (
	"context"
	"net/http"
)

type Options struct {
	CacheRoot        string
	Offline          bool
	HTTPClient       *http.Client
	RegistryTokens   map[string]string
	MaxDownloadBytes int64
	MaxExpandedBytes int64
	MaxCacheBytes    int64
	MaxFiles         int
	MaxPackages      int
	// RefreshGit ignores a prior commit lock for explicitly requested updates.
	RefreshGit bool
}

type Package struct {
	Name         string            `json:"name"`
	LogicalRoot  string            `json:"logicalRoot"`
	Path         string            `json:"path"`
	Version      string            `json:"version"`
	Source       string            `json:"source"`
	Requested    string            `json:"requested"`
	Registry     string            `json:"registry,omitempty"`
	Revision     string            `json:"revision,omitempty"`
	Subdirectory string            `json:"subdirectory,omitempty"`
	ContentHash  string            `json:"contentHash"`
	ManifestHash string            `json:"manifestHash"`
	Dependencies map[string]string `json:"dependencies"`
	Depth        int               `json:"depth"`
	Type         string            `json:"type,omitempty"`
	Unity        string            `json:"unity,omitempty"`
	UnityRelease string            `json:"unityRelease,omitempty"`
	Testable     bool              `json:"testable"`
	Proof        string            `json:"proof"`
	// ProfilePath identifies the selected engine's exact source inventory when
	// Path reuses a byte-verified UPM materialized builtin copy.
	ProfilePath        string `json:"profilePath,omitempty"`
	ProfileContentHash string `json:"profileContentHash,omitempty"`
	CommitPath         string `json:"commitPath,omitempty"`
	CommitContentHash  string `json:"commitContentHash,omitempty"`
}

type Snapshot struct {
	SchemaVersion    int               `json:"schemaVersion"`
	Project          string            `json:"project"`
	Editor           string            `json:"editor"`
	Packages         []Package         `json:"packages"`
	Inputs           map[string]string `json:"inputs"`
	ManifestHash     string            `json:"manifestHash"`
	LockHash         string            `json:"lockHash"`
	ResolutionHash   string            `json:"resolutionHash"`
	SnapshotPath     string            `json:"snapshotPath"`
	ProposedLockPath string            `json:"proposedLockPath,omitempty"`
	LockChanged      bool              `json:"lockChanged"`
	Diagnostics      []string          `json:"diagnostics,omitempty"`
}

type manifest struct {
	Dependencies     map[string]string `json:"dependencies"`
	Registry         string            `json:"registry"`
	ScopedRegistries []struct {
		Name   string   `json:"name"`
		URL    string   `json:"url"`
		Scopes []string `json:"scopes"`
	} `json:"scopedRegistries"`
	ResolutionStrategy string   `json:"resolutionStrategy"`
	Testables          []string `json:"testables"`
	EnableLockFile     *bool    `json:"enableLockFile,omitempty"`
}
type packageManifest struct {
	Name         string            `json:"name"`
	Version      string            `json:"version"`
	Dependencies map[string]string `json:"dependencies"`
	Type         string            `json:"type"`
	Unity        string            `json:"unity"`
	UnityRelease string            `json:"unityRelease"`
}
type lockEntry struct {
	Version      string            `json:"version"`
	Depth        int               `json:"depth"`
	Source       string            `json:"source"`
	Dependencies map[string]string `json:"dependencies"`
	URL          string            `json:"url,omitempty"`
	Hash         string            `json:"hash,omitempty"`
}
type lockFile struct {
	Dependencies map[string]lockEntry `json:"dependencies"`
}

// Validate rechecks every authored input and every resolved package tree before
// consumers publish compilation. Local package changes invalidate old snapshots.
func (s *Snapshot) Validate(ctx context.Context) error { return validateSnapshot(ctx, s) }
