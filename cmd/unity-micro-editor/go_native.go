package main

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"

	goproject "unitymicroeditor/internal/project"
)

type nativeProjectImporter struct {
	opts               launchOptions
	session            *goproject.Session
	entries            map[string]goproject.NativeReplacement
	environmentSHA256  string
	pendingNativePaths []string
}

func newNativeProjectImporter(opts launchOptions, session *goproject.Session) (*nativeProjectImporter, error) {
	environment, err := nativeEnvironmentFingerprint(opts.Project, opts.Editor)
	if err != nil {
		return nil, fmt.Errorf("native cache environment: %w", err)
	}
	importer := &nativeProjectImporter{opts: opts, session: session, entries: map[string]goproject.NativeReplacement{}, environmentSHA256: environment}
	var entries []goproject.NativeReplacement
	if err := readJSONFile(filepath.Join(opts.Output, "native/index.json"), &entries); err != nil && !errors.Is(err, os.ErrNotExist) {
		return nil, err
	}
	current := make(map[string]goproject.NativeReplacement)
	for _, entry := range entries {
		if entry.EnvironmentSHA256 != environment {
			if coldNativeProfile(entry.SourcePath) && entry.CompiledShaderSnapshot == nil {
				input, err := importer.coldInput(entry.SourcePath)
				if err == nil && input.guid == entry.GUID && input.sourceSHA == entry.SourceSHA256 && input.metaSHA == entry.MetaSHA256 {
					importer.pendingNativePaths = append(importer.pendingNativePaths, entry.SourcePath)
				}
			}
			continue
		}
		// Only a current, independently validated cache entry may replace a baseline.
		source, err := resolveProjectImportSource(opts.Project, entry.SourcePath)
		if err != nil {
			continue
		}
		sha, err := goFileHash(source)
		if err != nil || sha != entry.SourceSHA256 {
			continue
		}
		meta, err := goFileHash(source + ".meta")
		if err != nil || meta != entry.MetaSHA256 {
			continue
		}
		if err := verifyCompiledShaderCache(entry); err != nil {
			emitEvent(map[string]any{"event": "native-cache-rejected", "path": entry.SourcePath, "reason": err.Error()})
			continue
		}
		if err := validateNativeConfiguration(opts.Project, entry.SourcePath, entry.ConfigSourceSHA256); err != nil {
			continue
		}
		if _, duplicate := current[entry.GUID]; duplicate {
			return nil, errors.New("duplicate GUID in native import cache")
		}
		current[entry.GUID] = entry
	}
	visiting := make(map[string]bool)
	meshCPUUpgraded := false
	uiBindingsUpgraded := false
	var replay func(string) error
	replay = func(guid string) error {
		if _, loaded := importer.entries[guid]; loaded {
			return nil
		}
		entry, exists := current[guid]
		if !exists {
			return errors.New("native cache dependency has no current source revision: " + guid)
		}
		if visiting[guid] {
			return errors.New("native cache dependency cycle: " + guid)
		}
		visiting[guid] = true
		for _, dependency := range entry.Content.Dependencies {
			if !dependency.Global {
				if err := replay(dependency.GUID); err != nil {
					return err
				}
			}
		}
		previousContentPath := entry.Content.Path
		entry, err = importer.retainNativeModelMeshCPU(entry)
		if err != nil {
			return err
		}
		meshCPUUpgraded = meshCPUUpgraded || entry.Content.Path != previousContentPath
		previousUIBindings := len(entry.Content.UIBindings)
		entry, err = importer.bindNativeUIReferences(context.Background(), entry)
		if err != nil {
			return err
		}
		uiBindingsUpgraded = uiBindingsUpgraded || len(entry.Content.UIBindings) != previousUIBindings
		if err := session.ReplaceNativeContent(entry); err != nil {
			return fmt.Errorf("native cache %s: %w", entry.SourcePath, err)
		}
		importer.entries[guid] = entry
		delete(visiting, guid)
		return nil
	}
	for guid := range current {
		if err := replay(guid); err != nil {
			return nil, err
		}
	}
	if err := importer.requireCurrentEnvironment(); err != nil {
		return nil, err
	}
	sort.Strings(importer.pendingNativePaths)
	if meshCPUUpgraded || uiBindingsUpgraded {
		if err := importer.publishIndex(); err != nil {
			return nil, err
		}
		if meshCPUUpgraded {
			emitEvent(map[string]any{"event": "native-mesh-cpu-prepared", "entryCount": len(importer.entries), "workerStarted": false})
		}
		if uiBindingsUpgraded {
			emitEvent(map[string]any{"event": "native-ui-bindings-prepared", "entryCount": len(importer.entries), "workerStarted": false})
		}
	}
	return importer, nil
}

func (importer *nativeProjectImporter) Prepare(ctx context.Context) (goproject.Prepared, error) {
	if err := importer.requireCurrentEnvironment(); err != nil {
		return goproject.Prepared{}, err
	}
	if err := importer.refreshPendingNative(ctx); err != nil {
		return goproject.Prepared{}, err
	}
	attempted := map[string]bool{}
	for {
		prepared, err := importer.session.Prepare(importer.opts.Output)
		if err == nil {
			return prepared, nil
		}
		var needed *goproject.NeedsImportError
		if !errors.As(err, &needed) {
			return prepared, err
		}
		source := strings.TrimSuffix(needed.Path, ".meta")
		if attempted[source] || len(attempted) >= 1024 {
			return prepared, fmt.Errorf("native content import did not resolve %s: %w", source, err)
		}
		attempted[source] = true
		if err := importer.importAsset(ctx, source); err != nil {
			return prepared, err
		}
	}
}

