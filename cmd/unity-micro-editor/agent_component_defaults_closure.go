package main

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"gopkg.in/yaml.v3"
	goproject "unitymicroeditor/internal/project"
	unitypackages "unitymicroeditor/internal/project/packages"
)

const (
	componentDefaultsClosureFiles      = 200000
	componentDefaultsClosureAssets     = 4096
	componentDefaultsClosureBytes      = int64(1 << 30)
	componentDefaultsClosureAssetBytes = int64(512 << 20)
)

type componentDefaultsClosureAsset struct {
	path, logical string
	meta          goMetadataGUIDIdentity
}

// The caller supplies the freshly verified package snapshot. Only settings
// dependencies enter the profile; unrelated assets and gameplay sources do not.
func componentDefaultsReferenceClosure(ctx context.Context, project string, snapshot *unitypackages.Snapshot, inputs map[string]string) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	if err := agentCanonical(project); err != nil {
		return err
	}
	updates := map[string]string{}
	pending := map[string]bool{}
	for _, name := range []string{"GraphicsSettings.asset", "QualitySettings.asset", "PhysicsManager.asset", "Physics2DSettings.asset", "ProjectSettings.asset"} {
		logical := "ProjectSettings/" + name
		expected, exists := inputs[logical]
		if !exists {
			continue
		}
		if expected == "absent" {
			if _, err := os.Lstat(filepath.Join(project, filepath.FromSlash(logical))); !errors.Is(err, os.ErrNotExist) {
				return fmt.Errorf("component defaults absent settings appeared: %s", logical)
			}
			continue
		}
		content, hash, _, err := componentDefaultsClosureRead(ctx, filepath.Join(project, filepath.FromSlash(logical)), 32<<20, true)
		if err != nil {
			return err
		}
		if hash != expected {
			return fmt.Errorf("component defaults settings changed before dependency capture: %s", logical)
		}
		// Headerless settings still contribute their exact byte hash. Parse their
		// plain YAML only for references; a scalar without GUIDs needs no index.
		refs, err := componentDefaultsClosureReferences(content, !bytes.Contains(content, []byte("--- !u!")))
		if err != nil {
			return fmt.Errorf("component defaults settings %s: %w", logical, err)
		}
		for guid := range refs {
			pending[guid] = true
		}
	}
	// Preserve the existing empty/builtin-only profile without scanning Assets.
	if len(pending) == 0 {
		return nil
	}
	if snapshot == nil || snapshot.SchemaVersion != 1 || snapshot.Project != project {
		return errors.New("component defaults closure requires this project's verified package snapshot")
	}
	index, err := componentDefaultsClosureIndex(ctx, project, snapshot)
	if err != nil {
		return err
	}
	visited := map[string]bool{}
	absent := map[string]bool{}
	var total int64
	for len(pending) != 0 {
		if err := ctx.Err(); err != nil {
			return err
		}
		guids := make([]string, 0, len(pending))
		for guid := range pending {
			guids = append(guids, guid)
		}
		sort.Strings(guids)
		for _, guid := range guids {
			delete(pending, guid)
			if visited[guid] {
				continue
			}
			visited[guid] = true
			if len(visited) > componentDefaultsClosureAssets {
				return errors.New("component defaults dependency closure exceeds asset bound")
			}
			asset, exists := index[guid]
			if !exists {
				// Unity can retain unresolved optional settings fields. Capture the
				// observed absence explicitly; a subsequently imported GUID changes
				// this profile and requires fresh public defaults.
				updates["settingsReferenceGuid/"+guid] = "absent"
				absent[guid] = true
				continue
			}
			serialized := componentDefaultsSerializedAsset(asset.path)
			content, hash, size, err := componentDefaultsClosureRead(ctx, asset.path, componentDefaultsClosureAssetBytes, serialized)
			if err != nil {
				return err
			}
			meta, metaHash, metaSize, err := componentDefaultsClosureRead(ctx, asset.path+".meta", goMetadataGUIDMaximum, true)
			if err != nil {
				return err
			}
			if metaHash != asset.meta.SHA256 {
				return fmt.Errorf("component defaults metadata changed after indexing: %s", asset.logical)
			}
			total += size + metaSize
			if total > componentDefaultsClosureBytes {
				return errors.New("component defaults dependency closure exceeds byte bound")
			}
			updates[asset.logical], updates[asset.logical+".meta"] = hash, metaHash
			refs, err := componentDefaultsClosureReferences(meta, true)
			if err != nil {
				return fmt.Errorf("component defaults metadata %s: %w", asset.logical, err)
			}
			if serialized {
				sourceRefs, err := componentDefaultsClosureReferences(content, false)
				if err != nil {
					return fmt.Errorf("component defaults asset %s: %w", asset.logical, err)
				}
				for next := range sourceRefs {
					refs[next] = true
				}
			}
			for next := range refs {
				if !visited[next] {
					pending[next] = true
				}
			}
		}
	}
	// Detect edits while following dependencies and publish the complete map only.
	for logical, expected := range inputs {
		if !strings.HasPrefix(logical, "ProjectSettings/") {
			continue
		}
		if expected == "absent" {
			if _, err := os.Lstat(filepath.Join(project, filepath.FromSlash(logical))); !errors.Is(err, os.ErrNotExist) {
				return fmt.Errorf("component defaults absent settings appeared: %s", logical)
			}
			continue
		}
		_, hash, _, err := componentDefaultsClosureRead(ctx, filepath.Join(project, filepath.FromSlash(logical)), 32<<20, false)
		if err != nil {
			return err
		}
		if hash != expected {
			return fmt.Errorf("component defaults settings changed during dependency capture: %s", logical)
		}
	}
	for _, asset := range index {
		expected, selected := updates[asset.logical]
		if !selected {
			continue
		}
		_, hash, _, err := componentDefaultsClosureRead(ctx, asset.path, componentDefaultsClosureAssetBytes, false)
		if err != nil {
			return err
		}
		_, metaHash, _, err := componentDefaultsClosureRead(ctx, asset.path+".meta", goMetadataGUIDMaximum, false)
		if err != nil {
			return err
		}
		if hash != expected || metaHash != updates[asset.logical+".meta"] {
			return fmt.Errorf("component defaults dependency changed during capture: %s", asset.logical)
		}
	}
	if len(absent) != 0 {
		current, err := componentDefaultsClosureIndex(ctx, project, snapshot)
		if err != nil {
			return err
		}
		for guid := range absent {
			if _, appeared := current[guid]; appeared {
				return fmt.Errorf("component defaults unresolved settings GUID appeared during capture: %s", guid)
			}
		}
	}
	for logical, hash := range updates {
		inputs[logical] = hash
	}
	return nil
}

