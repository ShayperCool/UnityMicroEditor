package authoring

import (
	"fmt"
	"strings"

	goproject "unitymicroeditor/internal/project"
)

func graphObject(graph goproject.AuthoredGraph, id string) *goproject.AuthoredGraphObject {
	for i := range graph.Objects {
		if graph.Objects[i].FileID == id {
			return &graph.Objects[i]
		}
	}
	return nil
}

// ensurePrefabAlias creates the local stripped identity that Unity uses when a
// newly authored component references an inherited owner or companion.
func (p *prefabPlanner) ensurePrefabAlias(asset *yamlAsset, graph goproject.AuthoredGraph, object *goproject.AuthoredGraphObject, operation string) (string, error) {
	if doc := asset.find(object.FileID); doc != nil && doc.classID == object.ClassID {
		return doc.id, nil
	}
	for _, doc := range asset.docs {
		if doc.classID == object.ClassID && graph.LocalIDs[doc.id] == object.FileID {
			return doc.id, nil
		}
	}
	prefix := strings.Split(object.FileID, ":")[0]
	instance := asset.find(prefix)
	if instance == nil || instance.classID != 1001 {
		return "", failure("invalid_identity", "inherited object has no authored instance", nil)
	}
	ref, _ := instance.data["m_SourcePrefab"].(map[string]any)
	guid, _ := ref["guid"].(string)
	inner := *object
	inner.FileID = strings.TrimPrefix(object.FileID, instance.id+":")
	sourceID, err := prefabGraphIdentity(&inner, guid)
	if err != nil {
		return "", err
	}
	for _, doc := range asset.docs {
		corresponding, _ := doc.data["m_CorrespondingSourceObject"].(map[string]any)
		if doc.classID == object.ClassID && doc.stripped && idReference(doc.data["m_PrefabInstance"]) == instance.id && corresponding["guid"] == guid && idReference(corresponding) == sourceID {
			return doc.id, nil
		}
	}
	id := asset.newID(operation, "prefab-alias/"+object.FileID)
	data := map[string]any{"m_CorrespondingSourceObject": map[string]any{"fileID": sourceID, "guid": guid, "type": 3}, "m_PrefabInstance": reference(instance.id), "m_PrefabAsset": reference("0")}
	if object.ClassID != 1 {
		owner := graphObject(graph, idReference(object.Data["m_GameObject"]))
		if owner == nil {
			return "", failure("invalid_graph", "inherited component owner is absent", nil)
		}
		ownerID, err := p.ensurePrefabAlias(asset, graph, owner, operation)
		if err != nil {
			return "", err
		}
		data["m_GameObject"] = reference(ownerID)
	}
	if err := asset.appendDocument(object.ClassID, id, object.TypeName, data); err != nil {
		return "", err
	}
	doc := asset.find(id)
	doc.stripped = true
	doc.header = []byte(fmt.Sprintf("--- !u!%d &%s stripped%s", object.ClassID, id, asset.newline))
	if err := asset.refresh(); err != nil {
		return "", err
	}
	return id, nil
}

