package main

import (
	"bytes"
	"encoding/binary"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"os"
	"path"
	"path/filepath"
	"sort"
	"strconv"
	"strings"

	"github.com/zeebo/xxh3"
)

// ProjectRegistryAsset describes a worker-visible source file. Source and Meta
// default to Project/Path and Project/Path.meta. Explicit paths must resolve to
// those files; materializing a read-only source project is the caller's job.
// Parent folders are added automatically, with their original .meta GUID when
// available. Directory entries can also be supplied explicitly.
type ProjectRegistryAsset struct {
	Path      string
	Source    string
	Meta      string
	GUID      string
	Directory bool
}

type ProjectRegistryInput struct {
	Project      string
	UnityVersion string
	Assets       []ProjectRegistryAsset
}

type projectRegistryEntry struct {
	path, guid string
	wireGUID   []byte
	directory  bool
	meta       []byte
}

type registryFileHash struct {
	hash  [16]byte
	ticks uint64
	size  int64
}

func projectRegistryContext(project string) (string, error) {
	abs, err := filepath.Abs(project)
	if err != nil {
		return "", err
	}
	canonical, err := filepath.EvalSymlinks(abs)
	if err != nil || canonical != abs || !strings.Contains(abs, string(filepath.Separator)+"Temp"+string(filepath.Separator)) {
		return "", errors.New("project registry requires a new owned context under Temp without symlink ancestors")
	}
	info, err := os.Stat(abs)
	if err != nil || !info.IsDir() {
		return "", errors.New("project registry context is not a directory")
	}
	library := filepath.Join(abs, "Library")
	canonicalLibrary, err := filepath.EvalSymlinks(library)
	if err != nil || canonicalLibrary != library {
		return "", errors.New("project registry requires an existing owned Library directory")
	}
	if info, err := os.Stat(library); err != nil || !info.IsDir() {
		return "", errors.New("project registry Library is not a directory")
	}
	return abs, nil
}

func projectRegistryPath(name string) (string, error) {
	if name == "" || strings.ContainsAny(name, "\\\x00\r\n") || path.Clean(name) != name || path.IsAbs(name) || len(name) > 480 {
		return "", fmt.Errorf("invalid source asset path %q", name)
	}
	if name == "Assets" || strings.HasPrefix(name, "Assets/") {
		return "Assets", nil
	}
	parts := strings.Split(name, "/")
	if len(parts) >= 2 && parts[0] == "Packages" && parts[1] != "" && parts[1] != "." && parts[1] != ".." {
		return strings.Join(parts[:2], "/"), nil
	}
	return "", fmt.Errorf("source asset path must be inside Assets or a named Packages root: %q", name)
}

func registryGUIDText(wire []byte) string {
	ordinary := bytes.Clone(wire)
	for index, value := range ordinary {
		ordinary[index] = value<<4 | value>>4
	}
	return hex.EncodeToString(ordinary)
}

// registryMetaGUID only accepts the top-level GUID emitted in Unity .meta
// files. Nested object references must never become this asset's identity.
func registryMetaGUID(data []byte) (string, error) {
	var guid string
	for _, line := range strings.Split(string(data), "\n") {
		if !strings.HasPrefix(line, "guid:") {
			continue
		}
		if guid != "" {
			return "", errors.New("source .meta has duplicate top-level GUID fields")
		}
		guid = strings.TrimSpace(strings.TrimPrefix(line, "guid:"))
	}
	wire, err := sourceRegistryGUID(guid)
	if err != nil || bytes.Equal(wire, make([]byte, 16)) {
		return "", errors.New("source .meta must contain one nonzero 32-hex GUID")
	}
	return strings.ToLower(guid), nil
}