func componentDefaultsClosureIndex(ctx context.Context, project string, snapshot *unitypackages.Snapshot) (map[string]componentDefaultsClosureAsset, error) {
	type root struct {
		logical, path string
		packaged      bool
	}
	roots := []root{{"Assets", filepath.Join(project, "Assets"), false}}
	packages := append([]unitypackages.Package(nil), snapshot.Packages...)
	sort.Slice(packages, func(i, j int) bool { return packages[i].LogicalRoot < packages[j].LogicalRoot })
	seenRoots := map[string]bool{}
	for _, pkg := range packages {
		if pkg.Name == "" || pkg.LogicalRoot != "Packages/"+pkg.Name || pkg.Path == "" || len(pkg.ContentHash) != 64 || seenRoots[pkg.LogicalRoot] {
			return nil, errors.New("component defaults package root identity is invalid")
		}
		seenRoots[pkg.LogicalRoot] = true
		roots = append(roots, root{pkg.LogicalRoot, pkg.Path, true})
	}
	result := map[string]componentDefaultsClosureAsset{}
	reader := newGoMetadataGUIDIndex()
	files := 0
	var metadataBytes int64
	for _, current := range roots {
		if err := agentCanonical(current.path); err != nil {
			return nil, err
		}
		if info, err := os.Lstat(current.path); err != nil || !info.IsDir() {
			return nil, fmt.Errorf("component defaults source root is missing: %s", current.logical)
		}
		err := filepath.WalkDir(current.path, func(path string, entry fs.DirEntry, walkErr error) error {
			if walkErr != nil {
				return walkErr
			}
			if err := ctx.Err(); err != nil {
				return err
			}
			if entry.Type()&os.ModeSymlink != 0 {
				return fmt.Errorf("component defaults source index rejects symlink: %s", path)
			}
			if entry.IsDir() {
				if path != current.path && (strings.HasPrefix(entry.Name(), ".") || strings.HasSuffix(entry.Name(), "~")) {
					return filepath.SkipDir
				}
				return nil
			}
			files++
			if files > componentDefaultsClosureFiles {
				return errors.New("component defaults GUID index exceeds file bound")
			}
			if !entry.Type().IsRegular() {
				return fmt.Errorf("component defaults source index rejects special file: %s", path)
			}
			if !strings.HasSuffix(entry.Name(), ".meta") {
				return nil
			}
			relative, err := filepath.Rel(current.path, path)
			if err != nil {
				return err
			}
			// Import-ignored package folders can have a sibling .meta as well.
			if strings.HasSuffix(strings.TrimSuffix(entry.Name(), ".meta"), "~") || strings.HasPrefix(entry.Name(), ".") {
				return nil
			}
			identity, err := reader.read(path)
			if err != nil {
				return err
			}
			metadataBytes += identity.Bytes
			if metadataBytes > componentDefaultsClosureBytes {
				return errors.New("component defaults GUID index exceeds metadata byte bound")
			}
			logical := current.logical + "/" + strings.TrimSuffix(filepath.ToSlash(relative), ".meta")
			candidate := componentDefaultsClosureAsset{strings.TrimSuffix(path, ".meta"), logical, identity}
			if previous, duplicate := result[identity.GUID]; duplicate {
				return fmt.Errorf("component defaults duplicate GUID %s: %s and %s", identity.GUID, previous.logical, logical)
			}
			result[identity.GUID] = candidate
			return nil
		})
		if err != nil {
			return nil, err
		}
	}
	return result, nil
}

