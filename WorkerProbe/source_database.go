package main

import (
	"bytes"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/zeebo/xxh3"
)

type SourceRegistryReport struct {
	Database            string            `json:"database"`
	SHA256              string            `json:"sha256"`
	UnityVersion        string            `json:"unityVersion"`
	GUIDCount           int               `json:"guidCount"`
	TableCounts         map[string]int    `json:"tableCounts"`
	SourceHashAlgorithm string            `json:"sourceHashAlgorithm"`
	FileHashes          map[string]string `json:"fileHashes"`
	GUIDPaths           map[string]string `json:"guidPaths,omitempty"`
	SourceBytes         int64             `json:"sourceBytes,omitempty"`
	SourceFileCount     int               `json:"sourceFileCount,omitempty"`
}

func nativeSourceHash(data []byte) [16]byte {
	hash := xxh3.Hash128(data)
	var output [16]byte
	binary.LittleEndian.PutUint64(output[:8], hash.Lo)
	binary.LittleEndian.PutUint64(output[8:], hash.Hi)
	return output
}

func sourceRegistryTypes() []LMDBRecord {
	types := map[string]string{
		"AssetBundleIndex": "i32", "ImportLogEntriesCount": "pair<i32,i32>",
		"MainObjectLocalIdentifierInFile": "i64", "MonoImporter.defaultReferences": "pair<str,pptr>[]",
		"MonoImporter.executionOrder": "i32", "MonoImporter.icon": "pptr", "MonoImporter.userData": "str",
		"assetBundleName": "str", "assetBundleVariant": "str", "assetPath": "str",
		"assetReqImport": "bool", "coreAssetReqImport": "bool", "importLogFile": "hash128",
		"importerErrorFile": "hash128", "importerOverride": "imp", "labels": "str[]",
		"managedPlugin": "bool", "nativePluginPath": "str", "packageName": "str", "packageVersion": "str",
		"productId": "i32", "scriptCompilationAssetPath": "str", "uploadId": "i32",
	}
	for index := 0; index < 32; index++ {
		types[fmt.Sprintf("ArtifactFilePropDef%d", index)] = "hash128"
	}
	var records []LMDBRecord
	for name, kind := range types {
		records = append(records, LMDBRecord{Key: []byte(name + "\x00"), Value: []byte(kind + "\x00")})
	}
	return records
}

func sourceRegistryGUID(text string) ([]byte, error) {
	address, err := importAddress(text, 0, [16]byte{})
	if err != nil {
		return nil, err
	}
	return bytes.Clone(address[:16]), nil
}

func validateRegistryFixture(fixture Fixture) (string, error) {
	if fixture.SchemaVersion != 1 || fixture.UnityVersion != fixtureUnityVersion ||
		fixture.ModelAsset != modelAsset || fixture.AudioAsset != audioAsset || fixture.AudioGUID != audioGUID || fixture.ReferenceIncluded {
		return "", errors.New("source registry only supports the version-pinned worker fixture")
	}
	project, err := filepath.Abs(fixture.Project)
	if err != nil {
		return "", err
	}
	canonical, err := filepath.EvalSymlinks(project)
	if err != nil {
		return "", err
	}
	if canonical != project || filepath.Base(project) != "project" {
		return "", errors.New("source registry requires an owned fixture directory without symlink ancestors")
	}
	if !strings.Contains(project, string(filepath.Separator)+"Temp"+string(filepath.Separator)) {
		return "", errors.New("source registry fixture must be under a Temp directory")
	}
	encoded, err := os.ReadFile(filepath.Join(filepath.Dir(project), "fixture.json"))
	if err != nil {
		return "", fmt.Errorf("source registry fixture ownership: %w", err)
	}
	var recorded Fixture
	if err := json.Unmarshal(encoded, &recorded); err != nil {
		return "", err
	}
	actual, _ := json.Marshal(fixture)
	expected, _ := json.Marshal(recorded)
	if !bytes.Equal(actual, expected) {
		return "", errors.New("source registry input differs from its fixture manifest")
	}
	if len(fixture.InputHashes) != 6 {
		return "", errors.New("source registry fixture input set changed")
	}
	for name, hash := range fixture.InputHashes {
		if name != modelAsset && name != modelAsset+".meta" && name != audioAsset && name != audioAsset+".meta" &&
			name != "ProjectSettings/ProjectVersion.txt" && name != "Packages/manifest.json" {
			return "", errors.New("source registry has an unexpected fixture input")
		}
		path := filepath.Join(project, filepath.FromSlash(name))
		canonicalInput, err := filepath.EvalSymlinks(path)
		if err != nil || canonicalInput != path {
			return "", fmt.Errorf("source registry input has a symlink ancestor: %s", name)
		}
		info, err := os.Lstat(path)
		if err != nil || !info.Mode().IsRegular() || info.Size() > 64*1024*1024 {
			return "", fmt.Errorf("source registry input is not a bounded regular file: %s", name)
		}
		data, err := os.ReadFile(path)
		if err != nil {
			return "", err
		}
		if bytesSHA(data) != hash {
			return "", fmt.Errorf("source registry fixture input changed: %s", name)
		}
	}
	return project, nil
}

