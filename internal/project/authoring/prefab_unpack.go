package authoring

import (
	"fmt"
	"sort"
	"strings"

	goproject "unitymicroeditor/internal/project"
)

func (p *prefabPlanner) stableLocalIDs(selected map[string]bool, operation string) map[string]string {
	result := map[string]string{}
	// Existing serialized aliases are retained so surviving scene references
	// keep their identities. Newly materialized objects get deterministic IDs.
	for _, doc := range p.asset.docs {
		if expanded := p.graph.LocalIDs[doc.id]; selected[expanded] && result[expanded] == "" {
			result[expanded] = doc.id
		}
	}
	keys := []string{}
	for id := range selected {
		keys = append(keys, id)
	}
	sort.Strings(keys)
	used := map[string]bool{}
	for _, id := range result {
		used[id] = true
	}
	for _, id := range keys {
		if result[id] == "" {
			for attempt := 0; ; attempt++ {
				candidate := p.asset.newID(operation, fmt.Sprintf("unpack/%s/%d", id, attempt))
				if !used[candidate] {
					result[id] = candidate
					used[candidate] = true
					break
				}
			}
		}
	}
	return result
}

func (p *prefabPlanner) unpack(request PrefabEditRequest) error {
	owner, err := p.target(request.FileID, request.TargetPath)
	if err != nil {
		return err
	}
	instance, err := p.instance(owner)
	if err != nil {
		return err
	}
	prefix := instance.id + ":"
	var root *goproject.AuthoredGraphObject
	for _, value := range p.graph.Objects {
		if value.ClassID == 4 || value.ClassID == 224 {
			if strings.HasPrefix(value.FileID, prefix) && !strings.HasPrefix(idReference(value.Data["m_Father"]), prefix) {
				if root != nil {
					return failure("invalid_prefab_root", "unpack instance has multiple root transforms", nil)
				}
				root = p.objects[value.FileID]
			}
		}
	}
	if root == nil {
		return failure("invalid_prefab_root", "unpack instance root is absent", nil)
	}
	selected, err := p.selected(idReference(root.Data["m_GameObject"]))
	if err != nil {
		return err
	}
	mapping := p.stableLocalIDs(selected, request.OperationID)
	replacement := &yamlAsset{prefix: append([]byte{}, p.asset.prefix...), newline: p.asset.newline}
	rootReference := mapping[root.FileID]
	if request.Completely {
		for _, value := range p.graph.Objects {
			if !selected[value.FileID] {
				continue
			}
			data, err := remapPrefabData(value.Data, mapping, p.source.GUID, false)
			if err != nil {
				return err
			}
			for _, key := range []string{"m_CorrespondingSourceObject", "m_PrefabInstance", "m_PrefabAsset"} {
				if _, exists := data[key]; exists {
					data[key] = reference("0")
				}
			}
			if err := replacement.appendDocument(value.ClassID, mapping[value.FileID], value.TypeName, data); err != nil {
				return err
			}
		}
	} else {
		var err error
		replacement, rootReference, err = p.partialUnpack(request, instance, root, mapping)
		if err != nil {
			return err
		}
	}
	// Rewrite surviving references to any aliases collapsed to one materialized
	// object, and replace native SceneRoots instance entries explicitly.
	localRemap := map[string]string{instance.id: rootReference}
	for local, expanded := range p.graph.LocalIDs {
		if replacementID := mapping[expanded]; replacementID != "" {
			localRemap[local] = replacementID
		}
	}
	kept := &yamlAsset{prefix: append([]byte{}, p.asset.prefix...), newline: p.asset.newline}
	for _, doc := range p.asset.docs {
		if doc.id == instance.id || selected[p.graph.LocalIDs[doc.id]] {
			continue
		}
		if idReference(doc.data["m_PrefabInstance"]) == instance.id {
			continue
		}
		// Preserve every unrelated document exactly unless one of its references
		// actually needs changing as part of this source graph transaction.
		changed := false
		if err := walkReferences(doc.data, func(ref map[string]any) error {
			id := idReference(ref)
			if localRemap[id] != "" && localRemap[id] != id && !externalReference(ref) {
				changed = true
			}
			return nil
		}); err != nil {
			return err
		}
		if changed {
			copy, err := clonePrefabDocument(doc, localRemap, p.source.GUID, false)
			if err != nil {
				return err
			}
			kept.docs = append(kept.docs, copy)
		} else {
			kept.docs = append(kept.docs, doc)
		}
	}
	kept.docs = append(kept.docs, replacement.docs...)
	if err := kept.refresh(); err != nil {
		return err
	}
	p.created["unpacked"] = mapping[idReference(root.Data["m_GameObject"])]
	p.created["unpacked.transform"] = mapping[root.FileID]
	return p.writeAsset(p.source.Path, kept, nil)
}

