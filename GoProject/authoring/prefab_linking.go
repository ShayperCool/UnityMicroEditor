package authoring

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"path/filepath"
	"sort"
	"strconv"
	"strings"

	"gopkg.in/yaml.v3"
	goproject "unitymicroeditor/project"
)

func prefabGraphIdentity(object *goproject.AuthoredGraphObject, guid string) (string, error) {
	if strings.Contains(object.FileID, ":") {
		var inherited int64
		for _, part := range strings.Split(object.FileID, ":") {
			id, err := strconv.ParseInt(part, 10, 64)
			if err != nil {
				return "", err
			}
			inherited ^= id
		}
		candidate := strconv.FormatInt(inherited&0x7fffffffffffffff, 10)
		for _, identity := range object.Identities {
			if identity.GUID == guid && identity.FileID == candidate {
				return candidate, nil
			}
		}
	}
	return sourceIdentity(object, guid)
}

func prefabRoots(graph goproject.AuthoredGraph) (*goproject.AuthoredGraphObject, *goproject.AuthoredGraphObject, error) {
	var transform, owner *goproject.AuthoredGraphObject
	for i := range graph.Objects {
		value := &graph.Objects[i]
		if (value.ClassID == 4 || value.ClassID == 224) && idReference(value.Data["m_Father"]) == "0" {
			if transform != nil {
				return nil, nil, failure("invalid_prefab_root", "prefab must have one resolved root", nil)
			}
			transform = value
		}
	}
	if transform != nil {
		for i := range graph.Objects {
			value := &graph.Objects[i]
			if value.ClassID == 1 && value.FileID == idReference(transform.Data["m_GameObject"]) {
				owner = value
				break
			}
		}
	}
	if owner == nil {
		return nil, nil, failure("invalid_prefab_root", "prefab root owner is absent", nil)
	}
	return owner, transform, nil
}

func (p *prefabPlanner) instantiate(request PrefabEditRequest) error {
	parent := "0"
	if request.ParentFileID != "" || request.ParentPath != "" {
		target, err := p.target(request.ParentFileID, request.ParentPath)
		if err != nil {
			return err
		}
		parent = p.localOwner(target)
		if parent == "" {
			return failure("unsupported_parent", "instantiation under an inherited parent requires an explicit local parent alias", nil)
		}
	}
	source, err := p.store.readSource(request.PrefabPath)
	if err != nil {
		return err
	}
	_, revisions, err := p.includeGraph(request.PrefabPath)
	if err != nil {
		return err
	}
	created, err := p.store.instantiatePrefab(p.asset, p.source.Path, request.OperationID, Operation{PrefabPath: request.PrefabPath, PrefabRevision: source.Revision, DependencyRevisions: revisions, Key: "prefab", Name: request.Name, ParentID: parent})
	if err != nil {
		return err
	}
	for key, value := range created {
		p.created["prefab"+key] = value
	}
	return p.writeAsset(p.source.Path, p.asset, nil)
}
func (p *prefabPlanner) localOwner(object *goproject.AuthoredGraphObject) string {
	if doc := p.asset.find(object.FileID); doc != nil && doc.classID == 1 {
		return doc.id
	}
	for _, doc := range p.asset.docs {
		if doc.classID == 1 && p.graph.LocalIDs[doc.id] == object.FileID {
			return doc.id
		}
	}
	return ""
}

func (p *prefabPlanner) newAsset(request PrefabEditRequest, asset *yamlAsset) error {
	if _, err := p.store.assetPath(request.OutputPath, ".prefab"); err != nil {
		return err
	}
	revision, err := p.store.dependencyRevision(request.OutputPath)
	if err != nil {
		return err
	}
	if revision != "absent" {
		return failure("revision_conflict", "new prefab source or metadata already exists", map[string]any{"path": request.OutputPath})
	}
	hash := sha256.Sum256([]byte("unitymicroeditor-prefab-guid-v1\x00" + request.OperationID + "\x00" + request.OutputPath))
	guid := hex.EncodeToString(hash[:16])
	if err := p.store.checkGUIDAvailable(guid); err != nil {
		return err
	}
	metadata := "fileFormatVersion: 2\nguid: " + guid + "\nPrefabImporter:\n  externalObjects: {}\n  userData: \n  assetBundleName: \n  assetBundleVariant: \n"
	p.inputs[request.OutputPath] = "absent"
	p.created["assetGuid"] = guid
	return p.writeAsset(request.OutputPath, asset, &metadata)
}

