package authoring

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"

	goproject "unitymicroeditor/project"
)

func (s *Store) findPrefabGUID(guid string) (string, error) {
	path := ""
	err := filepath.WalkDir(filepath.Join(s.root, "Assets"), func(absolute string, entry os.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if entry.IsDir() || entry.Type()&os.ModeSymlink != 0 || !strings.HasSuffix(absolute, ".prefab.meta") {
			return nil
		}
		data, err := readLimited(absolute)
		if err != nil {
			return err
		}
		matches := guidPattern.FindAllSubmatch(data, -1)
		if len(matches) == 1 && strings.ToLower(string(matches[0][1])) == guid {
			relative, _ := filepath.Rel(s.root, strings.TrimSuffix(absolute, ".meta"))
			if path != "" {
				return failure("duplicate_guid", "prefab source GUID is ambiguous", nil)
			}
			path = filepath.ToSlash(relative)
		}
		return nil
	})
	if err != nil {
		return "", err
	}
	if path == "" {
		return "", failure("not_found", "authored prefab source GUID is not present under Assets", map[string]any{"guid": guid})
	}
	return path, nil
}

func checkGraphRevisions(revisions, expected map[string]string) error {
	for path, actual := range revisions {
		if expected[path] == "" {
			return failure("revision_required", "prefab operation requires each dependency revision from resolved hierarchy", map[string]any{"path": path})
		}
		if actual != expected[path] {
			return failure("revision_conflict", "prefab authoring dependency changed", map[string]any{"path": path, "actualRevision": actual, "expectedRevision": expected[path]})
		}
	}
	return nil
}

func (s *Store) instantiatePrefab(asset *yamlAsset, destinationPath, operationID string, operation Operation) (map[string]string, error) {
	if _, err := s.assetPath(operation.PrefabPath, ".prefab"); err != nil {
		return nil, err
	}
	source, err := s.readSource(operation.PrefabPath)
	if err != nil {
		return nil, err
	}
	if operation.PrefabRevision == "" || source.Revision != operation.PrefabRevision {
		return nil, failure("revision_conflict", "prefab source requires its exact current revision", map[string]any{"actualRevision": source.Revision})
	}
	graph, dependencies, _, err := s.resolvedGraph(operation.PrefabPath)
	if err != nil {
		return nil, err
	}
	if err := checkGraphRevisions(dependencies, operation.DependencyRevisions); err != nil {
		return nil, err
	}
	if strings.HasSuffix(destinationPath, ".prefab") && dependencies[destinationPath] != "" {
		return nil, failure("prefab_cycle", "instantiation would make this prefab depend on itself", nil)
	}
	var rootTransform, rootObject *goproject.AuthoredGraphObject
	for i := range graph.Objects {
		value := &graph.Objects[i]
		if (value.ClassID == 4 || value.ClassID == 224) && idReference(value.Data["m_Father"]) == "0" {
			if rootTransform != nil {
				return nil, failure("invalid_prefab_root", "prefab has multiple roots", nil)
			}
			rootTransform = value
		}
	}
	if rootTransform == nil {
		return nil, failure("invalid_prefab_root", "prefab has no root Transform", nil)
	}
	for i := range graph.Objects {
		value := &graph.Objects[i]
		if value.ClassID == 1 && value.FileID == idReference(rootTransform.Data["m_GameObject"]) {
			rootObject = value
			break
		}
	}
	if rootObject == nil {
		return nil, failure("invalid_prefab_root", "prefab root GameObject is absent", nil)
	}
	rootObjectID, err := prefabGraphIdentity(rootObject, source.GUID)
	if err != nil {
		return nil, err
	}
	rootTransformID, err := prefabGraphIdentity(rootTransform, source.GUID)
	if err != nil {
		return nil, err
	}
	parentTransform := "0"
	if operation.ParentID != "0" {
		_, parent, err := asset.plainObject(operation.ParentID)
		if err != nil {
			return nil, err
		}
		parentTransform = parent.id
	}
	instanceID := asset.newID(operationID, operation.Key+".instance")
	objectID := asset.newID(operationID, operation.Key)
	transformID := asset.newID(operationID, operation.Key+".transform")
	modifications := []any{}
	if operation.Name != "" {
		modifications = append(modifications, map[string]any{"target": map[string]any{"fileID": rootObjectID, "guid": source.GUID, "type": 3}, "propertyPath": "m_Name", "value": operation.Name, "objectReference": reference("0")})
	}
	instance := map[string]any{"m_ObjectHideFlags": int64(0), "serializedVersion": int64(2), "m_SourcePrefab": map[string]any{"fileID": "100100000", "guid": source.GUID, "type": 3}, "m_Modification": map[string]any{"serializedVersion": int64(3), "m_TransformParent": reference(parentTransform), "m_Modifications": modifications, "m_RemovedComponents": []any{}, "m_RemovedGameObjects": []any{}, "m_AddedGameObjects": []any{}, "m_AddedComponents": []any{}}}
	if err := asset.appendDocument(1001, instanceID, "PrefabInstance", instance); err != nil {
		return nil, err
	}
	for _, value := range []struct {
		id, sourceID, typeName string
		classID                int
	}{{objectID, rootObjectID, "GameObject", 1}, {transformID, rootTransformID, rootTransform.TypeName, rootTransform.ClassID}} {
		data := map[string]any{"m_CorrespondingSourceObject": map[string]any{"fileID": value.sourceID, "guid": source.GUID, "type": 3}, "m_PrefabInstance": reference(instanceID), "m_PrefabAsset": reference("0")}
		if err := asset.appendDocument(value.classID, value.id, value.typeName, data); err != nil {
			return nil, err
		}
		document := asset.find(value.id)
		document.header = []byte(fmt.Sprintf("--- !u!%d &%s stripped%s", value.classID, value.id, asset.newline))
		if err := asset.refresh(); err != nil {
			return nil, err
		}
	}
	if err := asset.appendChild(parentTransform, transformID); err != nil {
		return nil, err
	}
	return map[string]string{"": objectID, ".instance": instanceID, ".transform": transformID}, nil
}

