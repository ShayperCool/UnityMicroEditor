package main

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
)

type goScriptTypeCache struct {
	SchemaVersion int    `json:"schemaVersion"`
	Key           string `json:"key"`
	TypesSHA256   string `json:"typesSHA256"`
}

func (compiler *goCompiler) scriptTypeCacheKey(helper string, assemblies map[string]string) (string, error) {
	inputs := map[string]string{}
	for name, path := range assemblies {
		hash, err := goFileHash(path)
		if err != nil {
			return "", err
		}
		inputs[name] = hash
	}
	var helperFiles map[string]string
	if err := readJSONFile(filepath.Join(filepath.Dir(helper), "files.json"), &helperFiles); err != nil ||
		helperFiles["LifecycleCompiler.dll"] == "" || !verifyGoCacheFiles(filepath.Dir(helper), helperFiles) {
		return "", errors.New("script inventory SDK helper payload is not verified")
	}
	identity := map[string]any{"mode": "go-script-types-v1", "assemblies": inputs, "helper": helperFiles,
		"scripts": compiler.manifest.Scripts, "expectedTypes": compiler.expectedTypes}
	encoded, err := json.Marshal(identity)
	if err != nil {
		return "", err
	}
	return bytesSHA(encoded), nil
}

func (compiler *goCompiler) readScriptTypeCache(key string) map[string]string {
	directory := filepath.Join(compiler.cache, "script-types", key)
	canonical, err := filepath.EvalSymlinks(directory)
	if err != nil || canonical != directory {
		return nil
	}
	var entry goScriptTypeCache
	if readJSONFile(filepath.Join(directory, "artifact.json"), &entry) != nil || entry.SchemaVersion != 1 || entry.Key != key || entry.TypesSHA256 == "" {
		return nil
	}
	filename := filepath.Join(directory, "types.json")
	info, err := os.Lstat(filename)
	if err != nil || !info.Mode().IsRegular() || info.Size() > 2*1024*1024 {
		return nil
	}
	data, err := os.ReadFile(filename)
	if err != nil || bytesSHA(data) != entry.TypesSHA256 {
		return nil
	}
	var result map[string]string
	if json.Unmarshal(data, &result) != nil || result == nil {
		return nil
	}
	known := map[string]bool{}
	for _, script := range compiler.manifest.Scripts {
		known[script.GUID] = script.GUID != ""
	}
	for guid, name := range result {
		if !known[guid] || name == "" || len(name) > 1024 || strings.ContainsAny(name, "\x00\r\n") {
			return nil
		}
	}
	return result
}

func (compiler *goCompiler) saveScriptTypeCache(key string, types map[string]string) error {
	directory := filepath.Join(compiler.cache, "script-types", key)
	if err := os.MkdirAll(directory, 0700); err != nil {
		return err
	}
	if err := atomicJSON(filepath.Join(directory, "types.json"), types); err != nil {
		return err
	}
	hash, err := goFileHash(filepath.Join(directory, "types.json"))
	if err != nil {
		return err
	}
	return atomicJSON(filepath.Join(directory, "artifact.json"), goScriptTypeCache{SchemaVersion: 1, Key: key, TypesSHA256: hash})
}