func (p *prefabPlanner) createVariant(request PrefabEditRequest) error {
	graph, revisions, err := p.includeGraph(request.PrefabPath)
	if err != nil {
		return err
	}
	if revisions[request.OutputPath] != "" {
		return failure("prefab_cycle", "variant would depend on itself", nil)
	}
	source, err := p.store.readSource(request.PrefabPath)
	if err != nil {
		return err
	}
	asset := &yamlAsset{prefix: []byte("%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n"), newline: "\n"}
	id := asset.newID(request.OperationID, "variant-instance")
	owner, _, err := prefabRoots(graph)
	if err != nil {
		return err
	}
	targetID, err := prefabGraphIdentity(owner, source.GUID)
	if err != nil {
		return err
	}
	name := strings.TrimSuffix(filepath.Base(request.OutputPath), ".prefab")
	instance := map[string]any{"m_ObjectHideFlags": int64(0), "serializedVersion": int64(2), "m_SourcePrefab": map[string]any{"fileID": "100100000", "guid": source.GUID, "type": 3}, "m_Modification": map[string]any{"serializedVersion": int64(3), "m_TransformParent": reference("0"), "m_Modifications": []any{map[string]any{"target": map[string]any{"fileID": targetID, "guid": source.GUID, "type": 3}, "propertyPath": "m_Name", "value": name, "objectReference": reference("0")}}, "m_RemovedComponents": []any{}, "m_RemovedGameObjects": []any{}, "m_AddedGameObjects": []any{}, "m_AddedComponents": []any{}}}
	if err := asset.appendDocument(1001, id, "PrefabInstance", instance); err != nil {
		return err
	}
	p.created["variant.instance"] = id
	return p.newAsset(request, asset)
}

