package packages

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
)

type resolver struct {
	ctx                    context.Context
	project, editor, cache string
	options                Options
	manifest               manifest
	lock                   lockFile
	inputs                 map[string]string
	builtins, embedded     map[string]string
	caches                 map[string]map[string][]string
	chosen                 map[string]Package
	direct                 map[string]string
	floors                 map[string]string
	metadata               map[string]registryMetadata
	memo                   map[string]Package
	comparable             map[string]string
	diagnostics            []string
}

// Resolve resolves package identities and dependency closure. It never writes
// Packages/manifest.json or packages-lock.json. A changed lock is a proposal.
func Resolve(ctx context.Context, project, editor string, options Options) (*Snapshot, error) {
	project, err := canonical(project)
	if err != nil {
		return nil, err
	}
	if _, err = os.Stat(filepath.Join(project, "Assets")); err != nil {
		return nil, fmt.Errorf("explicit project Assets: %w", err)
	}
	if options.MaxDownloadBytes == 0 {
		options.MaxDownloadBytes = 256 << 20
	}
	if options.MaxExpandedBytes == 0 {
		options.MaxExpandedBytes = 1 << 30
	}
	if options.MaxFiles == 0 {
		options.MaxFiles = 100000
	}
	if options.MaxPackages == 0 {
		options.MaxPackages = 1024
	}
	if options.MaxCacheBytes == 0 {
		options.MaxCacheBytes = 4 << 30
	}
	if options.MaxDownloadBytes < 1 || options.MaxExpandedBytes < 1 || options.MaxCacheBytes < 1 || options.MaxFiles < 1 || options.MaxPackages < 1 {
		return nil, errors.New("package resource bounds must be positive")
	}
	if options.MaxDownloadBytes > 2<<30 || options.MaxExpandedBytes > 2<<30 || options.MaxCacheBytes > 1<<40 || options.MaxFiles > 200000 || options.MaxPackages > 1024 {
		return nil, errors.New("package resource bounds exceed supported validation limits")
	}
	cache, err := ensureCache(project, options.CacheRoot)
	if err != nil {
		return nil, err
	}
	releaseLock, err := acquireResolveLock(ctx, cache)
	if err != nil {
		return nil, err
	}
	defer releaseLock()
	r := &resolver{ctx: ctx, project: project, cache: cache, options: options, inputs: map[string]string{}, builtins: map[string]string{}, embedded: map[string]string{}, caches: map[string]map[string][]string{}, chosen: map[string]Package{}, direct: map[string]string{}, floors: map[string]string{}, metadata: map[string]registryMetadata{}, memo: map[string]Package{}}
	if err = r.checkCacheBudget(0); err != nil {
		return nil, err
	}
	manifestPath := filepath.Join(project, "Packages", "manifest.json")
	manifestHash, err := readJSON(manifestPath, &r.manifest)
	if err != nil {
		return nil, err
	}
	r.inputs[manifestPath] = manifestHash
	if r.manifest.Dependencies == nil {
		r.manifest.Dependencies = map[string]string{}
	}
	if r.manifest.ResolutionStrategy == "" {
		r.manifest.ResolutionStrategy = "lowest"
	}
	switch r.manifest.ResolutionStrategy {
	case "lowest", "highestPatch", "highestMinor", "highest":
	default:
		return nil, fmt.Errorf("unsupported resolutionStrategy %q", r.manifest.ResolutionStrategy)
	}
	lockPath := filepath.Join(project, "Packages", "packages-lock.json")
	lockHash, err := readJSON(lockPath, &r.lock)
	if os.IsNotExist(err) {
		lockHash = "absent"
		err = nil
		r.lock.Dependencies = map[string]lockEntry{}
	}
	if err != nil {
		return nil, err
	}
	r.inputs[lockPath] = lockHash
	originalLock := r.lock
	if r.manifest.EnableLockFile != nil && !*r.manifest.EnableLockFile {
		r.lock = lockFile{Dependencies: map[string]lockEntry{}}
		r.diagnostics = append(r.diagnostics, "enableLockFile=false: authored lock preserved but ignored for selection")
	}
	versionPath := filepath.Join(project, "ProjectSettings", "ProjectVersion.txt")
	if data, err := boundedRead(versionPath, maxJSON); err == nil {
		r.inputs[versionPath] = hashBytes(data)
	} else if !os.IsNotExist(err) {
		return nil, err
	}
	if editor != "" {
		r.editor, err = canonical(editor)
		if err != nil {
			return nil, err
		}
		if err = r.indexBuiltins(); err != nil {
			return nil, err
		}
	}
	if err = r.indexEmbedded(); err != nil {
		return nil, err
	}
	if err = r.indexCaches(); err != nil {
		return nil, err
	}
	for name, request := range r.manifest.Dependencies {
		if !packageNamePattern.MatchString(name) || request == "" {
			return nil, fmt.Errorf("invalid project dependency %q", name)
		}
		r.direct[name] = request
	}
	for name := range r.embedded {
		r.direct[name] = "embedded"
	}
	// Minimum constraints only increase. Previously selected versions remain valid
	// upgrade candidates, preventing oscillation when their dependency sets differ.
	for rounds := 0; ; rounds++ {
		if rounds > options.MaxPackages*4 {
			return nil, errors.New("package dependency resolution did not converge")
		}
		changed := false
		for _, name := range sortedKeys(r.direct) {
			p, err := r.selectPackage(name, r.direct[name], true)
			if err != nil {
				return nil, err
			}
			if old, ok := r.chosen[name]; !ok || old.Version != p.Version || old.Source != p.Source || old.ContentHash != p.ContentHash {
				r.chosen[name] = p
				changed = true
			}
		}
		for _, name := range sortedKeys(r.chosen) {
			p := r.chosen[name]
			for _, dep := range sortedKeys(p.Dependencies) {
				request := p.Dependencies[dep]
				if _, err := parseVersion(request); err != nil {
					return nil, fmt.Errorf("%s dependency %s: %w", name, dep, err)
				}
				if old, ok := r.floors[dep]; !ok || compareVersion(request, old) > 0 {
					r.floors[dep] = request
					changed = true
				}
				if direct, ok := r.direct[dep]; ok {
					_ = direct // Project SemVer declarations are minimum versions.
					continue
				}
				floor := r.floors[dep]
				next, err := r.selectPackage(dep, floor, false)
				if err != nil {
					return nil, fmt.Errorf("%s dependency: %w", name, err)
				}
				if old, ok := r.chosen[dep]; !ok || old.Version != next.Version || old.ContentHash != next.ContentHash {
					r.chosen[dep] = next
					changed = true
				}
				if len(r.chosen) > options.MaxPackages {
					return nil, errors.New("package closure exceeds package bound")
				}
			}
		}
		if !changed {
			break
		}
	}
	depths, err := r.reachableDepths()
	if err != nil {
		return nil, err
	}
	snapshot := &Snapshot{SchemaVersion: 1, Project: project, Editor: r.editor, Inputs: r.inputs, ManifestHash: manifestHash, LockHash: lockHash, Diagnostics: r.diagnostics}
	proposal := lockFile{Dependencies: map[string]lockEntry{}}
	for _, name := range sortedKeys(depths) {
		p := r.chosen[name]
		p.Depth = depths[name]
		snapshot.Packages = append(snapshot.Packages, p)
		r.inputs[filepath.Join(p.Path, "package.json")] = p.ManifestHash
		version := p.Version
		if p.Source == "local" || p.Source == "git" {
			version = p.Requested
		}
		proposal.Dependencies[name] = lockEntry{Version: version, Depth: p.Depth, Source: p.Source, Dependencies: p.Dependencies, URL: p.Registry, Hash: p.Revision}
	}
	// Canonical lock comparison ignores formatting, retaining the original file.
	oldLock, _ := json.Marshal(originalLock)
	newLock, _ := json.Marshal(proposal)
	snapshot.LockChanged = string(oldLock) != string(newLock)
	snapshot.ResolutionHash = resolutionHash(snapshot)
	snapshot.SnapshotPath = filepath.Join(cache, "snapshots", snapshot.ResolutionHash, "snapshot.json")
	if snapshot.LockChanged {
		snapshot.ProposedLockPath = filepath.Join(cache, "snapshots", snapshot.ResolutionHash, "packages-lock.proposed.json")
	}
	if err = snapshot.Validate(ctx); err != nil {
		return nil, err
	}
	snapshotData, _ := json.Marshal(snapshot)
	if len(snapshotData) > maxJSON || len(newLock) > maxJSON {
		return nil, errors.New("resolved snapshot/lock exceeds JSON bound")
	}
	// Account for indentation and the current pointer before durable publication.
	snapshotData, _ = json.MarshalIndent(snapshot, "", "  ")
	proposalData, _ := json.MarshalIndent(proposal, "", "  ")
	if len(snapshotData) > maxJSON || len(proposalData) > maxJSON {
		return nil, errors.New("resolved snapshot/lock exceeds JSON bound")
	}
	if err = r.checkCacheBudget(int64(len(snapshotData) + len(proposalData) + 4096)); err != nil {
		return nil, err
	}
	if snapshot.LockChanged {
		if err = atomicBytes(snapshot.ProposedLockPath, append(proposalData, '\n')); err != nil {
			return nil, err
		}
	}
	if err = atomicJSON(snapshot.SnapshotPath, snapshot); err != nil {
		return nil, err
	}
	if err = atomicJSON(filepath.Join(cache, "current.json"), struct {
		SchemaVersion                                 int `json:"schemaVersion"`
		Project, Editor, ResolutionHash, SnapshotPath string
	}{1, project, r.editor, snapshot.ResolutionHash, snapshot.SnapshotPath}); err != nil {
		return nil, err
	}
	return snapshot, nil
}
func sortedKeys[V any](m map[string]V) []string {
	keys := make([]string, 0, len(m))
	for key := range m {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	return keys
}
func (r *resolver) indexBuiltins() error {
	base := filepath.Dir(r.editor)
	candidates := []string{filepath.Join(base, "Data", "Resources", "PackageManager", "BuiltInPackages"), filepath.Join(base, "Resources", "PackageManager", "BuiltInPackages")}
	var root string
	for _, path := range candidates {
		if info, err := os.Stat(path); err == nil && info.IsDir() {
			root = path
			break
		}
	}
	if root == "" {
		return fmt.Errorf("built-in package inventory absent for explicit Editor %s", r.editor)
	}
	entries, err := os.ReadDir(root)
	if err != nil {
		return err
	}
	for _, entry := range entries {
		if !entry.IsDir() {
			continue
		}
		path := filepath.Join(root, entry.Name())
		m, _, err := manifestAt(path)
		if os.IsNotExist(err) {
			continue
		}
		if err != nil {
			return err
		}
		if r.builtins[m.Name] != "" {
			return fmt.Errorf("duplicate built-in package %s", m.Name)
		}
		r.builtins[m.Name] = path
	}
	return nil
}
func (r *resolver) indexEmbedded() error {
	entries, err := os.ReadDir(filepath.Join(r.project, "Packages"))
	if err != nil {
		return err
	}
	for _, entry := range entries {
		if !entry.IsDir() {
			continue
		}
		path := filepath.Join(r.project, "Packages", entry.Name())
		m, _, err := manifestAt(path)
		if os.IsNotExist(err) {
			continue
		}
		if err != nil {
			return err
		}
		if r.embedded[m.Name] != "" {
			return fmt.Errorf("duplicate embedded package %s", m.Name)
		}
		r.embedded[m.Name] = path
	}
	return nil
}
func (r *resolver) indexCaches() error {
	for _, root := range []string{filepath.Join(r.project, "Library", "PackageCache"), filepath.Join(r.cache, "content")} {
		entries, err := os.ReadDir(root)
		if os.IsNotExist(err) {
			continue
		}
		if err != nil {
			return err
		}
		for _, entry := range entries {
			if !entry.IsDir() {
				continue
			}
			path := filepath.Join(root, entry.Name())
			if root == filepath.Join(r.cache, "content") {
				var receipt contentReceipt
				if _, err := readJSON(filepath.Join(r.cache, "receipts", entry.Name()+".json"), &receipt); err != nil || receipt.Source != "registry" || receipt.ContentHash != entry.Name() {
					continue
				}
			}
			m, _, err := manifestAt(path)
			if err != nil {
				continue
			}
			if r.caches[m.Name] == nil {
				r.caches[m.Name] = map[string][]string{}
			}
			r.caches[m.Name][m.Version] = append(r.caches[m.Name][m.Version], path)
		}
	}
	return nil
}
func (r *resolver) fromPath(name, root, source, requested, proof string) (Package, error) {
	root, err := canonical(root)
	if err != nil {
		return Package{}, err
	}
	if previous, ok := r.memo[root]; ok {
		if previous.Name != name {
			return Package{}, errors.New("package root identity collision")
		}
		previous.Source, previous.Requested, previous.Proof = source, requested, proof
		return previous, nil
	}
	m, mh, err := manifestAt(root)
	if err != nil {
		return Package{}, err
	}
	if m.Name != name {
		return Package{}, fmt.Errorf("package identity mismatch: expected %s, got %s", name, m.Name)
	}
	h, err := treeHash(r.ctx, root, r.options.MaxExpandedBytes, r.options.MaxFiles)
	if err != nil {
		return Package{}, err
	}
	testable := source == "embedded"
	for _, item := range r.manifest.Testables {
		if item == name {
			testable = true
		}
	}
	p := Package{Name: name, LogicalRoot: "Packages/" + name, Path: root, Version: m.Version, Source: source, Requested: requested, ContentHash: h, ManifestHash: mh, Dependencies: m.Dependencies, Type: m.Type, Unity: m.Unity, UnityRelease: m.UnityRelease, Testable: testable, Proof: proof}
	r.memo[root] = p
	return p, nil
}
func (r *resolver) selectPackage(name, request string, direct bool) (Package, error) {
	originalRequest := request
	if path := r.embedded[name]; path != "" {
		return r.fromPath(name, path, "embedded", "embedded", "verified_manifest_content")
	}
	if isGit(request) {
		return r.gitPackage(name, request)
	}
	if strings.HasPrefix(request, "file:") {
		path := strings.TrimPrefix(request, "file:")
		if !filepath.IsAbs(path) {
			path = filepath.Join(r.project, "Packages", filepath.FromSlash(path))
		}
		info, err := os.Stat(path)
		if err != nil {
			return Package{}, err
		}
		if info.IsDir() {
			return r.fromPath(name, path, "local", request, "verified_manifest_content")
		}
		return r.localTarball(name, path, request)
	}
	if _, err := parseVersion(request); err != nil {
		return Package{}, err
	}
	if floor, ok := r.floors[name]; ok && compareVersion(floor, request) > 0 {
		request = floor
	}
	if path := r.builtins[name]; path != "" {
		return r.builtinPackage(name, path, request)
	}
	if strings.HasPrefix(name, "com.unity.modules.") {
		return Package{}, fmt.Errorf("required engine module unavailable in selected Editor: %s", name)
	}
	selected := request
	registry, err := r.registryFor(name)
	if err != nil {
		return Package{}, err
	}
	locked, ok := r.lock.Dependencies[name]
	floorVersion, _ := parseVersion(request)
	if ok && locked.Source == "registry" && locked.URL == registry && compareVersion(locked.Version, request) >= 0 {
		if lockedVersion, err := parseVersion(locked.Version); err == nil && (len(floorVersion.pre) > 0 || len(lockedVersion.pre) == 0) {
			selected = locked.Version
		}
	}
	if old, ok := r.chosen[name]; ok && old.Source == "registry" && compareVersion(old.Version, selected) > 0 {
		oldVersion, _ := parseVersion(old.Version)
		if len(floorVersion.pre) > 0 || len(oldVersion.pre) == 0 {
			selected = old.Version
		}
	}
	if !direct && r.manifest.ResolutionStrategy != "lowest" && selected == request {
		selected, err = r.chooseRegistryVersion(name, request, r.manifest.ResolutionStrategy)
		if err != nil {
			return Package{}, err
		}
	}
	if selected == request && len(r.caches[name][selected]) == 0 {
		strategy := r.manifest.ResolutionStrategy
		if direct {
			strategy = "lowest"
		}
		// Offline resolution can use a verified metadata snapshot to select an
		// available higher minimum. Folder names alone never define a registry.
		selected, err = r.chooseRegistryVersion(name, request, strategy)
		if err != nil {
			return Package{}, err
		}
	}
	if paths := r.caches[name][selected]; len(paths) > 0 {
		var result Package
		for _, path := range paths {
			p, err := r.fromPath(name, path, "registry", originalRequest, "verified_manifest_content")
			if err != nil {
				return Package{}, err
			}
			if p.Version != selected {
				return Package{}, errors.New("cached package changed version during resolution")
			}
			if inside(filepath.Join(r.cache, "content"), p.Path) {
				var receipt contentReceipt
				if _, err := readJSON(filepath.Join(r.cache, "receipts", p.ContentHash+".json"), &receipt); err != nil || receipt.Name != p.Name || receipt.Version != p.Version || receipt.ContentHash != p.ContentHash || receipt.Registry != registry {
					return Package{}, errors.New("owned registry cache provenance/content mismatch")
				}
				p.Proof = receipt.Proof
			}
			if result.Path != "" && result.ContentHash != p.ContentHash {
				return Package{}, fmt.Errorf("conflicting cached contents for %s@%s", name, selected)
			}
			result = p
		}
		result.Registry = registry
		return result, nil
	}
	if r.options.Offline {
		return Package{}, fmt.Errorf("offline package missing verified source: %s@%s", name, selected)
	}
	return r.registryPackage(name, selected, originalRequest, registry)
}
func (r *resolver) reachableDepths() (map[string]int, error) {
	state := map[string]int{}
	var visit func(string, []string) error
	visit = func(name string, stack []string) error {
		if state[name] == 1 {
			return fmt.Errorf("package dependency cycle: %s", strings.Join(append(stack, name), " -> "))
		}
		if state[name] == 2 {
			return nil
		}
		p, ok := r.chosen[name]
		if !ok {
			return fmt.Errorf("unresolved dependency %s", name)
		}
		state[name] = 1
		for _, dep := range sortedKeys(p.Dependencies) {
			if err := visit(dep, append(stack, name)); err != nil {
				return err
			}
		}
		state[name] = 2
		return nil
	}
	for _, name := range sortedKeys(r.direct) {
		if err := visit(name, nil); err != nil {
			return nil, err
		}
	}
	depth := map[string]int{}
	queue := sortedKeys(r.direct)
	for _, name := range queue {
		depth[name] = 0
	}
	for len(queue) > 0 {
		name := queue[0]
		queue = queue[1:]
		for _, dep := range sortedKeys(r.chosen[name].Dependencies) {
			next := depth[name] + 1
			if old, ok := depth[dep]; !ok || next < old {
				depth[dep] = next
				queue = append(queue, dep)
			}
		}
	}
	return depth, nil
}