func (s *Store) prefabOverride(asset *yamlAsset, operation Operation) error {
	instance := asset.find(operation.InstanceID)
	if instance == nil || instance.classID != 1001 || instance.stripped {
		return failure("not_found", "instanceId must address an authored PrefabInstance", nil)
	}
	if operation.TargetFileID == "" || operation.Property == "" || strings.HasSuffix(operation.Property, ".Array.size") {
		return failure("invalid_request", "prefab override requires targetFileId and scalar/reference property; source array resizing is unsupported", nil)
	}
	for _, part := range strings.Split(operation.Property, ".") {
		key := strings.Split(part, "[")[0]
		if protectedProperty(key) {
			return failure("protected_property", "identity and ownership changes require a prefab graph operation", nil)
		}
	}
	sourceReference, _ := instance.data["m_SourcePrefab"].(map[string]any)
	sourceGUID, _ := sourceReference["guid"].(string)
	path, err := s.findPrefabGUID(sourceGUID)
	if err != nil {
		return err
	}
	graph, revisions, _, err := s.resolvedGraph(path)
	if err != nil {
		return err
	}
	if err := checkGraphRevisions(revisions, operation.DependencyRevisions); err != nil {
		return err
	}
	targetGUID := operation.TargetGUID
	if targetGUID == "" {
		targetGUID = sourceGUID
	}
	var target *goproject.AuthoredGraphObject
	for i := range graph.Objects {
		object := &graph.Objects[i]
		for _, identity := range object.Identities {
			if identity.GUID == targetGUID && identity.FileID == operation.TargetFileID {
				target = object
				break
			}
		}
		if target != nil {
			break
		}
	}
	if target == nil {
		return failure("not_found", "override target is outside the resolved prefab source graph", nil)
	}
	modification, _ := instance.data["m_Modification"].(map[string]any)
	modifications, _ := modification["m_Modifications"].([]any)
	updated := append([]any{}, modifications...)
	match := -1
	for i, value := range updated {
		item, _ := value.(map[string]any)
		ref, _ := item["target"].(map[string]any)
		guid, _ := ref["guid"].(string)
		if guid == "" {
			guid = sourceGUID
		}
		if guid == targetGUID && idReference(ref) == operation.TargetFileID && item["propertyPath"] == operation.Property {
			if match >= 0 {
				return failure("invalid_prefab", "duplicate property override records require Editor normalization", nil)
			}
			match = i
		}
	}
	if operation.Action == "revert_prefab_override" {
		if match < 0 {
			return nil
		}
		updated = append(updated[:match], updated[match+1:]...)
	} else {
		value, err := normalizeJSON(operation.Value)
		if err != nil {
			return err
		}
		ref := operation.ObjectReference
		if mapping, ok := value.(map[string]any); ok {
			ref = mapping
			value = ""
		}
		if ref == nil {
			ref = reference("0")
		}
		normalized, err := normalizeJSON(ref)
		if err != nil {
			return err
		}
		ref = normalized.(map[string]any)
		if value == nil {
			value = ""
		}
		switch value.(type) {
		case string, int, int64, float64:
		default:
			return failure("invalid_property_value", "prefab override must be a scalar or object reference", nil)
		}
		if err := goproject.ApplyAuthoredOverride(target.Data, operation.Property, fmt.Sprint(value), ref); err != nil {
			return failure("unsupported_property", err.Error(), map[string]any{"property": operation.Property})
		}
		item := map[string]any{"target": map[string]any{"fileID": operation.TargetFileID, "guid": targetGUID, "type": 3}, "propertyPath": operation.Property, "value": fmt.Sprint(value), "objectReference": ref}
		if match < 0 {
			updated = append(updated, item)
		} else {
			old, _ := updated[match].(map[string]any)
			for key, value := range old {
				if _, exists := item[key]; !exists {
					item[key] = value
				}
			}
			updated[match] = item
		}
	}
	return setPrefabModificationField(asset, instance.id, "m_Modifications", updated)
}
