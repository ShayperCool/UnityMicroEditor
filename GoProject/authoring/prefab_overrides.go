package authoring

import (
	"fmt"
	"sort"
	"strings"

	goproject "unitymicroeditor/project"
)

func (p *prefabPlanner) setOverride(request PrefabEditRequest) error {
	if mapping, ok := request.Value.(map[string]any); ok {
		if _, reference := mapping["fileID"]; !reference {
			if len(mapping) == 0 {
				return failure("invalid_property_value", "empty mapping override has no serialized leaves", nil)
			}
			keys := []string{}
			for key := range mapping {
				keys = append(keys, key)
			}
			sort.Strings(keys)
			for _, key := range keys {
				nested := request
				nested.Property = request.Property + "." + key
				nested.Value = mapping[key]
				if err := p.setOverride(nested); err != nil {
					return err
				}
			}
			return nil
		}
	}
	if values, ok := request.Value.([]any); ok {
		_, target, err := p.overrideTarget(request)
		if err != nil {
			return err
		}
		old, err := authoredPropertyValue(target.Data, strings.ReplaceAll(request.Property, ".Array.data[", "["))
		if err != nil {
			return err
		}
		items, ok := old.([]any)
		if !ok || len(items) != len(values) {
			return failure("unsupported_array_resize", "array overrides must preserve existing serialized element count", nil)
		}
		for index, value := range values {
			nested := request
			nested.Property = fmt.Sprintf("%s.Array.data[%d]", request.Property, index)
			nested.Value = value
			if err := p.setOverride(nested); err != nil {
				return err
			}
		}
		return p.writeAsset(p.source.Path, p.asset, nil)
	}
	owner, target, err := p.overrideTarget(request)
	if err != nil {
		return err
	}
	instance, err := p.instance(owner)
	if err != nil {
		return err
	}
	sourceRef, _ := instance.data["m_SourcePrefab"].(map[string]any)
	guid, _ := sourceRef["guid"].(string)
	sourcePath, err := p.store.findPrefabGUID(guid)
	if err != nil {
		return err
	}
	graph, revisions, err := p.includeGraph(sourcePath)
	if err != nil {
		return err
	}
	var sourceTarget *goproject.AuthoredGraphObject
	for i := range graph.Objects {
		value := &graph.Objects[i]
		if instance.id+":"+value.FileID == target.FileID {
			sourceTarget = value
			break
		}
	}
	if sourceTarget == nil {
		if doc := p.asset.find(target.FileID); doc != nil && !doc.stripped && doc.classID != 1 && doc.classID != 4 && doc.classID != 224 {
			if _, err := p.asset.setProperty(doc.id, strings.ReplaceAll(request.Property, ".Array.data[", "["), request.Value); err != nil {
				return err
			}
			return p.writeAsset(p.source.Path, p.asset, nil)
		}
		return failure("unsupported_target", "added prefab objects require structural source overrides", nil)
	}
	id, err := prefabGraphIdentity(sourceTarget, guid)
	if err != nil {
		return err
	}
	if err := p.store.prefabOverride(p.asset, Operation{Action: "set_prefab_override", InstanceID: instance.id, TargetGUID: guid, TargetFileID: id, Property: request.Property, Value: request.Value, DependencyRevisions: revisions}); err != nil {
		return err
	}
	return p.writeAsset(p.source.Path, p.asset, nil)
}

func (p *prefabPlanner) overrideTarget(request PrefabEditRequest) (*goproject.AuthoredGraphObject, *goproject.AuthoredGraphObject, error) {
	owner, err := p.target(request.FileID, request.TargetPath)
	if err != nil {
		return nil, nil, err
	}
	target := owner
	if request.ComponentType != "" {
		target = nil
		for _, id := range componentIDs(owner.Data) {
			value := p.objects[id]
			if value != nil && (value.TypeName == request.ComponentType || "UnityEngine."+value.TypeName == request.ComponentType || value.Data["m_EditorClassIdentifier"] == request.ComponentType) {
				if target != nil {
					return nil, nil, failure("ambiguous_target", "multiple components match componentType", nil)
				}
				target = value
			}
		}
		if target == nil {
			return nil, nil, failure("not_found", "requested prefab component is absent", nil)
		}
	}
	return owner, target, nil
}