func registryExpectedFile(project, logical, supplied string, directory bool) (string, error) {
	expected := filepath.Join(project, filepath.FromSlash(logical))
	if supplied != "" {
		actual, err := filepath.Abs(supplied)
		if err != nil || actual != expected {
			return "", fmt.Errorf("registry path differs from worker-visible source: %s", logical)
		}
	}
	canonical, err := filepath.EvalSymlinks(expected)
	if err != nil || canonical != expected {
		return "", fmt.Errorf("registry source is missing or has symlink ancestors: %s", logical)
	}
	info, err := os.Lstat(expected)
	if err != nil || (directory && !info.IsDir()) || (!directory && !info.Mode().IsRegular()) {
		return "", fmt.Errorf("registry source has an unsupported file type: %s", logical)
	}
	return expected, nil
}

func registryHashFile(filename string, maximum int64, buffer []byte) (registryFileHash, error) {
	var result registryFileHash
	file, err := os.Open(filename)
	if err != nil {
		return result, err
	}
	defer file.Close()
	before, err := file.Stat()
	if err != nil || !before.Mode().IsRegular() || before.Size() > maximum {
		return result, errors.New("source file exceeds registry bounds")
	}
	hasher := xxh3.New()
	n, err := io.CopyBuffer(hasher, io.LimitReader(file, maximum+1), buffer)
	if err != nil || n != before.Size() {
		return result, errors.New("source file changed or failed while hashing")
	}
	after, err := file.Stat()
	current, pathErr := os.Lstat(filename)
	if err != nil || pathErr != nil || !os.SameFile(before, current) || before.Size() != after.Size() ||
		before.ModTime() != after.ModTime() || before.ModTime() != current.ModTime() || before.Size() != current.Size() {
		return result, errors.New("source file changed while preparing its registry")
	}
	hash := hasher.Sum128()
	binary.LittleEndian.PutUint64(result.hash[:8], hash.Lo)
	binary.LittleEndian.PutUint64(result.hash[8:], hash.Hi)
	result.size = n
	result.ticks = uint64(before.ModTime().UnixNano()/100 + 621355968000000000)
	return result, nil
}

func projectRootFolder(guid []byte, logical, project string) []byte {
	// RootFolderProperties: GUID16, flags4 and three relative BlobString
	// offsets. The observed Assets root shares one empty string. Package
	// roots carry group, package name and physical directory instead.
	if logical == "Assets" {
		return append(bytes.Clone(guid), []byte{0, 0, 0, 0, 12, 0, 0, 0, 8, 0, 0, 0, 4, 0, 0, 0, 0}...)
	}
	data := make([]byte, 32)
	copy(data, guid)
	binary.LittleEndian.PutUint32(data[16:20], 1)
	for index, value := range []string{"Packages", strings.TrimPrefix(logical, "Packages/"), filepath.Join(project, filepath.FromSlash(logical))} {
		offset := 20 + 4*index
		binary.LittleEndian.PutUint32(data[offset:offset+4], uint32(len(data)-offset))
		data = append(data, []byte(value+"\x00")...)
	}
	return data
}

func registryMetaScalar(line string) (string, error) {
	value := strings.TrimSpace(line)
	if strings.HasPrefix(value, "\"") {
		return strconv.Unquote(value)
	}
	if strings.HasPrefix(value, "'") {
		if len(value) < 2 || value[len(value)-1] != '\'' {
			return "", errors.New("unterminated single-quoted importer metadata scalar")
		}
		return strings.ReplaceAll(value[1:len(value)-1], "''", "'"), nil
	}
	if strings.ContainsAny(value, "\x00\r\n") || strings.HasPrefix(value, "|") || strings.HasPrefix(value, ">") {
		return "", errors.New("unsupported importer metadata scalar")
	}
	return value, nil
}