func (p *prefabPlanner) createPrefab(request PrefabEditRequest) error {
	target, err := p.target(request.FileID, request.TargetPath)
	if err != nil {
		return err
	}
	selected, err := p.selected(target.FileID)
	if err != nil {
		return err
	}
	if strings.Contains(target.FileID, ":") {
		return failure("unsupported_subtree", "creating a prefab from an inherited instance root requires an explicit variant; choose create_variant", nil)
	}
	transform := p.transform(target.FileID)
	if transform == nil {
		return failure("invalid_graph", "prefab root Transform is absent", nil)
	}
	// The selected root loses its outside parent before closed-reference
	// validation. Every other source reference must stay within this subtree.
	input, err := parseAsset(p.asset.bytes())
	if err != nil {
		return err
	}
	if err := input.setTopLevel(transform.FileID, "m_Father", reference("0")); err != nil {
		return err
	}
	included := map[string]bool{}
	for _, doc := range p.asset.docs {
		if expanded := p.graph.LocalIDs[doc.id]; selected[expanded] {
			included[doc.id] = true
		}
		if doc.classID == 1001 {
			prefix := doc.id + ":"
			inside, outside := false, false
			for id := range p.objects {
				if strings.HasPrefix(id, prefix) {
					if selected[id] {
						inside = true
					} else {
						outside = true
					}
				}
			}
			if inside && outside {
				return failure("unsupported_subtree", "prefab subtree cuts through an inherited nested instance", nil)
			}
			if inside {
				included[doc.id] = true
			}
		}
	}
	output := &yamlAsset{prefix: append([]byte{}, p.asset.prefix...), newline: p.asset.newline}
	mapping := map[string]string{}
	// Ordinary IDs are retained. Inherited stripped aliases must become the
	// canonical prefab-local XOR identity rather than an arbitrary scene alias:
	// otherwise Unity propagates a different source ID through later variants.
	for id := range included {
		mapping[id] = id
	}
	for _, doc := range input.docs {
		if !included[doc.id] || !doc.stripped {
			continue
		}
		ref, _ := doc.data["m_CorrespondingSourceObject"].(map[string]any)
		instanceID, err := strconv.ParseInt(idReference(doc.data["m_PrefabInstance"]), 10, 64)
		if err != nil {
			return err
		}
		sourceID, err := strconv.ParseInt(idReference(ref), 10, 64)
		if err != nil {
			return err
		}
		mapping[doc.id] = strconv.FormatInt((instanceID^sourceID)&0x7fffffffffffffff, 10)
	}
	used := map[string]string{}
	for sourceID, targetID := range mapping {
		if old := used[targetID]; old != "" && old != sourceID {
			return failure("invalid_identity", "canonical prefab alias collides with another source document", map[string]any{"fileId": targetID})
		}
		used[targetID] = sourceID
	}
	for _, doc := range input.docs {
		if included[doc.id] {
			copy, err := clonePrefabDocument(doc, mapping, p.source.GUID, true)
			if err != nil {
				return err
			}
			output.docs = append(output.docs, copy)
		}
	}
	if len(output.docs) == 0 {
		return failure("invalid_subtree", "prefab subtree contains no authored documents", nil)
	}
	if err := output.refresh(); err != nil {
		return err
	}
	if err := output.setTopLevel(transform.FileID, "m_Father", reference("0")); err != nil {
		return err
	}
	name := strings.TrimSuffix(filepath.Base(request.OutputPath), ".prefab")
	if _, err := output.setProperty(target.FileID, "m_Name", name); err != nil {
		return err
	}
	p.created["prefab"] = target.FileID
	p.created["prefab.transform"] = transform.FileID
	return p.newAsset(request, output)
}

func clonePrefabDocument(doc *yamlDocument, mapping map[string]string, sourceGUID string, requireClosed bool) (*yamlDocument, error) {
	asset, err := parseAsset(append([]byte("%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n"), append(append([]byte{}, doc.header...), doc.body...)...))
	if err != nil {
		return nil, err
	}
	copy := asset.docs[0]
	if id := mapping[copy.id]; id != "" {
		copy.id = id
		copy.header = []byte(fmt.Sprintf("--- !u!%d &%s%s\n", copy.classID, id, map[bool]string{true: " stripped"}[copy.stripped]))
	}
	// Rewrite only PPtr identity spans. Re-encoding an entire copied document
	// would alter unknown serialized fields, scalar tags, comments and styles.
	type edit struct {
		start, end int
		text       []byte
	}
	var edits []edit
	var visit func(*yaml.Node) error
	visit = func(node *yaml.Node) error {
		if node.Kind == yaml.MappingNode {
			identity, _ := nodeField(node, "fileID")
			if identity != nil && identity.Kind == yaml.ScalarNode {
				guidNode, _ := nodeField(node, "guid")
				guid := ""
				if guidNode != nil {
					guid = guidNode.Value
				}
				id := identity.Value
				if id == "0" || guid != "" && guid != sourceGUID {
					return nil
				}
				replacement := mapping[id]
				if replacement == "" {
					if requireClosed {
						return failure("external_subtree_reference", "prefab subtree references an object outside the selected source subtree", map[string]any{"fileId": id})
					}
					return nil
				}
				if guid != "" {
					// Self-GUID references become local PPtrs. Preserve every
					// other field/tag on this PPtr rather than rebuilding the object.
					n := *node
					n.Content = nil
					for i := 0; i+1 < len(node.Content); i += 2 {
						key, value := node.Content[i], node.Content[i+1]
						if key.Value == "guid" || key.Value == "type" {
							continue
						}
						if key.Value == "fileID" {
							c := *value
							c.Value = replacement
							c.Tag = "!!int"
							c.Style = 0
							value = &c
						}
						n.Content = append(n.Content, key, value)
					}
					start, end, err := prefabCollectionSpan(copy, node)
					if err != nil {
						return err
					}
					text, err := encodeNode(&n)
					if err != nil {
						return err
					}
					if node.Style&yaml.FlowStyle == 0 {
						text += asset.newline
					}
					edits = append(edits, edit{start, end, []byte(text)})
					return nil
				}
				if replacement != id {
					start := nodeOffset(copy.body, identity.Line, identity.Column)
					end, err := scalarEnd(copy.body, start, identity)
					if err != nil {
						return err
					}
					edits = append(edits, edit{start, end, []byte(replacement)})
				}
				return nil
			}
		}
		for _, child := range node.Content {
			if err := visit(child); err != nil {
				return err
			}
		}
		return nil
	}
	if err := visit(copy.node); err != nil {
		return nil, err
	}
	sort.Slice(edits, func(i, j int) bool { return edits[i].start > edits[j].start })
	for _, edit := range edits {
		copy.body = append(append(append([]byte{}, copy.body[:edit.start]...), edit.text...), copy.body[edit.end:]...)
	}
	if err := asset.refresh(); err != nil {
		return nil, err
	}
	return asset.docs[0], nil
}