func modification(instance *yamlDocument) map[string]any {
	result, _ := instance.data["m_Modification"].(map[string]any)
	return result
}
func arrayField(data map[string]any, key string) []any { result, _ := data[key].([]any); return result }

func sourceOverrideTarget(graph goproject.AuthoredGraph, entry map[string]any, defaultGUID string) (*goproject.AuthoredGraphObject, error) {
	reference, _ := entry["target"].(map[string]any)
	guid, _ := reference["guid"].(string)
	if guid == "" {
		guid = defaultGUID
	}
	id := idReference(reference)
	var target *goproject.AuthoredGraphObject
	for i := range graph.Objects {
		object := &graph.Objects[i]
		for _, identity := range object.Identities {
			if identity.GUID == guid && identity.FileID == id {
				if target != nil && target.FileID != object.FileID {
					return nil, failure("ambiguous_target", "override target matches repeated nested prefab objects; use immediate source identity", nil)
				}
				target = object
			}
		}
	}
	if target == nil {
		return nil, failure("invalid_prefab_override", "override target is absent from resolved source", map[string]any{"guid": guid, "fileId": id})
	}
	return target, nil
}
func defaultPrefabOverride(root, transform, target *goproject.AuthoredGraphObject, property string) bool {
	if target.FileID == root.FileID && property == "m_Name" {
		return true
	}
	if target.FileID != transform.FileID {
		return false
	}
	for _, prefix := range []string{"m_LocalPosition", "m_LocalRotation", "m_LocalEulerAnglesHint", "m_RootOrder"} {
		if property == prefix || strings.HasPrefix(property, prefix+".") {
			return true
		}
	}
	return false
}

func (p *prefabPlanner) applyRevert(request PrefabEditRequest) error {
	owner, err := p.target(request.FileID, request.TargetPath)
	if err != nil {
		return err
	}
	instance, err := p.instance(owner)
	if err != nil {
		return err
	}
	mod := modification(instance)
	for _, key := range []string{"m_RemovedComponents", "m_RemovedGameObjects", "m_AddedGameObjects"} {
		if len(arrayField(mod, key)) != 0 {
			return failure("unsupported_structural_override", "this source prefab operation cannot apply/revert this structural override yet", map[string]any{"field": key})
		}
	}
	sourceRef, _ := instance.data["m_SourcePrefab"].(map[string]any)
	sourceGUID, _ := sourceRef["guid"].(string)
	sourcePath, err := p.store.findPrefabGUID(sourceGUID)
	if err != nil {
		return err
	}
	graph, _, err := p.includeGraph(sourcePath)
	if err != nil {
		return err
	}
	root, transform, err := prefabRoots(graph)
	if err != nil {
		return err
	}
	source, err := p.store.readSource(sourcePath)
	if err != nil {
		return err
	}
	asset, err := parseAsset([]byte(source.Content))
	if err != nil {
		return err
	}
	added := arrayField(mod, "m_AddedComponents")
	if request.Action == "apply" {
		if _, err := p.applyAddedComponents(sourcePath, asset, graph, instance, added, request.OperationID, true); err != nil {
			return err
		}
	} else {
		remove := map[string]bool{}
		for _, value := range added {
			entry, _ := value.(map[string]any)
			id := idReference(entry["addedObject"])
			if doc := p.asset.find(id); doc == nil || doc.stripped {
				return failure("invalid_prefab_override", "added component is absent", nil)
			}
			remove[id] = true
		}
		kept := p.asset.docs[:0]
		for _, doc := range p.asset.docs {
			if !remove[doc.id] {
				kept = append(kept, doc)
			}
		}
		p.asset.docs = kept
		if err := p.asset.refresh(); err != nil {
			return err
		}
	}
	retained := []any{}
	for _, value := range arrayField(mod, "m_Modifications") {
		entry, _ := value.(map[string]any)
		target, err := sourceOverrideTarget(graph, entry, sourceGUID)
		if err != nil {
			return err
		}
		property, _ := entry["propertyPath"].(string)
		if defaultPrefabOverride(root, transform, target, property) {
			retained = append(retained, entry)
			continue
		}
		if request.Action == "apply" {
			if err := p.applySourceProperty(sourcePath, asset, graph, target, entry); err != nil {
				return err
			}
		}
	}
	if err := setPrefabModificationField(p.asset, instance.id, "m_AddedComponents", []any{}); err != nil {
		return err
	}
	if err := setPrefabModificationField(p.asset, instance.id, "m_Modifications", retained); err != nil {
		return err
	}
	if request.Action == "apply" {
		if err := p.writeAsset(sourcePath, asset, nil); err != nil {
			return err
		}
	}
	return p.writeAsset(p.source.Path, p.asset, nil)
}
func replaceMapField(original map[string]any, key string, value any) map[string]any {
	result := map[string]any{}
	for k, v := range original {
		result[k] = v
	}
	result[key] = value
	return result
}