func registryMetaProperties(meta []byte) (map[string][]byte, error) {
	properties := make(map[string][]byte)
	mono := false
	origin := false
	for _, line := range strings.Split(string(meta), "\n") {
		line = strings.TrimSuffix(line, "\r")
		if len(line) > 0 && line[0] != ' ' {
			mono = line == "MonoImporter:"
			origin = line == "AssetOrigin:"
		}
		// Cache only importer-level scalars with native property types;
		// primary importer still reads complete settings from the .meta.
		if !strings.HasPrefix(line, "  ") || strings.HasPrefix(line, "   ") {
			continue
		}
		parts := strings.SplitN(strings.TrimPrefix(line, "  "), ":", 2)
		if len(parts) != 2 {
			continue
		}
		name := parts[0]
		originField := origin && (name == "productId" || name == "uploadId" || name == "packageName" || name == "packageVersion")
		if name != "assetBundleName" && name != "assetBundleVariant" && !(mono && (name == "executionOrder" || name == "userData")) && !originField {
			continue
		}
		value, err := registryMetaScalar(parts[1])
		if err != nil {
			return nil, fmt.Errorf("%s: %w", name, err)
		}
		if (mono && name == "executionOrder") || (origin && (name == "productId" || name == "uploadId")) {
			order, err := strconv.ParseInt(value, 10, 32)
			if err != nil {
				return nil, fmt.Errorf("invalid native int32 %s: %w", name, err)
			}
			encoded := make([]byte, 4)
			binary.LittleEndian.PutUint32(encoded, uint32(order))
			if mono {
				name = "MonoImporter.executionOrder"
			}
			properties[name] = encoded
		} else {
			if mono && name == "userData" {
				name = "MonoImporter.userData"
			}
			properties[name] = []byte(value + "\x00")
		}
	}
	return properties, nil
}

