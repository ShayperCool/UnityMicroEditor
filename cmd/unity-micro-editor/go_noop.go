package main

import (
	"bytes"
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"
	"strings"
)

// Metadata-only writes must not reset a running scene. Source hashes remain
// validated by Prepare; the Player only needs a reload for changed runtime data.
func sceneRuntimeEqual(previous, next string) (bool, error) {
	decode := func(path string) (map[string]any, error) {
		raw, err := os.ReadFile(path)
		if err != nil {
			return nil, err
		}
		var doc map[string]json.RawMessage
		if err = json.Unmarshal(raw, &doc); err != nil {
			return nil, err
		}
		out := make(map[string]any)
		for key, data := range doc {
			switch key {
			case "revision", "manifest", "contentHash", "diagnostics":
				continue
			}
			var value any
			decoder := json.NewDecoder(bytes.NewReader(data))
			decoder.UseNumber()
			if err = decoder.Decode(&value); err != nil {
				return nil, err
			}
			if key == "sourceDependencies" {
				normalizeSceneDependencies(value)
			}
			out[key] = value
		}
		return out, nil
	}
	a, err := decode(previous)
	if err != nil {
		return false, err
	}
	b, err := decode(next)
	if err != nil {
		return false, err
	}
	return reflect.DeepEqual(a, b), nil
}

// Authored scenes and prefabs are fully represented in the runtime graph. Other
// dependencies can hold file-backed content that a scene patch cannot reload.
func normalizeSceneDependencies(value any) {
	if dependencies, ok := value.([]any); ok {
		for _, item := range dependencies {
			if dependency, ok := item.(map[string]any); ok {
				path, _ := dependency["path"].(string)
				switch strings.ToLower(filepath.Ext(path)) {
				case ".unity", ".prefab":
					delete(dependency, "sha256")
					delete(dependency, "bytes")
				}
			}
		}
	}
}
