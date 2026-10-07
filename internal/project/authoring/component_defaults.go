package authoring

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"sort"
	"strings"
)

// NativeDefaultsTemplate is a version/profile-bound snapshot obtained once
// through Unity's public component creation API. The caller verifies its profile
// against the installed engine and resolved packages before source publication.
type NativeDefaultsTemplate struct {
	SchemaVersion int    `json:"schemaVersion"`
	Profile       string `json:"profile"`
	UnityVersion  string `json:"unityVersion"`
	UnityRevision string `json:"unityRevision"`
	ComponentType string `json:"componentType"`
	Content       string `json:"content"`
	SHA256        string `json:"sha256"`
}

type NativeDefaultsRequest struct {
	Path             string                 `json:"path"`
	ExpectedRevision string                 `json:"expectedRevision"`
	OperationID      string                 `json:"operationId"`
	FileID           string                 `json:"fileId,omitempty"`
	TargetPath       string                 `json:"targetPath,omitempty"`
	ComponentType    string                 `json:"componentType"`
	Fields           map[string]any         `json:"fields,omitempty"`
	Template         NativeDefaultsTemplate `json:"template"`
	BeforePublish    func() error           `json:"-"`
}

// AddNativeDefaults publishes the component and all required companions in one
// existing source journal. No Editor process or authored scratch asset is used.
func (s *Store) AddNativeDefaults(ctx context.Context, request NativeDefaultsRequest) (MutationResult, error) {
	if _, err := s.assetPath(request.Path, ".unity", ".prefab"); err != nil {
		return MutationResult{}, err
	}
	return s.mutate(ctx, request.Path, request.ExpectedRevision, request.OperationID, request, func() (mutationPlan, error) {
		source, err := s.readSource(request.Path)
		if err != nil {
			return mutationPlan{}, err
		}
		if source.Revision != request.ExpectedRevision {
			return mutationPlan{}, failure("revision_conflict", "authored source changed before cached component publication", map[string]any{"actualRevision": source.Revision})
		}
		asset, err := parseAsset([]byte(source.Content))
		if err != nil {
			return mutationPlan{}, err
		}
		original, err := parseAsset([]byte(source.Content))
		if err != nil {
			return mutationPlan{}, err
		}
		created, err := applyNativeDefaults(asset, request)
		if err != nil {
			return mutationPlan{}, err
		}
		if err := asset.validate(source.GUID); err != nil {
			return mutationPlan{}, err
		}
		if err := asset.preservePrefabDocuments(original); err != nil {
			return mutationPlan{}, err
		}
		if strings.HasSuffix(request.Path, ".prefab") {
			if err := asset.validatePrefabRoot(); err != nil {
				return mutationPlan{}, err
			}
		}
		return mutationPlan{source: source, content: asset.bytes(), metadata: []byte(source.Metadata), created: created, beforePublish: request.BeforePublish,
			changes: []Change{{FileID: created[request.ComponentType], Action: "add_component", After: request.ComponentType}}}, nil
	})
}