func (p *prefabPlanner) addComponent(request PrefabEditRequest) error {
	if request.DefaultsTemplate == nil {
		return failure("component_defaults_missing", "prefab components require a publicly captured version-bound default template", nil)
	}
	owner, err := p.target(request.FileID, request.TargetPath)
	if err != nil {
		return err
	}
	if doc := p.asset.find(owner.FileID); doc != nil && !doc.stripped && idReference(doc.data["m_PrefabInstance"]) == "0" {
		created, err := applyNativeDefaults(p.asset, NativeDefaultsRequest{OperationID: request.OperationID, FileID: owner.FileID, ComponentType: request.ComponentType, Fields: request.Fields, Template: *request.DefaultsTemplate})
		if err != nil {
			return err
		}
		p.created = created
		return p.writeAsset(p.source.Path, p.asset, nil)
	}
	instance, err := p.instance(owner)
	if err != nil {
		return err
	}
	transform := p.transform(owner.FileID)
	_, _, components, err := nativeDefaultsDocuments(*request.DefaultsTemplate, request.ComponentType)
	if err != nil {
		return err
	}
	for _, component := range components {
		if component.classID == 224 && transform.ClassID != 224 {
			return failure("unsupported_transform_replacement", "an inherited Transform cannot become RectTransform while its prefab link remains; unpack first", nil)
		}
	}
	selected := map[string]bool{}
	for _, object := range p.graph.Objects {
		selected[object.FileID] = true
	}
	mapping := p.stableLocalIDs(selected, request.OperationID)
	temp := &yamlAsset{prefix: append([]byte{}, p.asset.prefix...), newline: p.asset.newline}
	for _, object := range p.graph.Objects {
		data, err := remapPrefabData(object.Data, mapping, p.source.GUID, false)
		if err != nil {
			return err
		}
		for _, key := range []string{"m_CorrespondingSourceObject", "m_PrefabInstance", "m_PrefabAsset"} {
			if _, exists := data[key]; exists {
				data[key] = reference("0")
			}
		}
		if err := temp.appendDocument(object.ClassID, mapping[object.FileID], object.TypeName, data); err != nil {
			return err
		}
	}
	before := map[string]bool{}
	for _, doc := range temp.docs {
		before[doc.id] = true
	}
	created, err := applyNativeDefaults(temp, NativeDefaultsRequest{OperationID: request.OperationID, FileID: mapping[owner.FileID], ComponentType: request.ComponentType, Fields: request.Fields, Template: *request.DefaultsTemplate})
	if err != nil {
		return err
	}
	ref, _ := instance.data["m_SourcePrefab"].(map[string]any)
	guid, _ := ref["guid"].(string)
	inner := *owner
	inner.FileID = strings.TrimPrefix(owner.FileID, instance.id+":")
	sourceOwner, err := prefabGraphIdentity(&inner, guid)
	if err != nil {
		return err
	}
	ownerAlias, err := p.ensurePrefabAlias(p.asset, p.graph, owner, request.OperationID)
	if err != nil {
		return err
	}
	localMapping := map[string]string{mapping[owner.FileID]: ownerAlias}
	for _, doc := range temp.docs {
		if !before[doc.id] {
			if p.asset.find(doc.id) != nil {
				return failure("invalid_identity", "new component collides with an authored identity", nil)
			}
			localMapping[doc.id] = doc.id
		}
	}
	// Capture only companions actually referenced by the new components.
	for _, doc := range temp.docs {
		if !before[doc.id] {
			if err := walkReferences(doc.data, func(ref map[string]any) error {
				id := idReference(ref)
				if id == "0" || externalReference(ref) || localMapping[id] != "" {
					return nil
				}
				for expanded, local := range mapping {
					if local == id {
						alias, err := p.ensurePrefabAlias(p.asset, p.graph, p.objects[expanded], request.OperationID)
						if err != nil {
							return err
						}
						localMapping[id] = alias
						return nil
					}
				}
				return failure("invalid_identity", "default component references an unknown scene identity", map[string]any{"fileId": id})
			}); err != nil {
				return err
			}
		}
	}
	// Alias creation refreshes documents; look up the current instance before
	// editing its existing modification map.
	instance = p.asset.find(instance.id)
	mod := modification(instance)
	added := append([]any{}, arrayField(mod, "m_AddedComponents")...)
	for _, doc := range temp.docs {
		if !before[doc.id] {
			copy, err := clonePrefabDocument(doc, localMapping, "", true)
			if err != nil {
				return err
			}
			p.asset.docs = append(p.asset.docs, copy)
			added = append(added, map[string]any{"targetCorrespondingSourceObject": map[string]any{"fileID": sourceOwner, "guid": guid, "type": 3}, "insertIndex": -1, "addedObject": reference(copy.id)})
		}
	}
	if err := p.asset.refresh(); err != nil {
		return err
	}
	if err := setPrefabModificationField(p.asset, instance.id, "m_AddedComponents", added); err != nil {
		return err
	}
	p.created = created
	return p.writeAsset(p.source.Path, p.asset, nil)
}