// Binary imported leaves are hashed with their importer metadata. Serialized
// Unity objects require readable text; a binary object cannot prove its closure.
func componentDefaultsSerializedAsset(path string) bool {
	switch strings.ToLower(filepath.Ext(path)) {
	case ".asset", ".mat", ".prefab", ".unity", ".anim", ".controller", ".overridecontroller", ".playable", ".physicmaterial", ".physicsmaterial2d", ".rendertexture", ".lighting", ".shadervariants", ".guiskin", ".fontsettings", ".terrainlayer", ".preset", ".mask", ".flare", ".cubemap":
		return true
	}
	return false
}

func componentDefaultsClosureReferences(content []byte, metadata bool) (map[string]bool, error) {
	refs := map[string]bool{}
	var values []any
	if metadata {
		var node yaml.Node
		if err := yaml.Unmarshal(content, &node); err != nil {
			return nil, err
		}
		budget := 1000000
		var visit func(*yaml.Node, int) error
		visit = func(n *yaml.Node, depth int) error {
			budget--
			if budget < 0 || depth > 128 {
				return errors.New("component defaults metadata YAML exceeds traversal bound")
			}
			if n.Kind == yaml.AliasNode {
				return errors.New("component defaults metadata YAML aliases are unsupported")
			}
			if n.Kind == yaml.MappingNode {
				seen := map[string]bool{}
				for i := 0; i < len(n.Content); i += 2 {
					key, value := n.Content[i], n.Content[i+1]
					if key.Kind != yaml.ScalarNode || seen[key.Value] {
						return errors.New("component defaults metadata has non-scalar or duplicate mapping key")
					}
					seen[key.Value] = true
					if key.Value == "guid" {
						if value.Kind != yaml.ScalarNode {
							return errors.New("component defaults GUID must be a scalar")
						}
						if err := componentDefaultsClosureGUID(value.Value, refs); err != nil {
							return err
						}
					}
				}
			}
			for _, child := range n.Content {
				if err := visit(child, depth+1); err != nil {
					return err
				}
			}
			return nil
		}
		if err := visit(&node, 0); err != nil {
			return nil, err
		}
		return refs, nil
	}
	docs, err := goproject.ParseUnityYAML(content)
	if err != nil {
		return nil, err
	}
	for _, doc := range docs {
		values = append(values, doc.Data)
	}
	var walk func(any) error
	walk = func(value any) error {
		switch value := value.(type) {
		case map[string]any:
			if guid, exists := value["guid"]; exists {
				text, ok := guid.(string)
				if !ok {
					return errors.New("component defaults GUID must be a string")
				}
				if err := componentDefaultsClosureGUID(text, refs); err != nil {
					return err
				}
			}
			for _, child := range value {
				if err := walk(child); err != nil {
					return err
				}
			}
		case []any:
			for _, child := range value {
				if err := walk(child); err != nil {
					return err
				}
			}
		}
		return nil
	}
	for _, value := range values {
		if err := walk(value); err != nil {
			return nil, err
		}
	}
	return refs, nil
}