// WriteProjectSourceRegistry publishes a new immutable SourceAssetDB from
// actual source files, without an Editor-produced Library. The caller must
// launch workers only after it returns. An existing database is never replaced;
// GUID/topology/content changes require a fresh context revision after stopping
// the import worker. Player and extracted native-asset caches can stay alive.
func WriteProjectSourceRegistry(input ProjectRegistryInput) (SourceRegistryReport, error) {
	var report SourceRegistryReport
	if input.UnityVersion == "" {
		input.UnityVersion = fixtureUnityVersion
	}
	if input.UnityVersion != fixtureUnityVersion || len(input.Assets) == 0 || len(input.Assets) > 100000 {
		return report, errors.New("registry requires the supported Unity version and a bounded nonempty source asset set")
	}
	project, err := projectRegistryContext(input.Project)
	if err != nil {
		return report, err
	}
	database := filepath.Join(project, "Library", "SourceAssetDB")
	if _, err := os.Lstat(database); !errors.Is(err, os.ErrNotExist) {
		return report, errors.New("refusing to replace an existing source database; use a new stopped context revision")
	}
	entries := make(map[string]*projectRegistryEntry)
	casePaths := make(map[string]string)
	fileHashes := make(map[string]registryFileHash)
	hashBuffer := make([]byte, 128*1024)
	var addEntry func(ProjectRegistryAsset) error
	addEntry = func(asset ProjectRegistryAsset) error {
		root, err := projectRegistryPath(asset.Path)
		if err != nil {
			return err
		}
		// Validate explicit mappings even if this logical path was already
		// discovered through another asset's parent chain.
		for _, supplied := range []struct{ path, expected string }{
			{asset.Source, filepath.Join(project, filepath.FromSlash(asset.Path))},
			{asset.Meta, filepath.Join(project, filepath.FromSlash(asset.Path)) + ".meta"},
		} {
			if supplied.path != "" {
				absolute, err := filepath.Abs(supplied.path)
				if err != nil || absolute != supplied.expected {
					return fmt.Errorf("registry path differs from worker-visible source: %s", asset.Path)
				}
			}
		}
		lower := strings.ToLower(asset.Path)
		if previous, ok := casePaths[lower]; ok && previous != asset.Path {
			return fmt.Errorf("case-insensitive source paths collide: %s and %s", previous, asset.Path)
		}
		if previous, ok := entries[asset.Path]; ok {
			if previous.directory != asset.Directory || (asset.GUID != "" && !strings.EqualFold(previous.guid, asset.GUID)) {
				return fmt.Errorf("conflicting source registry entry: %s", asset.Path)
			}
			return nil
		}
		filename, err := registryExpectedFile(project, asset.Path, asset.Source, asset.Directory)
		if err != nil {
			return err
		}
		entry := &projectRegistryEntry{path: asset.Path, directory: asset.Directory}
		metaName := filename + ".meta"
		if asset.Path == "Assets" {
			entry.wireGUID = make([]byte, 16)
			entry.wireGUID[8] = 1
			entry.guid = registryGUIDText(entry.wireGUID)
		} else if _, err := os.Lstat(metaName); errors.Is(err, os.ErrNotExist) && asset.Directory && asset.Meta == "" {
			// Missing parent folder metas must not mutate original content.
			// Their stable synthetic GUIDs are private to this coordinator.
			hash := nativeSourceHash([]byte("unity-worker-directory-v1\x00" + asset.Path))
			entry.wireGUID, entry.guid = bytes.Clone(hash[:]), registryGUIDText(hash[:])
		} else {
			if _, err := registryExpectedFile(project, asset.Path+".meta", asset.Meta, false); err != nil {
				return err
			}
			digest, err := registryHashFile(metaName, 16*1024*1024, hashBuffer)
			if err != nil {
				return err
			}
			meta, err := os.ReadFile(metaName)
			if err != nil || nativeSourceHash(meta) != digest.hash {
				return errors.New("source .meta changed while preparing registry")
			}
			entry.meta = meta
			entry.guid, err = registryMetaGUID(meta)
			if err != nil {
				return fmt.Errorf("%s: %w", asset.Path, err)
			}
			entry.wireGUID, err = sourceRegistryGUID(entry.guid)
			if err != nil {
				return err
			}
			fileHashes[asset.Path+".meta"] = digest
		}
		if asset.GUID != "" && !strings.EqualFold(asset.GUID, entry.guid) {
			return fmt.Errorf("source registry GUID override differs from original .meta: %s", asset.Path)
		}
		if !asset.Directory {
			digest, err := registryHashFile(filename, 1024*1024*1024, hashBuffer)
			if err != nil {
				return fmt.Errorf("%s: %w", asset.Path, err)
			}
			fileHashes[asset.Path] = digest
		}
		entries[asset.Path], casePaths[lower] = entry, asset.Path
		if asset.Path != root {
			return addEntry(ProjectRegistryAsset{Path: path.Dir(asset.Path), Directory: true})
		}
		return nil
	}
	for _, asset := range input.Assets {
		if strings.HasSuffix(asset.Path, ".meta") {
			return report, errors.New("provide assets rather than separate .meta registry entries")
		}
		if err := addEntry(asset); err != nil {
			return report, err
		}
	}
	tables := map[string][]LMDBRecord{
		"DBVersion":        {{Key: []byte("Version\x00"), Value: []byte{0xe1, 0xdc, 0xee, 0x6c}}},
		"PropertyIDToType": sourceRegistryTypes(), "GuidPropertyIDToProperty": {}, "GuidToChildren": {},
		"GuidToIsDir": {}, "GuidToPath": {}, "Misc": {}, "PathToGuid": {}, "RootFolders": {}, "SerializedSingletons": {}, "hash": {},
	}
	var names []string
	for name := range entries {
		names = append(names, name)
	}
	sort.Strings(names)
	guidPaths := make(map[string]string)
	children := make(map[string][]byte)
	addProperty := func(entry *projectRegistryEntry, name string, value []byte) {
		tables["GuidPropertyIDToProperty"] = append(tables["GuidPropertyIDToProperty"], LMDBRecord{
			Key: append([]byte(name), entry.wireGUID...), Value: value})
	}
	for _, name := range names {
		entry := entries[name]
		if previous, ok := guidPaths[entry.guid]; ok {
			return report, fmt.Errorf("source GUID shared by two paths: %s and %s", previous, name)
		}
		guidPaths[entry.guid] = name
		directory := byte(0)
		if entry.directory {
			directory = 1
		}
		tables["GuidToPath"] = append(tables["GuidToPath"], LMDBRecord{Key: entry.wireGUID, Value: []byte(name + "\x00")})
		tables["GuidToIsDir"] = append(tables["GuidToIsDir"], LMDBRecord{Key: entry.wireGUID, Value: []byte{directory}})
		mapping := make([]byte, 48)
		copy(mapping, entry.wireGUID)
		meta, source := fileHashes[name+".meta"], fileHashes[name]
		copy(mapping[16:32], meta.hash[:])
		copy(mapping[32:48], source.hash[:])
		tables["PathToGuid"] = append(tables["PathToGuid"], LMDBRecord{Key: []byte(strings.ToLower(name) + "\x00"), Value: mapping})
		addProperty(entry, "assetPath", []byte(name+"\x00"))
		properties, err := registryMetaProperties(entry.meta)
		if err != nil {
			return report, fmt.Errorf("%s: %w", name, err)
		}
		for property, value := range properties {
			addProperty(entry, property, value)
		}
		root, _ := projectRegistryPath(name)
		if name == root {
			tables["RootFolders"] = append(tables["RootFolders"], LMDBRecord{Key: []byte(root + "\x00"), Value: projectRootFolder(entry.wireGUID, root, project)})
		} else {
			parent := path.Dir(name)
			children[parent] = append(children[parent], entry.wireGUID...)
		}
	}
	for name, values := range children {
		tables["GuidToChildren"] = append(tables["GuidToChildren"], LMDBRecord{Key: entries[name].wireGUID, Value: values})
	}
	for name, digest := range fileHashes {
		value := make([]byte, 40)
		copy(value, digest.hash[:])
		binary.LittleEndian.PutUint64(value[16:24], digest.ticks)
		binary.LittleEndian.PutUint64(value[24:32], uint64(digest.size))
		// hash16, year-1 ticks8, size8, untrustedTimestamp=false, padding7.
		tables["hash"] = append(tables["hash"], LMDBRecord{Key: []byte(strings.ToLower(name) + "\x00"), Value: value})
	}
	data, err := buildProjectLMDB(tables)
	if err != nil {
		return report, err
	}
	temporary, err := os.CreateTemp(filepath.Dir(database), ".source-registry-*")
	if err != nil {
		return report, err
	}
	defer os.Remove(temporary.Name())
	if _, err := temporary.Write(data); err != nil {
		temporary.Close()
		return report, err
	}
	if err := temporary.Sync(); err != nil {
		temporary.Close()
		return report, err
	}
	if err := temporary.Close(); err != nil {
		return report, err
	}
	// Hard-link publishes atomically without ever replacing an existing
	// mapped file, even if another coordinator wins the creation race.
	if err := os.Link(temporary.Name(), database); err != nil {
		return report, fmt.Errorf("publish new immutable source database: %w", err)
	}
	report = SourceRegistryReport{Database: database, SHA256: bytesSHA(data), UnityVersion: input.UnityVersion,
		GUIDCount: len(entries), GUIDPaths: guidPaths, TableCounts: make(map[string]int),
		SourceHashAlgorithm: "XXH3-128 low64/high64 little endian", FileHashes: make(map[string]string)}
	for name, records := range tables {
		report.TableCounts[name] = len(records)
	}
	for name, digest := range fileHashes {
		report.FileHashes[name] = hex.EncodeToString(digest.hash[:])
		report.SourceBytes += digest.size
		report.SourceFileCount++
	}
	return report, nil
}