// applyAddedComponents copies exact authored component serialization into the
// immediate prefab, retaining scene aliases when its link replaces the override.
func (p *prefabPlanner) applyAddedComponents(sourcePath string, asset *yamlAsset, graph goproject.AuthoredGraph, instance *yamlDocument, entries []any, operation string, retainSceneAliases bool) (map[string]string, error) {
	ref, _ := instance.data["m_SourcePrefab"].(map[string]any)
	guid, _ := ref["guid"].(string)
	ids := map[string]string{}
	owners := map[string]*goproject.AuthoredGraphObject{}
	for _, value := range entries {
		entry, _ := value.(map[string]any)
		targetEntry := map[string]any{"target": entry["targetCorrespondingSourceObject"]}
		owner, err := sourceOverrideTarget(graph, targetEntry, guid)
		if err != nil {
			return nil, err
		}
		if owner.ClassID != 1 {
			return nil, failure("invalid_prefab_override", "added component target must be a GameObject", nil)
		}
		id := idReference(entry["addedObject"])
		doc := p.asset.find(id)
		if doc == nil || doc.stripped || doc.classID == 1 || doc.classID == 4 || doc.classID == 224 || doc.classID == 1001 {
			return nil, failure("invalid_prefab_override", "added component source is absent or invalid", nil)
		}
		if ids[id] != "" {
			return nil, failure("invalid_prefab_override", "duplicate added component record", nil)
		}
		newID := id
		if asset.find(newID) != nil {
			newID = asset.newID(operation, "applied-component/"+id)
		}
		ids[id] = newID
		owners[id] = owner
	}
	for _, entryValue := range entries {
		entry, _ := entryValue.(map[string]any)
		oldID := idReference(entry["addedObject"])
		owner := owners[oldID]
		alias, err := p.ensurePrefabAlias(asset, graph, owner, operation)
		if err != nil {
			return nil, err
		}
		doc := p.asset.find(oldID)
		mapping := map[string]string{}
		for key, value := range ids {
			mapping[key] = value
		}
		mapping[idReference(doc.data["m_GameObject"])] = alias
		if err := walkReferences(doc.data, func(ref map[string]any) error {
			id := idReference(ref)
			if id == "0" || externalReference(ref) || mapping[id] != "" {
				return nil
			}
			expanded := p.graph.LocalIDs[id]
			if expanded == "" {
				expanded = id
			}
			innerID := strings.TrimPrefix(expanded, instance.id+":")
			if innerID == expanded {
				return failure("external_subtree_reference", "applying a component with outside scene references into a prefab is unsupported", map[string]any{"fileId": id})
			}
			target := graphObject(graph, innerID)
			if target == nil {
				return failure("invalid_identity", "component reference lacks proven prefab source identity", nil)
			}
			local, err := p.ensurePrefabAlias(asset, graph, target, operation)
			if err != nil {
				return err
			}
			mapping[id] = local
			return nil
		}); err != nil {
			return nil, err
		}
		copy, err := clonePrefabDocument(doc, mapping, p.source.GUID, true)
		if err != nil {
			return nil, err
		}
		asset.docs = append(asset.docs, copy)
		if err := asset.refresh(); err != nil {
			return nil, err
		}
		ownerDoc := asset.find(alias)
		if !ownerDoc.stripped {
			components := componentIDs(ownerDoc.data)
			components = append(components, copy.id)
			items := []any{}
			for _, id := range components {
				items = append(items, map[string]any{"component": reference(id)})
			}
			if err := asset.setTopLevel(alias, "m_Component", items); err != nil {
				return nil, err
			}
		} else {
			innerInstance := asset.find(strings.Split(owner.FileID, ":")[0])
			innerRef, _ := innerInstance.data["m_SourcePrefab"].(map[string]any)
			innerGUID, _ := innerRef["guid"].(string)
			innerOwner := *owner
			innerOwner.FileID = strings.TrimPrefix(owner.FileID, innerInstance.id+":")
			id, err := prefabGraphIdentity(&innerOwner, innerGUID)
			if err != nil {
				return nil, err
			}
			mod := modification(innerInstance)
			values := append([]any{}, arrayField(mod, "m_AddedComponents")...)
			values = append(values, map[string]any{"targetCorrespondingSourceObject": map[string]any{"fileID": id, "guid": innerGUID, "type": 3}, "insertIndex": -1, "addedObject": reference(copy.id)})
			if err := setPrefabModificationField(asset, innerInstance.id, "m_AddedComponents", values); err != nil {
				return nil, err
			}
		}
		if retainSceneAliases {
			ownerAlias := idReference(doc.data["m_GameObject"])
			data := map[string]any{"m_CorrespondingSourceObject": map[string]any{"fileID": copy.id, "guid": guid, "type": 3}, "m_PrefabInstance": reference(instance.id), "m_PrefabAsset": reference("0"), "m_GameObject": reference(ownerAlias)}
			strip := &yamlAsset{prefix: p.asset.prefix, newline: p.asset.newline}
			if err := strip.appendDocument(doc.classID, doc.id, doc.typeName, data); err != nil {
				return nil, err
			}
			strip.docs[0].stripped = true
			strip.docs[0].header = []byte(fmt.Sprintf("--- !u!%d &%s stripped%s", doc.classID, doc.id, p.asset.newline))
			for i, value := range p.asset.docs {
				if value.id == doc.id {
					p.asset.docs[i] = strip.docs[0]
					break
				}
			}
		}
	}
	if retainSceneAliases {
		if err := p.asset.refresh(); err != nil {
			return nil, err
		}
	}
	return ids, nil
}