func componentDefaultsClosureGUID(guid string, refs map[string]bool) error {
	if guid == "" {
		return nil
	}
	decoded, err := hex.DecodeString(guid)
	if err != nil || len(decoded) != 16 {
		return fmt.Errorf("component defaults has invalid GUID %q", guid)
	}
	guid = strings.ToLower(guid)
	if guid == strings.Repeat("0", 32) || guid == "0000000000000000e000000000000000" || guid == "0000000000000000f000000000000000" {
		return nil
	}
	refs[guid] = true
	return nil
}

func componentDefaultsClosureRead(ctx context.Context, path string, limit int64, retain bool) ([]byte, string, int64, error) {
	if err := ctx.Err(); err != nil {
		return nil, "", 0, err
	}
	if err := agentCanonical(path); err != nil {
		return nil, "", 0, err
	}
	before, err := os.Lstat(path)
	if err != nil || !before.Mode().IsRegular() || before.Size() > limit || (retain && before.Size() > 32<<20) {
		return nil, "", 0, fmt.Errorf("component defaults expected bounded regular source: %s", path)
	}
	file, err := os.Open(path)
	if err != nil {
		return nil, "", 0, err
	}
	defer file.Close()
	opened, err := file.Stat()
	if err != nil || !os.SameFile(before, opened) {
		return nil, "", 0, fmt.Errorf("component defaults source changed before reading: %s", path)
	}
	hasher := sha256.New()
	var content bytes.Buffer
	var writer io.Writer = hasher
	if retain {
		writer = io.MultiWriter(hasher, &content)
	}
	count, err := io.Copy(writer, io.LimitReader(file, before.Size()+1))
	if err != nil {
		return nil, "", 0, err
	}
	after, statErr := file.Stat()
	current, pathErr := os.Lstat(path)
	canonicalErr := agentCanonical(path)
	if statErr != nil || pathErr != nil || canonicalErr != nil || !os.SameFile(before, after) || !os.SameFile(before, current) || count != before.Size() || before.Size() != after.Size() || before.Size() != current.Size() || !before.ModTime().Equal(after.ModTime()) || !before.ModTime().Equal(current.ModTime()) {
		return nil, "", 0, fmt.Errorf("component defaults source changed while reading: %s", path)
	}
	if err := ctx.Err(); err != nil {
		return nil, "", 0, err
	}
	return content.Bytes(), hex.EncodeToString(hasher.Sum(nil)), count, nil
}