func prefabCollectionSpan(doc *yamlDocument, node *yaml.Node) (int, int, error) {
	start := nodeOffset(doc.body, node.Line, node.Column)
	if node.Style&yaml.FlowStyle != 0 {
		depth, quote := 0, byte(0)
		for i := start; i < len(doc.body); i++ {
			c := doc.body[i]
			if quote != 0 {
				if quote == '"' && c == '\\' {
					i++
					continue
				}
				if c == quote {
					if quote == '\'' && i+1 < len(doc.body) && doc.body[i+1] == '\'' {
						i++
						continue
					}
					quote = 0
				}
				continue
			}
			if c == '\'' || c == '"' {
				quote = c
				continue
			}
			if c == '{' || c == '[' {
				depth++
			}
			if c == '}' || c == ']' {
				depth--
				if depth == 0 {
					return start, i + 1, nil
				}
			}
		}
		return 0, 0, failure("invalid_yaml", "reference lexical span is incomplete", nil)
	}
	end := len(doc.body)
	var next func(*yaml.Node)
	next = func(value *yaml.Node) {
		if value == node {
			return
		}
		if offset := nodeOffset(doc.body, value.Line, value.Column); offset > start && lineOffset(doc.body, value.Line) < end {
			end = lineOffset(doc.body, value.Line)
		}
		for _, child := range value.Content {
			next(child)
		}
	}
	next(doc.node)
	return start, end, nil
}
func remapPrefabData(data map[string]any, mapping map[string]string, sourceGUID string, requireClosed bool) (map[string]any, error) {
	encoded, err := json.Marshal(data)
	if err != nil {
		return nil, err
	}
	decoder := json.NewDecoder(strings.NewReader(string(encoded)))
	decoder.UseNumber()
	var copied map[string]any
	if err := decoder.Decode(&copied); err != nil {
		return nil, err
	}
	err = walkReferences(copied, func(ref map[string]any) error {
		id := idReference(ref)
		guid, _ := ref["guid"].(string)
		if id == "0" || guid != "" && guid != sourceGUID {
			return nil
		}
		if replacement := mapping[id]; replacement != "" {
			ref["fileID"] = replacement
			if guid == sourceGUID {
				delete(ref, "guid")
				delete(ref, "type")
			}
			return nil
		}
		if requireClosed {
			return failure("external_subtree_reference", "prefab subtree references an object outside the selected source subtree", map[string]any{"fileId": id})
		}
		return nil
	})
	if err != nil {
		return nil, err
	}
	normalized, err := normalizeJSON(copied)
	if err != nil {
		return nil, err
	}
	return normalized.(map[string]any), nil
}