// WriteSourceRegistry creates the minimal native source database for our two
// fixtures. It never copies an Editor-generated Library. An existing registry
// must be the blank database created by a stopped worker. Callers must stop all
// workers before invoking this function; it is not an online LMDB writer.
func WriteSourceRegistry(fixture Fixture) (SourceRegistryReport, error) {
	var report SourceRegistryReport
	project, err := validateRegistryFixture(fixture)
	if err != nil {
		return report, err
	}
	path := filepath.Join(project, "Library", "SourceAssetDB")
	canonicalLibrary, err := filepath.EvalSymlinks(filepath.Dir(path))
	if err != nil || canonicalLibrary != filepath.Dir(path) {
		return report, errors.New("source registry Library must be an owned directory without symlink ancestors")
	}
	var previous os.FileInfo
	if info, err := os.Lstat(path); err == nil {
		if !info.Mode().IsRegular() || info.Size() > 64*1024 {
			return report, errors.New("refusing to replace a source database outside blank fixture bounds")
		}
		guidRecords, err := ReadLMDBTable(path, "GuidToPath")
		if err != nil || len(guidRecords) != 0 {
			return report, errors.New("refusing to replace a source database with registered assets")
		}
		previous = info
	} else if !errors.Is(err, os.ErrNotExist) {
		return report, err
	}
	tables := map[string][]LMDBRecord{
		"DBVersion":                {{Key: []byte("Version\x00"), Value: []byte{0xe1, 0xdc, 0xee, 0x6c}}},
		"PropertyIDToType":         sourceRegistryTypes(),
		"GuidPropertyIDToProperty": {}, "GuidToChildren": {}, "GuidToIsDir": {}, "GuidToPath": {},
		"Misc": {}, "PathToGuid": {}, "RootFolders": {}, "SerializedSingletons": {}, "hash": {},
	}
	modelGUID, err := sourceRegistryGUID(fixture.ModelGUID)
	if err != nil {
		return report, err
	}
	audioGUID, err := sourceRegistryGUID(fixture.AudioGUID)
	if err != nil {
		return report, err
	}
	assetsGUID := make([]byte, 16)
	assetsGUID[8] = 1 // Native UnityGUID constant for the Assets root folder.
	probeGUID, err := sourceRegistryGUID("e02c15f38b174000b2bac61a5e7a0701")
	if err != nil {
		return report, err
	}
	fileHashes := make(map[string][16]byte)
	for _, name := range []string{modelAsset, modelAsset + ".meta", audioAsset, audioAsset + ".meta"} {
		file := filepath.Join(project, filepath.FromSlash(name))
		data, err := os.ReadFile(file)
		if err != nil {
			return report, err
		}
		if bytesSHA(data) != fixture.InputHashes[name] {
			return report, fmt.Errorf("source registry fixture changed while preparing: %s", name)
		}
		hash := nativeSourceHash(data)
		fileHashes[name] = hash
		info, err := os.Stat(file)
		if err != nil {
			return report, err
		}
		value := make([]byte, 40)
		copy(value[:16], hash[:])
		// Native layout is hash16, DateTime8, size8, untrustedTimestamp1,
		// padding7. DateTime stores 100 ns ticks since year 1. This fresh
		// digest was checked against the fixture manifest, so the flag is 0.
		ticks := info.ModTime().UnixNano()/100 + 621355968000000000
		binary.LittleEndian.PutUint64(value[16:24], uint64(ticks))
		binary.LittleEndian.PutUint64(value[24:32], uint64(info.Size()))
		tables["hash"] = append(tables["hash"], LMDBRecord{Key: []byte(strings.ToLower(name) + "\x00"), Value: value})
	}
	entries := []struct {
		path      string
		guid      []byte
		directory bool
	}{{"Assets", assetsGUID, true}, {"Assets/Probe", probeGUID, true},
		{modelAsset, modelGUID, false}, {audioAsset, audioGUID, false}}
	for _, entry := range entries {
		directory := byte(0)
		if entry.directory {
			directory = 1
		}
		tables["GuidToPath"] = append(tables["GuidToPath"], LMDBRecord{Key: entry.guid, Value: []byte(entry.path + "\x00")})
		tables["GuidToIsDir"] = append(tables["GuidToIsDir"], LMDBRecord{Key: entry.guid, Value: []byte{directory}})
		mapping := make([]byte, 48)
		copy(mapping[:16], entry.guid)
		metaHash, sourceHash := fileHashes[entry.path+".meta"], fileHashes[entry.path]
		copy(mapping[16:32], metaHash[:])
		copy(mapping[32:48], sourceHash[:])
		tables["PathToGuid"] = append(tables["PathToGuid"], LMDBRecord{Key: []byte(strings.ToLower(entry.path) + "\x00"), Value: mapping})
		propertyKey := append([]byte("assetPath"), entry.guid...)
		tables["GuidPropertyIDToProperty"] = append(tables["GuidPropertyIDToProperty"], LMDBRecord{Key: propertyKey, Value: []byte(entry.path + "\x00")})
	}
	tables["GuidToChildren"] = []LMDBRecord{{Key: assetsGUID, Value: probeGUID},
		{Key: probeGUID, Value: append(bytes.Clone(modelGUID), audioGUID...)}}
	root := append(bytes.Clone(assetsGUID), []byte{0, 0, 0, 0, 12, 0, 0, 0, 8, 0, 0, 0, 4, 0, 0, 0, 0}...)
	tables["RootFolders"] = []LMDBRecord{{Key: []byte("Assets\x00"), Value: root}}
	data, err := buildFixtureLMDB(tables)
	if err != nil {
		return report, err
	}
	temporary, err := os.CreateTemp(filepath.Dir(path), ".source-registry-*")
	if err != nil {
		return report, err
	}
	name := temporary.Name()
	defer os.Remove(name)
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
	// Guard against another process replacing the blank registry during our
	// offline preparation. Never overwrite a populated source database.
	current, err := os.Lstat(path)
	if previous == nil {
		if !errors.Is(err, os.ErrNotExist) {
			return report, errors.New("source database appeared while preparing registry")
		}
	} else if err != nil || !os.SameFile(previous, current) || previous.ModTime() != current.ModTime() || previous.Size() != current.Size() {
		return report, errors.New("source database changed while preparing registry")
	}
	if err := os.Rename(name, path); err != nil {
		return report, err
	}
	report = SourceRegistryReport{Database: path, SHA256: bytesSHA(data), UnityVersion: fixtureUnityVersion,
		GUIDCount: len(entries), TableCounts: make(map[string]int), SourceHashAlgorithm: "XXH3-128 low64/high64 little endian",
		FileHashes: make(map[string]string)}
	for table, records := range tables {
		report.TableCounts[table] = len(records)
	}
	for file, hash := range fileHashes {
		report.FileHashes[file] = hex.EncodeToString(hash[:])
	}
	return report, nil
}
