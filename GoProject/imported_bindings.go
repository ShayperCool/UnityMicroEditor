package goproject

import (
	"fmt"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
)

type assetPreparation struct {
	Assets           []any
	ImportedBindings []any
}

type importedBindingClosure struct {
	session  *Session
	byOwner  map[string][]map[string]any
	selected map[string]map[string]any
}

func (s *Session) importedBindingClosure() *importedBindingClosure {
	bindings := s.catalog.DeferredBindings
	if s.catalog.SchemaVersion == 0 && len(bindings) == 0 {
		for _, value := range array(s.baseline["importedAssetBindings"]) {
			bindings = append(bindings, object(value))
		}
	}
	closure := &importedBindingClosure{session: s, byOwner: map[string][]map[string]any{}, selected: map[string]map[string]any{}}
	for _, binding := range bindings {
		key := assetKey(str(binding["ownerGuid"]), str(binding["ownerFileId"]))
		closure.byOwner[key] = append(closure.byOwner[key], binding)
	}
	return closure
}

var importedBindingProperty = regexp.MustCompile(`^[A-Za-z_][A-Za-z0-9_]*(?:(?:\.[A-Za-z_][A-Za-z0-9_]*)|(?:\[[0-9]+\]))*$`)

func (closure *importedBindingClosure) queue(owner map[string]any, pending *assetReferenceWorklist) error {
	if str(owner["kind"]) != "imported" {
		return nil
	}
	ownerGUID, ownerID := str(owner["guid"]), str(owner["fileId"])
	for _, binding := range closure.byOwner[assetKey(ownerGUID, ownerID)] {
		property := str(binding["propertyPath"])
		if !validNativeGUID(ownerGUID) || !validNativeID(ownerID) || !importedBindingProperty.MatchString(property) || number(owner["classId"]) != 114 {
			return fmt.Errorf("imported binding owner/property is invalid: %s/%s.%s", ownerGUID, ownerID, property)
		}
		targetGUID, targetID, targetPath := str(binding["targetGuid"]), str(binding["targetFileId"]), str(binding["targetPath"])
		if targetID != "0" {
			if !validNativeGUID(targetGUID) || !validNativeID(targetID) || closure.session.byGUID[targetGUID] == "" || targetPath != closure.session.byGUID[targetGUID] ||
				filepath.ToSlash(filepath.Clean(targetPath)) != targetPath || !strings.EqualFold(filepath.Ext(targetPath), ".prefab") {
				return fmt.Errorf("imported binding target identity/path is invalid: %s/%s.%s -> %s/%s (%s)", ownerGUID, ownerID, property, targetGUID, targetID, targetPath)
			}
		}
		key := assetKey(ownerGUID, ownerID) + "\x00" + property
		if existing := closure.selected[key]; existing != nil {
			if str(existing["targetGuid"]) != targetGUID || str(existing["targetFileId"]) != targetID || str(existing["targetPath"]) != targetPath {
				return fmt.Errorf("imported binding property has conflicting target identities: %s/%s.%s", ownerGUID, ownerID, property)
			}
			continue
		}
		closure.selected[key] = clone(binding).(map[string]any)
		if targetID != "0" {
			if err := pending.addIdentity(targetGUID, targetID); err != nil {
				return err
			}
		}
		if len(closure.selected) > 32768 {
			return fmt.Errorf("imported binding dependency closure exceeds supported bound")
		}
	}
	return nil
}

func (closure *importedBindingClosure) result(assets map[string]map[string]any) ([]any, error) {
	keys := make([]string, 0, len(closure.selected))
	for key, binding := range closure.selected {
		guid, id := str(binding["targetGuid"]), str(binding["targetFileId"])
		if id != "0" && !preparedAssetIdentity(assets, guid, id) {
			return nil, fmt.Errorf("imported binding target is absent from proven asset/prefab identities: %s/%s", guid, id)
		}
		keys = append(keys, key)
	}
	sort.Strings(keys)
	result := make([]any, 0, len(keys))
	for _, key := range keys {
		result = append(result, closure.selected[key])
	}
	return result, nil
}

func preparedAssetIdentity(assets map[string]map[string]any, guid, id string) bool {
	if asset := assets[assetKey(guid, id)]; asset != nil && str(asset["guid"]) == guid && str(asset["fileId"]) == id {
		return true
	}
	for _, asset := range assets {
		if str(asset["guid"]) != guid || str(asset["kind"]) != "prefab" {
			continue
		}
		for _, alias := range array(asset["objectAliases"]) {
			if str(object(alias)["fileId"]) == id {
				return true
			}
		}
		for _, record := range array(asset["objects"]) {
			if str(object(record)["fileId"]) == id {
				return true
			}
		}
	}
	return false
}