func nativeDefaultsDocuments(template NativeDefaultsTemplate, requested string) (*yamlAsset, *yamlDocument, []*yamlDocument, error) {
	name := strings.TrimPrefix(requested, "UnityEngine.")
	hash := sha256.Sum256([]byte(template.Content))
	if template.SchemaVersion != 1 || template.Profile == "" || template.UnityVersion != VerifiedUnityVersion ||
		template.UnityRevision == "" || template.SHA256 != hex.EncodeToString(hash[:]) ||
		strings.TrimPrefix(template.ComponentType, "UnityEngine.") != name || len(template.Content) > 32<<20 {
		return nil, nil, nil, failure("invalid_component_defaults", "cached component defaults lack their exact version/profile/content identity", nil)
	}
	asset, err := parseAsset([]byte(template.Content))
	if err != nil {
		return nil, nil, nil, err
	}
	var owner *yamlDocument
	for _, doc := range asset.docs {
		if doc.classID == 1 {
			if owner != nil || doc.stripped || idReference(doc.data["m_PrefabInstance"]) != "0" {
				return nil, nil, nil, failure("invalid_component_defaults", "defaults must contain one ordinary template GameObject", nil)
			}
			owner = doc
		}
	}
	if owner == nil {
		return nil, nil, nil, failure("invalid_component_defaults", "template GameObject is absent", nil)
	}
	var components []*yamlDocument
	seen := map[string]bool{}
	transforms, selected := 0, 0
	for _, id := range defaultsComponentIDs(owner) {
		doc := asset.find(id)
		if doc == nil || seen[id] || doc.stripped || idReference(doc.data["m_GameObject"]) != owner.id ||
			idReference(doc.data["m_PrefabInstance"]) != "0" {
			return nil, nil, nil, failure("invalid_component_defaults", "template component ownership is invalid", nil)
		}
		seen[id] = true
		if doc.classID == 4 || doc.classID == 224 {
			transforms++
		} else if nativeComponentTypes[doc.classID] != doc.typeName {
			return nil, nil, nil, failure("invalid_component_defaults", "template contains an unsupported companion component", nil)
		}
		if doc.typeName == name {
			selected++
		}
		components = append(components, doc)
	}
	if transforms != 1 || selected != 1 || len(components) > 32 {
		return nil, nil, nil, failure("invalid_component_defaults", "template requires one transform and one selected native component", nil)
	}
	if err := asset.validate(""); err != nil {
		return nil, nil, nil, err
	}
	return asset, owner, components, nil
}

func defaultsComponentIDs(owner *yamlDocument) []string {
	values, _ := owner.data["m_Component"].([]any)
	ids := make([]string, 0, len(values))
	for _, item := range values {
		entry, _ := item.(map[string]any)
		ids = append(ids, idReference(entry["component"]))
	}
	return ids
}

func defaultsTarget(asset *yamlAsset, fileID, targetPath string) (string, error) {
	if fileID != "" {
		if targetPath != "" {
			return "", failure("invalid_request", "select a component owner by fileId or targetPath", nil)
		}
		return fileID, nil
	}
	if targetPath == "" {
		return "", failure("invalid_request", "component owner requires fileId or targetPath", nil)
	}
	view, err := hierarchy(Source{}, asset, "")
	if err != nil {
		return "", err
	}
	objects := map[string]GameObject{}
	for _, object := range view.Objects {
		objects[object.FileID] = object
	}
	found := ""
	for _, object := range view.Objects {
		parts := []string{object.Name}
		seen := map[string]bool{object.FileID: true}
		for parent := object.ParentID; parent != "0"; {
			value, exists := objects[parent]
			if !exists || seen[parent] {
				return "", failure("invalid_graph", "component target hierarchy has an absent or cyclic parent", nil)
			}
			seen[parent] = true
			parts = append([]string{value.Name}, parts...)
			parent = value.ParentID
		}
		if strings.Join(parts, "/") == strings.TrimPrefix(targetPath, "/") {
			if found != "" {
				return "", failure("ambiguous_target", "component target path matches multiple GameObjects", nil)
			}
			found = object.FileID
		}
	}
	if found == "" {
		return "", failure("not_found", "component target path is absent", nil)
	}
	return found, nil
}