func (p *prefabPlanner) partialUnpack(request PrefabEditRequest, instance *yamlDocument, sceneRoot *goproject.AuthoredGraphObject, mapping map[string]string) (*yamlAsset, string, error) {
	mod := modification(instance)
	for _, key := range []string{"m_RemovedComponents", "m_RemovedGameObjects", "m_AddedGameObjects"} {
		if len(arrayField(mod, key)) != 0 {
			return nil, "", failure("unsupported_structural_override", "outermost unpack of structural overrides is not yet supported", map[string]any{"field": key})
		}
	}
	ref, _ := instance.data["m_SourcePrefab"].(map[string]any)
	guid, _ := ref["guid"].(string)
	path, err := p.store.findPrefabGUID(guid)
	if err != nil {
		return nil, "", err
	}
	source, err := p.store.readSource(path)
	if err != nil {
		return nil, "", err
	}
	graph, _, err := p.includeGraph(path)
	if err != nil {
		return nil, "", err
	}
	template, err := parseAsset([]byte(source.Content))
	if err != nil {
		return nil, "", err
	}
	for _, value := range arrayField(mod, "m_Modifications") {
		entry, _ := value.(map[string]any)
		target, err := sourceOverrideTarget(graph, entry, guid)
		if err != nil {
			return nil, "", err
		}
		if err := p.applySourceProperty(path, template, graph, target, entry); err != nil {
			return nil, "", err
		}
	}
	addedMapping, err := p.applyAddedComponents(path, template, graph, instance, arrayField(mod, "m_AddedComponents"), request.OperationID, false)
	if err != nil {
		return nil, "", err
	}
	sourceRoot, sourceTransform, err := prefabRoots(graph)
	if err != nil {
		return nil, "", err
	}
	localMapping := map[string]string{}
	for local, expanded := range graph.LocalIDs {
		target := mapping[instance.id+":"+expanded]
		if target != "" {
			localMapping[local] = target
		}
	}
	for sceneID, sourceID := range addedMapping {
		localMapping[sourceID] = mapping[sceneID]
	}
	for _, doc := range template.docs {
		if !doc.stripped || localMapping[doc.id] != "" {
			continue
		}
		ref, _ := doc.data["m_CorrespondingSourceObject"].(map[string]any)
		referenceGUID, _ := ref["guid"].(string)
		prefix := idReference(doc.data["m_PrefabInstance"]) + ":"
		for _, object := range graph.Objects {
			if !strings.HasPrefix(object.FileID, prefix) {
				continue
			}
			inner := object
			inner.FileID = strings.TrimPrefix(object.FileID, prefix)
			id, err := prefabGraphIdentity(&inner, referenceGUID)
			if err != nil {
				continue
			}
			if id == idReference(ref) {
				localMapping[doc.id] = mapping[instance.id+":"+object.FileID]
				break
			}
		}
		if localMapping[doc.id] == "" {
			return nil, "", failure("invalid_identity", "partial unpack could not prove an added companion alias", nil)
		}
	}
	for _, doc := range template.docs {
		if doc.classID == 1001 {
			localMapping[doc.id] = p.asset.newID(request.OperationID, "retained-instance/"+doc.id)
		}
	}
	output := &yamlAsset{prefix: append([]byte{}, p.asset.prefix...), newline: p.asset.newline}
	for _, doc := range template.docs {
		copy, err := clonePrefabDocument(doc, localMapping, guid, false)
		if err != nil {
			return nil, "", err
		}
		output.docs = append(output.docs, copy)
	}
	if err := output.refresh(); err != nil {
		return nil, "", err
	}
	ownerID := mapping[instance.id+":"+sourceRoot.FileID]
	transformID := mapping[instance.id+":"+sourceTransform.FileID]
	parent := idReference(sceneRoot.Data["m_Father"])
	if candidate := mapping[parent]; candidate != "" {
		parent = candidate
	}
	if rootDoc := output.find(transformID); rootDoc != nil && !rootDoc.stripped {
		if err := output.setTopLevel(transformID, "m_Father", reference(parent)); err != nil {
			return nil, "", err
		}
		return output, transformID, nil
	}
	// Unpacking the outer instance of a variant retains its immediate base
	// instance. Add explicit aliases for the surviving scene root identities.
	prefix := strings.Split(sourceTransform.FileID, ":")[0]
	baseInstance := output.find(localMapping[prefix])
	if baseInstance == nil || baseInstance.classID != 1001 {
		return nil, "", failure("unsupported_variant", "partial unpack cannot identify variant's immediate base instance", nil)
	}
	if err := setPrefabModificationField(output, baseInstance.id, "m_TransformParent", reference(parent)); err != nil {
		return nil, "", err
	}
	baseRef, _ := baseInstance.data["m_SourcePrefab"].(map[string]any)
	baseGUID, _ := baseRef["guid"].(string)
	for _, value := range []struct {
		object *goproject.AuthoredGraphObject
		id     string
	}{{sourceRoot, ownerID}, {sourceTransform, transformID}} {
		if output.find(value.id) != nil {
			continue
		}
		inner := *value.object
		inner.FileID = strings.TrimPrefix(inner.FileID, prefix+":")
		sourceID, err := prefabGraphIdentity(&inner, baseGUID)
		if err != nil {
			return nil, "", err
		}
		data := map[string]any{"m_CorrespondingSourceObject": map[string]any{"fileID": sourceID, "guid": baseGUID, "type": 3}, "m_PrefabInstance": reference(baseInstance.id), "m_PrefabAsset": reference("0")}
		if err := output.appendDocument(value.object.ClassID, value.id, value.object.TypeName, data); err != nil {
			return nil, "", err
		}
		doc := output.find(value.id)
		doc.header = []byte(fmt.Sprintf("--- !u!%d &%s stripped\n", doc.classID, doc.id))
		if err := output.refresh(); err != nil {
			return nil, "", err
		}
	}
	return output, baseInstance.id, nil
}
