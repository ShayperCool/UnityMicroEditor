package main

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math/big"
	"os"
	"path/filepath"
	"reflect"
)

const sceneRootsClass = 1660057539

type patchScene struct {
	fields   map[string]json.RawMessage
	revision string
	objects  []map[string]any
	byID     map[string]map[string]any
	data     map[string]map[string]any
}

// makeScenePatch preserves unrelated runtime state. Script/component changes,
// removals, reparenting and changed native content use the complete loader.
func makeScenePatch(previous, next string) (string, string, error) {
	old, err := readPatchScene(previous)
	if err != nil {
		return "", "", err
	}
	fresh, err := readPatchScene(next)
	if err != nil {
		return "", "", err
	}
	if old.revision == fresh.revision {
		return "", "", errors.New("scene patch requires a new revision")
	}
	oldDependencies, err := patchSceneDependencies(old.fields["sourceDependencies"])
	if err != nil {
		return "", "", err
	}
	newDependencies, err := patchSceneDependencies(fresh.fields["sourceDependencies"])
	if err != nil {
		return "", "", err
	}
	if !reflect.DeepEqual(oldDependencies, newDependencies) {
		return "", "", nil
	}
	ignored := map[string]bool{"objects": true, "revision": true, "sourceDependencies": true, "contentHash": true, "manifest": true}
	for key, value := range old.fields {
		if !ignored[key] && !jsonEqual(value, fresh.fields[key]) {
			return "", "", nil
		}
	}
	for key, value := range fresh.fields {
		if !ignored[key] && !jsonEqual(value, old.fields[key]) {
			return "", "", nil
		}
	}
	for id := range old.byID {
		if fresh.byID[id] == nil {
			return "", "", nil
		}
	}
	if err := validatePatchGraph(fresh); err != nil {
		return "", "", err
	}
	changed := []map[string]any{}
	for _, record := range fresh.objects {
		id := record["fileId"].(string)
		before := old.byID[id]
		data := fresh.data[id]
		if before != nil && patchRecordEqual(before, record, old.data[id], data) {
			continue
		}
		class := patchInteger(record["classId"])
		if class != 1 && class != 4 && class != sceneRootsClass {
			return "", "", nil
		}
		if before != nil {
			for _, field := range []string{"classId", "gameObjectId", "sourcePath", "guid", "typeName", "skipMissingScript"} {
				if !patchValueEqual(before[field], record[field]) {
					return "", "", nil
				}
			}
			allowed := map[string]bool{"m_Name": true, "m_IsActive": true, "m_Layer": true, "m_TagString": true}
			if class == 4 {
				allowed = map[string]bool{"m_LocalPosition": true, "m_LocalRotation": true, "m_LocalScale": true, "m_LocalEulerAnglesHint": true, "m_Children": true}
			} else if class == sceneRootsClass {
				allowed = map[string]bool{"m_Roots": true}
				// Native Player SetSiblingIndex does not reorder scene roots.
				// Appending new roots preserves enumeration; insertion/reordering
				// needs reconstruction in the new authored creation order.
				previousRoots, _ := old.data[id]["m_Roots"].([]any)
				nextRoots, _ := data["m_Roots"].([]any)
				if len(nextRoots) < len(previousRoots) {
					return "", "", nil
				}
				for index, root := range previousRoots {
					if !patchValueEqual(root, nextRoots[index]) {
						return "", "", nil
					}
				}
			}
			if !patchMapsEqualExcept(old.data[id], data, allowed) {
				return "", "", nil
			}
		} else if class == 1 {
			components, ok := data["m_Component"].([]any)
			if !ok || len(components) != 1 {
				return "", "", nil
			}
			component := patchReference(patchMap(components[0])["component"])
			transform := fresh.byID[component]
			if old.byID[component] != nil || transform == nil || patchInteger(transform["classId"]) != 4 || transform["gameObjectId"] != id {
				return "", "", nil
			}
		} else if class == 4 {
			owner, _ := record["gameObjectId"].(string)
			if old.byID[owner] != nil || fresh.byID[owner] == nil || patchInteger(fresh.byID[owner]["classId"]) != 1 {
				return "", "", nil
			}
		}
		changed = append(changed, record)
	}
	if len(changed) == 0 {
		return "", "", nil
	}
	patch := map[string]any{"schemaVersion": 1, "baseRevision": old.revision, "revision": fresh.revision, "objects": changed}
	payload, err := json.Marshal(patch)
	if err != nil {
		return "", "", err
	}
	if len(payload) > 2<<20 {
		return "", "", nil
	}
	path := filepath.Join(filepath.Dir(next), "patch.json")
	if err := atomicJSON(path, patch); err != nil {
		return "", "", err
	}
	content, err := os.ReadFile(path)
	if err != nil {
		return "", "", err
	}
	return path, bytesSHA(content), nil
}
func patchSceneDependencies(data json.RawMessage) (any, error) {
	if len(data) == 0 {
		return nil, nil
	}
	var value any
	if err := decodePatchJSON(data, &value); err != nil {
		return nil, err
	}
	normalizeSceneDependencies(value)
	return value, nil
}
func readPatchScene(path string) (patchScene, error) {
	var result patchScene
	if err := readJSONFile(path, &result.fields); err != nil {
		return result, err
	}
	if err := json.Unmarshal(result.fields["revision"], &result.revision); err != nil || result.revision == "" {
		return result, errors.New("scene revision is missing")
	}
	if err := decodePatchJSON(result.fields["objects"], &result.objects); err != nil || len(result.objects) == 0 {
		return result, errors.New("scene objects are missing or malformed")
	}
	result.byID = map[string]map[string]any{}
	result.data = map[string]map[string]any{}
	for _, record := range result.objects {
		id, ok := record["fileId"].(string)
		if !ok || id == "" || result.byID[id] != nil {
			return result, errors.New("scene object identities must be unique nonempty strings")
		}
		if patchInteger(record["classId"]) <= 0 {
			return result, fmt.Errorf("object %s classId is invalid", id)
		}
		text, ok := record["dataJson"].(string)
		if !ok {
			return result, fmt.Errorf("object %s dataJson is invalid", id)
		}
		var data map[string]any
		if err := decodePatchJSON([]byte(text), &data); err != nil || data == nil {
			return result, fmt.Errorf("object %s dataJson must be a map", id)
		}
		result.byID[id], result.data[id] = record, data
	}
	return result, nil
}
func decodePatchJSON(data []byte, value any) error {
	d := json.NewDecoder(bytes.NewReader(data))
	d.UseNumber()
	if err := d.Decode(value); err != nil {
		return err
	}
	var trailing any
	if err := d.Decode(&trailing); !errors.Is(err, io.EOF) {
		return errors.New("trailing JSON data")
	}
	return nil
}
func patchInteger(value any) int64 {
	n, ok := value.(json.Number)
	if !ok {
		return -1
	}
	r, ok := new(big.Rat).SetString(string(n))
	if !ok || !r.IsInt() || !r.Num().IsInt64() {
		return -1
	}
	return r.Num().Int64()
}
func patchMap(value any) map[string]any { m, _ := value.(map[string]any); return m }
func patchReference(value any) string   { id, _ := patchMap(value)["fileID"].(string); return id }
func validatePatchGraph(scene patchScene) error {
	transforms := map[string]map[string]any{}
	rootIDs := map[string]bool{}
	var rootList []any
	rootMetadata := 0
	for id, record := range scene.byID {
		class := patchInteger(record["classId"])
		data := scene.data[id]
		if class == 4 || class == 224 {
			transforms[id] = data
			parent := patchReference(data["m_Father"])
			if parent == "" {
				return fmt.Errorf("Transform %s parent descriptor is missing", id)
			}
			if parent == "0" {
				rootIDs[id] = true
			}
			owner, _ := record["gameObjectId"].(string)
			if scene.byID[owner] == nil || patchInteger(scene.byID[owner]["classId"]) != 1 || patchReference(data["m_GameObject"]) != owner {
				return fmt.Errorf("Transform %s GameObject identity differs", id)
			}
		}
		if class == 1 {
			if layer := patchInteger(data["m_Layer"]); data["m_Layer"] != nil && (layer < 0 || layer > 31) {
				return fmt.Errorf("GameObject %s layer is invalid", id)
			}
			if active := patchInteger(data["m_IsActive"]); data["m_IsActive"] != nil && active != 0 && active != 1 {
				return fmt.Errorf("GameObject %s active state is invalid", id)
			}
		}
		if class == sceneRootsClass {
			rootMetadata++
			var ok bool
			rootList, ok = data["m_Roots"].([]any)
			if !ok {
				return errors.New("SceneRoots list is missing")
			}
		}
	}
	listedChildren := map[string]string{}
	for id, data := range transforms {
		parent := patchReference(data["m_Father"])
		if parent != "0" && transforms[parent] == nil {
			return fmt.Errorf("Transform %s parent is missing", id)
		}
		children, ok := data["m_Children"].([]any)
		if !ok {
			return fmt.Errorf("Transform %s child list is missing", id)
		}
		seen := map[string]bool{}
		for _, ref := range children {
			child := patchReference(ref)
			if child == "" || seen[child] || transforms[child] == nil || patchReference(transforms[child]["m_Father"]) != id {
				return fmt.Errorf("Transform %s child/parent records disagree", id)
			}
			seen[child] = true
			listedChildren[child] = id
		}
		visited := map[string]bool{id: true}
		for parent != "0" {
			if visited[parent] {
				return errors.New("Transform cycle")
			}
			visited[parent] = true
			parent = patchReference(transforms[parent]["m_Father"])
		}
	}
	for id, data := range transforms {
		parent := patchReference(data["m_Father"])
		if parent != "0" && listedChildren[id] != parent {
			return fmt.Errorf("Transform %s is absent from its parent's child list", id)
		}
	}
	if rootMetadata > 1 {
		return errors.New("multiple SceneRoots records")
	}
	if rootMetadata == 1 {
		seen := map[string]bool{}
		for _, ref := range rootList {
			if guid, _ := patchMap(ref)["guid"].(string); guid != "" {
				return errors.New("SceneRoots requires local authored references")
			}
			id := patchReference(ref)
			if !rootIDs[id] || seen[id] {
				return errors.New("SceneRoots list does not match root Transforms")
			}
			seen[id] = true
		}
		if len(seen) != len(rootIDs) {
			return errors.New("SceneRoots omits a root Transform")
		}
	}
	return nil
}
func patchRecordEqual(old, next, oldData, nextData map[string]any) bool {
	ignored := map[string]bool{"dataJson": true, "scalarDataJson": true, "references": true}
	return patchMapsEqualExcept(old, next, ignored) && patchValueEqual(oldData, nextData)
}
func patchMapsEqualExcept(old, next map[string]any, ignored map[string]bool) bool {
	for k, v := range old {
		if !ignored[k] && !patchValueEqual(v, next[k]) {
			return false
		}
	}
	for k, v := range next {
		if !ignored[k] && !patchValueEqual(v, old[k]) {
			return false
		}
	}
	return true
}

type patchNumber struct{ Value string }

func patchNormalize(value any) any {
	switch v := value.(type) {
	case json.Number:
		if n, ok := new(big.Rat).SetString(string(v)); ok {
			return patchNumber{n.RatString()}
		}
		return v
	case map[string]any:
		r := map[string]any{}
		for k, item := range v {
			r[k] = patchNormalize(item)
		}
		return r
	case []any:
		r := make([]any, len(v))
		for i, item := range v {
			r[i] = patchNormalize(item)
		}
		return r
	default:
		return value
	}
}
func patchValueEqual(old, next any) bool {
	return reflect.DeepEqual(patchNormalize(old), patchNormalize(next))
}
func jsonEqual(old, next []byte) bool {
	if len(old) == 0 || len(next) == 0 {
		return len(old) == len(next)
	}
	var a, b any
	if decodePatchJSON(old, &a) != nil || decodePatchJSON(next, &b) != nil {
		return false
	}
	return patchValueEqual(a, b)
}