// applyNativeDefaults is shared with the source prefab planner. Its caller must
// already hold the source transaction and supply an ordinary materialized owner.
func applyNativeDefaults(asset *yamlAsset, request NativeDefaultsRequest) (map[string]string, error) {
	_, templateOwner, components, err := nativeDefaultsDocuments(request.Template, request.ComponentType)
	if err != nil {
		return nil, err
	}
	ownerID, err := defaultsTarget(asset, request.FileID, request.TargetPath)
	if err != nil {
		return nil, err
	}
	owner, transform, err := asset.plainObject(ownerID)
	if err != nil {
		return nil, err
	}
	requested := strings.TrimPrefix(request.ComponentType, "UnityEngine.")
	existing := map[int]*yamlDocument{}
	for _, id := range defaultsComponentIDs(owner) {
		doc := asset.find(id)
		if doc == nil {
			return nil, failure("invalid_graph", "target component is absent", nil)
		}
		if doc.typeName == requested {
			return nil, failure("component_exists", "the requested native component already exists on the target", nil)
		}
		if existing[doc.classID] == nil {
			existing[doc.classID] = doc
		}
	}
	remap := map[string]string{templateOwner.id: owner.id}
	created := map[string]string{}
	for _, component := range components {
		if component.classID == 4 || component.classID == 224 {
			remap[component.id] = transform.id
			continue
		}
		if companion := existing[component.classID]; companion != nil {
			remap[component.id] = companion.id
		} else {
			remap[component.id] = asset.newID(request.OperationID, "native-default."+component.id)
		}
		created["UnityEngine."+component.typeName] = remap[component.id]
	}
	created[request.ComponentType] = created["UnityEngine."+requested]
	ownerComponents := append([]string{}, defaultsComponentIDs(owner)...)
	for _, component := range components {
		if component.classID == 4 || component.classID == 224 {
			if component.classID == 224 && transform.classID == 4 {
				data, err := defaultsData(component.data, remap)
				if err != nil {
					return nil, err
				}
				for field, value := range transform.data {
					switch field {
					case "serializedVersion", "m_GameObject", "m_CorrespondingSourceObject", "m_PrefabInstance", "m_PrefabAsset":
						continue
					}
					data[field] = value
				}
				index := -1
				for i, doc := range asset.docs {
					if doc.id == transform.id {
						index = i
						break
					}
				}
				id := transform.id
				asset.docs = append(asset.docs[:index], asset.docs[index+1:]...)
				if err := asset.appendDocument(224, id, "RectTransform", data); err != nil {
					return nil, err
				}
				added := asset.docs[len(asset.docs)-1]
				asset.docs = append(asset.docs[:len(asset.docs)-1], nil)
				copy(asset.docs[index+1:], asset.docs[index:len(asset.docs)-1])
				asset.docs[index] = added
				if err := asset.refresh(); err != nil {
					return nil, err
				}
				created["UnityEngine.RectTransform"] = id
			}
			continue
		}
		if existing[component.classID] != nil {
			continue
		}
		data, err := defaultsData(component.data, remap)
		if err != nil {
			return nil, err
		}
		id := remap[component.id]
		if err := asset.appendDocument(component.classID, id, component.typeName, data); err != nil {
			return nil, err
		}
		ownerComponents = append(ownerComponents, id)
	}
	values := make([]any, 0, len(ownerComponents))
	for _, id := range ownerComponents {
		values = append(values, map[string]any{"component": reference(id)})
	}
	if err := asset.setTopLevel(ownerID, "m_Component", values); err != nil {
		return nil, err
	}
	selected := created["UnityEngine."+requested]
	keys := make([]string, 0, len(request.Fields))
	for key := range request.Fields {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	for _, key := range keys {
		if key == "m_ObjectHideFlags" {
			return nil, failure("protected_property", "component fields cannot replace identity or ownership", nil)
		}
		if _, err := asset.setProperty(selected, key, request.Fields[key]); err != nil {
			return nil, err
		}
	}
	return created, nil
}

func defaultsData(input map[string]any, remap map[string]string) (map[string]any, error) {
	encoded, err := json.Marshal(input)
	if err != nil {
		return nil, err
	}
	var output map[string]any
	decoder := json.NewDecoder(strings.NewReader(string(encoded)))
	decoder.UseNumber()
	if err := decoder.Decode(&output); err != nil {
		return nil, err
	}
	normalized, err := normalizeJSON(output)
	if err != nil {
		return nil, err
	}
	output = normalized.(map[string]any)
	if err := walkReferences(output, func(ref map[string]any) error {
		if externalReference(ref) || idReference(ref) == "0" {
			return nil
		}
		id, exists := remap[idReference(ref)]
		if !exists {
			return failure("invalid_component_defaults", "template local reference leaves the captured component closure", map[string]any{"fileId": fmt.Sprint(ref["fileID"])})
		}
		ref["fileID"] = id
		return nil
	}); err != nil {
		return nil, err
	}
	return output, nil
}