func (p *prefabPlanner) applySourceProperty(path string, asset *yamlAsset, graph goproject.AuthoredGraph, target *goproject.AuthoredGraphObject, entry map[string]any) error {
	property, _ := entry["propertyPath"].(string)
	ref, _ := entry["objectReference"].(map[string]any)
	if idReference(ref) != "0" && !externalReference(ref) {
		return failure("external_subtree_reference", "applying a scene object reference into a prefab requires an explicit source identity", nil)
	}
	if document := asset.find(target.FileID); document != nil && !document.stripped && document.classID != 1001 {
		copied, err := remapPrefabData(target.Data, map[string]string{}, "", false)
		if err != nil {
			return err
		}
		if err := goproject.ApplyAuthoredOverride(copied, property, entry["value"], ref); err != nil {
			return failure("unsupported_property", err.Error(), nil)
		}
		// Change only the addressed serialized span. Unity Array.data paths are
		// translated to the lossless source property's indexed notation.
		normalized := strings.ReplaceAll(property, ".Array.data[", "[")
		value, err := authoredPropertyValue(copied, normalized)
		if err != nil {
			return err
		}
		_, err = asset.setProperty(document.id, normalized, value)
		return err
	}
	prefix := strings.Split(target.FileID, ":")[0]
	instance := asset.find(prefix)
	if instance == nil || instance.classID != 1001 {
		return failure("unsupported_target", "cannot locate inherited source property's authored instance", nil)
	}
	sourceRef, _ := instance.data["m_SourcePrefab"].(map[string]any)
	guid, _ := sourceRef["guid"].(string)
	// Remove exactly this enclosing instance's prefix before computing the
	// inherited identity in its immediate source prefab.
	inner := *target
	inner.FileID = strings.TrimPrefix(target.FileID, instance.id+":")
	id, err := prefabGraphIdentity(&inner, guid)
	if err != nil {
		return err
	}
	sourcePath, err := p.store.findPrefabGUID(guid)
	if err != nil {
		return err
	}
	_, revisions, err := p.includeGraph(sourcePath)
	if err != nil {
		return err
	}
	value := entry["value"]
	if idReference(ref) != "0" {
		value = ref
	}
	return p.store.prefabOverride(asset, Operation{Action: "set_prefab_override", InstanceID: instance.id, TargetGUID: guid, TargetFileID: id, Property: property, Value: value, ObjectReference: ref, DependencyRevisions: revisions})
}
func authoredPropertyValue(data map[string]any, path string) (any, error) {
	parts, err := parseProperty(path)
	if err != nil {
		return nil, err
	}
	var current any = data
	for _, part := range parts {
		match := propertySegment.FindStringSubmatch(part)
		mapping, ok := current.(map[string]any)
		if !ok {
			return nil, failure("unsupported_property", "property traversal is not a mapping", nil)
		}
		current = mapping[match[1]]
		if match[2] != "" {
			var index int
			if _, err := fmt.Sscan(match[2], &index); err != nil {
				return nil, err
			}
			items, ok := current.([]any)
			if !ok || index < 0 || index >= len(items) {
				return nil, failure("unsupported_property", "property array element is absent", nil)
			}
			current = items[index]
		}
	}
	return current, nil
}