func (importer *nativeProjectImporter) importAsset(ctx context.Context, logical string) error {
	if err := importer.requireCurrentEnvironment(); err != nil {
		return err
	}
	profile := strings.TrimPrefix(strings.ToLower(filepath.Ext(logical)), ".")
	if profile == "uxml" || profile == "uss" || profile == "ttf" || profile == "otf" || profile == "jpg" || profile == "jpeg" {
		return importer.importWarmUI(ctx, logical, map[string]bool{})
	}
	if profile == "png" {
		warm, err := nativePNGNeedsWarm(importer.opts.Project, logical)
		if err != nil {
			return err
		}
		if warm {
			return importer.importWarmUI(ctx, logical, map[string]bool{})
		}
	}
	if !coldNativeProfile(logical) {
		return fmt.Errorf("native update for %s requires an unsupported importer runtime profile (%s)", logical, profile)
	}
	input, err := importer.coldInput(logical)
	if err != nil {
		return err
	}
	var cached goproject.NativeReplacement
	if readJSONFile(input.cachePath, &cached) == nil && cached.SourcePath == logical && cached.SourceSHA256 == input.sourceSHA && cached.MetaSHA256 == input.metaSHA && cached.EnvironmentSHA256 == importer.environmentSHA256 && verifyCompiledShaderCache(cached) == nil {
		cached, err = importer.retainNativeModelMeshCPU(cached)
		if err != nil {
			return err
		}
		if err := importer.session.ReplaceNativeContent(cached); err == nil {
			importer.entries[cached.GUID] = cached
			if err := importer.publishIndex(); err != nil {
				return err
			}
			emitEvent(map[string]any{"event": "native-cache-hit", "path": logical, "guid": cached.GUID})
			return nil
		}
	}
	return importer.importColdBatch(ctx, []string{logical})
}

func (importer *nativeProjectImporter) requireCurrentEnvironment() error {
	current, err := nativeEnvironmentFingerprint(importer.opts.Project, importer.opts.Editor)
	if err != nil {
		return err
	}
	if current != importer.environmentSHA256 {
		return errors.New("native import environment changed; restart the runner to rebuild dependent content")
	}
	return nil
}

func (importer *nativeProjectImporter) publishIndex() error {
	entries := make([]goproject.NativeReplacement, 0, len(importer.entries))
	for _, entry := range importer.entries {
		entries = append(entries, entry)
	}
	sort.Slice(entries, func(i, j int) bool { return entries[i].GUID < entries[j].GUID })
	return atomicJSON(filepath.Join(importer.opts.Output, "native/index.json"), entries)
}

func stageNativeResources(manifest, runDirectory string) error {
	var doc struct {
		Files   []goproject.NativeContentFile `json:"nativeContentFiles"`
		Project string                        `json:"projectRoot"`
	}
	if err := readJSONFile(manifest, &doc); err != nil {
		return err
	}
	aliases := make(map[string]bool)
	type mount struct {
		destination string
		contents    []byte
	}
	var mounts []mount
	for _, file := range doc.Files {
		if !validNativeGUID(file.GUID) {
			return errors.New("native resource GUID is invalid")
		}
		for _, resource := range file.Resources {
			if resource.VirtualPath != "VirtualArtifacts/Primary/"+file.GUID+".resource" {
				return errors.New("native resource path is outside source GUID")
			}
			if aliases[resource.VirtualPath] {
				return errors.New("duplicate native resource alias")
			}
			aliases[resource.VirtualPath] = true
			destination := filepath.Join(runDirectory, filepath.FromSlash(resource.VirtualPath))
			canonical, err := canonicalDestination(destination)
			if err != nil || canonical != destination {
				return errors.New("native resource mount has symlink ancestors")
			}
			contents, err := os.ReadFile(resource.Path)
			if err != nil {
				return err
			}
			if bytesSHA(contents) != resource.SHA256 {
				return errors.New("native resource changed before mounting")
			}
			mounts = append(mounts, mount{destination, contents})
			if file.Path != "" {
				if !filepath.IsAbs(file.Path) || doc.Project == "" || !goproject.IsOwnedCachePath(doc.Project, file.Path) {
					return errors.New("native resource content file must be owned beneath project Temp or Library/UnityMicroEditor")
				}
				local := filepath.Join(filepath.Dir(file.Path), filepath.FromSlash(resource.VirtualPath))
				canonical, err := canonicalDestination(local)
				if err != nil || canonical != local {
					return errors.New("native content resource mirror has symlink ancestors")
				}
				if local != destination {
					mounts = append(mounts, mount{local, contents})
				}
			}
		}
	}
	// Validate the entire published set before touching a live Player's aliases.
	for _, resource := range mounts {
		destination := resource.destination
		if err := os.MkdirAll(filepath.Dir(destination), 0700); err != nil {
			return err
		}
		temporary := destination + "." + randomID() + ".tmp"
		if err := os.WriteFile(temporary, resource.contents, 0600); err != nil {
			return err
		}
		if err := os.Rename(temporary, destination); err != nil {
			os.Remove(temporary)
			return err
		}
	}
	return nil
}

func validNativeGUID(guid string) bool {
	if len(guid) != 32 || guid == strings.Repeat("0", 32) {
		return false
	}
	for _, digit := range guid {
		if digit < '0' || digit > '9' && (digit < 'a' || digit > 'f') {
			return false
		}
	}
	return true
}
