package authoring

import (
	"context"
	"crypto/sha256"
	"encoding/binary"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"

	"gopkg.in/yaml.v3"
)

type Component struct {
	FileID   string         `json:"fileId"`
	ClassID  int            `json:"classId"`
	Type     string         `json:"type"`
	Fields   map[string]any `json:"fields"`
	Stripped bool           `json:"stripped"`
}
type GameObject struct {
	FileID                      string      `json:"fileId"`
	TransformID                 string      `json:"transformId,omitempty"`
	Name                        string      `json:"name"`
	ParentID                    string      `json:"parentId"`
	ChildIDs                    []string    `json:"childIds"`
	UnresolvedChildTransformIDs []string    `json:"unresolvedChildTransformIds,omitempty"`
	UnresolvedParentTransformID string      `json:"unresolvedParentTransformId,omitempty"`
	Active                      bool        `json:"active"`
	Components                  []Component `json:"components"`
	PrefabInstanceID            string      `json:"prefabInstanceId,omitempty"`
	Stripped                    bool        `json:"stripped,omitempty"`
}
type Hierarchy struct {
	Path                      string       `json:"path"`
	GUID                      string       `json:"guid"`
	Revision                  string       `json:"revision"`
	Roots                     []string     `json:"roots"`
	Objects                   []GameObject `json:"objects"`
	UnresolvedPrefabInstances []string     `json:"unresolvedPrefabInstances"`
	UnresolvedRootIDs         []string     `json:"unresolvedRootIds,omitempty"`
}

func hierarchy(source Source, asset *yamlAsset, query string) (Hierarchy, error) {
	result := Hierarchy{Path: source.Path, GUID: source.GUID, Revision: source.Revision, Roots: []string{}, Objects: []GameObject{}, UnresolvedPrefabInstances: []string{}}
	transforms := map[string]*yamlDocument{}
	for _, d := range asset.docs {
		if d.classID == 4 || d.classID == 224 {
			transforms[idReference(d.data["m_GameObject"])] = d
		}
		if d.classID == 1001 {
			result.UnresolvedPrefabInstances = append(result.UnresolvedPrefabInstances, d.id)
		}
	}
	for _, d := range asset.docs {
		if d.classID != 1 {
			continue
		}
		name, _ := d.data["m_Name"].(string)
		object := GameObject{FileID: d.id, Name: name, ParentID: "0", ChildIDs: []string{}, Active: fmt.Sprint(d.data["m_IsActive"]) == "1", Components: []Component{}, Stripped: d.stripped}
		if instance := idReference(d.data["m_PrefabInstance"]); instance != "0" {
			object.PrefabInstanceID = instance
		}
		if transform := transforms[d.id]; transform != nil {
			object.TransformID = transform.id
			if parent := asset.find(idReference(transform.data["m_Father"])); parent != nil {
				object.ParentID = idReference(parent.data["m_GameObject"])
				if parent.stripped && object.ParentID == "0" {
					object.UnresolvedParentTransformID = parent.id
				}
			}
			for _, id := range idsFromList(transform.data["m_Children"]) {
				if child := asset.find(id); child != nil {
					if owner := idReference(child.data["m_GameObject"]); owner != "0" {
						object.ChildIDs = append(object.ChildIDs, owner)
					} else {
						object.UnresolvedChildTransformIDs = append(object.UnresolvedChildTransformIDs, child.id)
					}
				}
			}
		}
		if object.ParentID == "0" && object.UnresolvedParentTransformID == "" {
			result.Roots = append(result.Roots, d.id)
		}
		components, _ := d.data["m_Component"].([]any)
		for _, item := range components {
			mapping, _ := item.(map[string]any)
			if component := asset.find(idReference(mapping["component"])); component != nil {
				object.Components = append(object.Components, Component{component.id, component.classID, component.typeName, component.data, component.stripped})
			}
		}
		if query == "" || strings.Contains(strings.ToLower(name), strings.ToLower(query)) || d.id == query {
			result.Objects = append(result.Objects, object)
		}
	}
	// SceneRoots stores sibling order independently of document order.
	for _, d := range asset.docs {
		if d.classID == 1660057539 {
			result.Roots = []string{}
			for _, id := range idsFromList(d.data["m_Roots"]) {
				owner := rawRootOwner(asset, id)
				if owner != "0" {
					result.Roots = append(result.Roots, owner)
				} else {
					result.UnresolvedRootIDs = append(result.UnresolvedRootIDs, id)
				}
			}
		}
	}
	return result, nil
}

func rawRootOwner(asset *yamlAsset, id string) string {
	document := asset.find(id)
	if document == nil {
		return "0"
	}
	ownerOf := func(transform *yamlDocument) string {
		owner := idReference(transform.data["m_GameObject"])
		if object := asset.find(owner); object != nil && object.classID == 1 {
			return owner
		}
		return "0"
	}
	if document.classID == 4 || document.classID == 224 {
		return ownerOf(document)
	}
	if document.classID != 1001 {
		return "0"
	}
	// A SceneRoots entry can reference PrefabInstance rather than its stripped
	// Transform. Only explicit local root ownership can identify a raw owner;
	// absent inherited parent fields must not turn a child alias into a root.
	owner := "0"
	for _, alias := range asset.docs {
		if alias.classID != 4 && alias.classID != 224 || idReference(alias.data["m_PrefabInstance"]) != id {
			continue
		}
		parent, explicit := alias.data["m_Father"]
		if !explicit || idReference(parent) != "0" {
			continue
		}
		candidate := ownerOf(alias)
		if candidate == "0" {
			continue
		}
		if owner != "0" && owner != candidate {
			return "0"
		}
		owner = candidate
	}
	return owner
}
func (s *Store) Hierarchy(ctx context.Context, path, query string) (result Hierarchy, err error) {
	err = s.locked(ctx, func() error {
		if _, err := s.assetPath(path, ".unity", ".prefab"); err != nil {
			return err
		}
		source, err := s.readSource(path)
		if err != nil {
			return err
		}
		asset, err := parseAsset([]byte(source.Content))
		if err != nil {
			return err
		}
		result, err = hierarchy(source, asset, query)
		return err
	})
	return
}

// Operation addresses local Unity fileIDs as strings, never JSON numbers.
// Creation keys allow subsequent operations in the same transaction to use
// "$key" and "$key.transform" before IDs have been published.
type Operation struct {
	Action              string            `json:"action" jsonschema:"create_gameobject,delete_gameobject,reparent,set_property,add_component,remove_component,instantiate_prefab,set_prefab_override,revert_prefab_override"`
	FileID              string            `json:"fileId,omitempty"`
	Key                 string            `json:"key,omitempty"`
	Name                string            `json:"name,omitempty"`
	ParentID            string            `json:"parentId,omitempty" jsonschema:"parent GameObject fileID or 0 for root; local transform is preserved"`
	Property            string            `json:"property,omitempty"`
	Value               any               `json:"value,omitempty"`
	ScriptPath          string            `json:"scriptPath,omitempty"`
	ComponentType       string            `json:"componentType,omitempty"`
	ClassID             int               `json:"classId,omitempty"`
	TemplatePath        string            `json:"templatePath,omitempty"`
	TemplateFileID      string            `json:"templateFileId,omitempty"`
	TemplateRevision    string            `json:"templateRevision,omitempty"`
	PrefabPath          string            `json:"prefabPath,omitempty"`
	PrefabRevision      string            `json:"prefabRevision,omitempty"`
	InstanceID          string            `json:"instanceId,omitempty"`
	TargetGUID          string            `json:"targetGuid,omitempty"`
	TargetFileID        string            `json:"targetFileId,omitempty"`
	ObjectReference     map[string]any    `json:"objectReference,omitempty"`
	DependencyRevisions map[string]string `json:"dependencyRevisions,omitempty"`
	RectTransform       bool              `json:"rectTransform,omitempty"`
	Fields              map[string]any    `json:"fields,omitempty"`
	Recursive           bool              `json:"recursive,omitempty"`
}
type EditRequest struct {
	Path             string      `json:"path"`
	ExpectedRevision string      `json:"expectedRevision"`
	OperationID      string      `json:"operationId"`
	Operations       []Operation `json:"operations"`
	Preview          bool        `json:"preview,omitempty"`
}
type EditResult struct {
	MutationResult
	Preview bool   `json:"preview,omitempty"`
	Content string `json:"content,omitempty"`
}

func (s *Store) Edit(ctx context.Context, request EditRequest) (result EditResult, err error) {
	if _, err := s.assetPath(request.Path, ".unity", ".prefab", ".asset", ".mat"); err != nil {
		return result, err
	}
	if len(request.Operations) == 0 || len(request.Operations) > 256 {
		return result, failure("invalid_request", "provide 1 to 256 edit operations", nil)
	}
	build := func() (mutationPlan, error) {
		source, err := s.readSource(request.Path)
		if err != nil {
			return mutationPlan{}, err
		}
		if source.Revision != request.ExpectedRevision {
			return mutationPlan{}, failure("revision_conflict", "authored asset or metadata changed", map[string]any{"actualRevision": source.Revision, "expectedRevision": request.ExpectedRevision})
		}
		asset, err := parseAsset([]byte(source.Content))
		if err != nil {
			return mutationPlan{}, err
		}
		original, err := parseAsset([]byte(source.Content))
		if err != nil {
			return mutationPlan{}, err
		}
		beforeDocs := map[string]bool{}
		for _, doc := range asset.docs {
			beforeDocs[doc.id] = true
		}
		created := map[string]string{}
		allowedPrefabChanges := map[string]bool{}
		changes := []Change{}
		resolve := func(id string) string {
			if strings.HasPrefix(id, "$") {
				return created[strings.TrimPrefix(id, "$")]
			}
			return id
		}
		for i, operation := range request.Operations {
			operation.FileID = resolve(operation.FileID)
			operation.InstanceID = resolve(operation.InstanceID)
			if strings.HasPrefix(operation.ParentID, "$") {
				operation.ParentID = resolve(operation.ParentID)
				if operation.ParentID == "" {
					return mutationPlan{}, failure("not_found", "creation key for parent does not exist", nil)
				}
			}
			if operation.ParentID == "" {
				operation.ParentID = "0"
			}
			switch operation.Action {
			case "instantiate_prefab":
				if operation.Key == "" {
					operation.Key = strconv.Itoa(i)
				}
				if _, exists := created[operation.Key]; exists {
					return mutationPlan{}, failure("invalid_request", "duplicate creation key", nil)
				}
				ids, err := s.instantiatePrefab(asset, request.Path, request.OperationID, operation)
				if err != nil {
					return mutationPlan{}, err
				}
				for suffix, id := range ids {
					created[operation.Key+suffix] = id
				}
				changes = append(changes, Change{FileID: ids[".instance"], Action: operation.Action, After: operation.PrefabPath})
			case "set_prefab_override", "revert_prefab_override":
				if err := s.prefabOverride(asset, operation); err != nil {
					return mutationPlan{}, err
				}
				allowedPrefabChanges[operation.InstanceID] = true
				changes = append(changes, Change{FileID: operation.InstanceID, Property: operation.Property, Action: operation.Action, After: operation.Value})
			case "set_property":
				before, err := asset.setProperty(operation.FileID, operation.Property, operation.Value)
				if err != nil {
					return mutationPlan{}, err
				}
				changes = append(changes, Change{FileID: operation.FileID, Property: operation.Property, Action: operation.Action, Before: before, After: operation.Value})
			case "create_gameobject":
				if operation.Key == "" {
					operation.Key = strconv.Itoa(i)
				}
				if _, exists := created[operation.Key]; exists {
					return mutationPlan{}, failure("invalid_request", "duplicate creation key", nil)
				}
				if operation.Name == "" || len(operation.Name) > 1024 {
					return mutationPlan{}, failure("invalid_request", "GameObject name must contain 1 to 1024 bytes", nil)
				}
				var id, transformID string
				if operation.RectTransform {
					fields, fieldErr := s.rectTransformFields(operation)
					if fieldErr != nil {
						return mutationPlan{}, fieldErr
					}
					id, transformID, err = asset.createObjectWithTransform(request.OperationID, operation.Key, operation.Name, operation.ParentID, 224, fields)
				} else {
					id, transformID, err = asset.createObject(request.OperationID, operation.Key, operation.Name, operation.ParentID)
				}
				if err != nil {
					return mutationPlan{}, err
				}
				created[operation.Key] = id
				created[operation.Key+".transform"] = transformID
				changes = append(changes, Change{FileID: id, Action: operation.Action, After: operation.Name})
			case "reparent":
				before, err := asset.reparent(operation.FileID, operation.ParentID)
				if err != nil {
					return mutationPlan{}, err
				}
				changes = append(changes, Change{FileID: operation.FileID, Action: operation.Action, Before: before, After: operation.ParentID})
			case "delete_gameobject":
				removed, err := asset.deleteObject(operation.FileID, operation.Recursive)
				if err != nil {
					return mutationPlan{}, err
				}
				changes = append(changes, Change{FileID: operation.FileID, Action: operation.Action, Before: removed})
			case "add_component":
				if operation.Key == "" {
					operation.Key = strconv.Itoa(i)
				}
				if _, exists := created[operation.Key]; exists {
					return mutationPlan{}, failure("invalid_request", "duplicate creation key", nil)
				}
				if operation.ComponentType != "" || operation.ClassID != 0 || operation.TemplatePath != "" {
					id, err := s.addNativeComponent(asset, request.OperationID, operation)
					if err != nil {
						return mutationPlan{}, err
					}
					created[operation.Key] = id
					changes = append(changes, Change{FileID: id, Action: operation.Action, After: operation.ComponentType})
					continue
				}
				if _, err := s.assetPath(operation.ScriptPath, ".cs"); err != nil {
					return mutationPlan{}, err
				}
				script, err := s.readSource(operation.ScriptPath)
				if err != nil {
					return mutationPlan{}, err
				}
				id, err := asset.addComponent(request.OperationID, operation.Key, operation.FileID, script.GUID, operation.Fields)
				if err != nil {
					return mutationPlan{}, err
				}
				created[operation.Key] = id
				changes = append(changes, Change{FileID: id, Action: operation.Action, After: operation.ScriptPath})
			case "remove_component":
				if err := asset.removeComponent(operation.FileID); err != nil {
					return mutationPlan{}, err
				}
				changes = append(changes, Change{FileID: operation.FileID, Action: operation.Action})
			default:
				return mutationPlan{}, failure("unsupported_operation", "unknown source edit action", map[string]any{"action": operation.Action})
			}
		}
		if err := asset.validate(source.GUID); err != nil {
			return mutationPlan{}, err
		}
		if err := asset.preservePrefabDocumentsExcept(original, allowedPrefabChanges); err != nil {
			return mutationPlan{}, err
		}
		if strings.HasSuffix(request.Path, ".prefab") {
			if err := asset.validatePrefabRoot(); err != nil {
				return mutationPlan{}, err
			}
		}
		removed := map[string]bool{}
		for id := range beforeDocs {
			if asset.find(id) == nil {
				removed[id] = true
			}
		}
		if len(removed) != 0 {
			if err := s.checkExternalReferences(source, removed); err != nil {
				return mutationPlan{}, err
			}
		}
		return mutationPlan{source: source, content: asset.bytes(), metadata: []byte(source.Metadata), created: created, changes: changes}, nil
	}
	if request.Preview {
		err = s.locked(ctx, func() error {
			if request.ExpectedRevision == "" || request.OperationID == "" {
				return failure("invalid_request", "preview requires expectedRevision and operationId", nil)
			}
			plan, err := build()
			if err != nil {
				return err
			}
			result = EditResult{MutationResult: MutationResult{OperationID: request.OperationID, Path: request.Path, GUID: plan.source.GUID, PreviousRevision: plan.source.Revision, Revision: revision(plan.content, plan.metadata), Changed: plan.source.Content != string(plan.content), CreatedIDs: plan.created, Changes: plan.changes}, Preview: true, Content: string(plan.content)}
			return nil
		})
		return
	}
	result.MutationResult, err = s.mutate(ctx, request.Path, request.ExpectedRevision, request.OperationID, request, build)
	return
}

func (a *yamlAsset) validatePrefabRoot() error {
	roots := 0
	for _, document := range a.docs {
		if document.classID == 1660057539 {
			return failure("invalid_prefab_root", "prefab cannot contain a scene root registry", nil)
		}
		if (document.classID == 4 || document.classID == 224) && !document.stripped && idReference(document.data["m_Father"]) == "0" {
			roots++
		}
		if document.classID == 1001 {
			modification, _ := document.data["m_Modification"].(map[string]any)
			if idReference(modification["m_TransformParent"]) == "0" {
				roots++
			}
		}
	}
	if roots != 1 {
		return failure("invalid_prefab_root", "prefab must retain exactly one authored or inherited root", nil)
	}
	return nil
}
func (a *yamlAsset) plainObject(id string) (*yamlDocument, *yamlDocument, error) {
	object := a.find(id)
	if object == nil || object.classID != 1 {
		return nil, nil, failure("not_found", "GameObject fileID is absent", map[string]any{"fileId": id})
	}
	if object.stripped || idReference(object.data["m_PrefabInstance"]) != "0" {
		return nil, nil, failure("unsupported_prefab_write", "prefab instance and stripped GameObjects require override-aware authoring", nil)
	}
	for _, doc := range a.docs {
		if (doc.classID == 4 || doc.classID == 224) && idReference(doc.data["m_GameObject"]) == id {
			if doc.stripped || idReference(doc.data["m_PrefabInstance"]) != "0" {
				return nil, nil, failure("unsupported_prefab_write", "prefab instance transforms cannot be changed structurally", nil)
			}
			return object, doc, nil
		}
	}
	return nil, nil, failure("invalid_graph", "GameObject requires an authored Transform", nil)
}
func (a *yamlAsset) newID(operation, key string) string {
	for attempt := 0; ; attempt++ {
		digest := sha256.Sum256([]byte(operation + "\x00" + key + "\x00" + strconv.Itoa(attempt)))
		value := binary.BigEndian.Uint64(digest[:8]) & ((1 << 56) - 1)
		if value == 0 {
			continue
		}
		id := strconv.FormatUint(value, 10)
		if a.find(id) == nil {
			return id
		}
	}
}
func (a *yamlAsset) appendDocument(classID int, id, typeName string, data map[string]any) error {
	var node yaml.Node
	if err := node.Encode(map[string]any{typeName: data}); err != nil {
		return err
	}
	var normalizeIDs func(*yaml.Node)
	normalizeIDs = func(n *yaml.Node) {
		if n.Kind == yaml.MappingNode {
			for i := 0; i+1 < len(n.Content); i += 2 {
				if n.Content[i].Value == "fileID" && n.Content[i+1].Kind == yaml.ScalarNode {
					n.Content[i+1].Tag = "!!int"
					n.Content[i+1].Style = 0
				}
			}
		}
		for _, child := range n.Content {
			normalizeIDs(child)
		}
	}
	normalizeIDs(&node)
	// yaml.Node sorts supplied maps; this affects only the newly created object.
	text, err := encodeNode(&node)
	if err != nil {
		return err
	}
	text = strings.ReplaceAll(text, "\n", a.newline) + a.newline
	if len(a.docs) > 0 {
		last := a.docs[len(a.docs)-1]
		if len(last.body) > 0 && last.body[len(last.body)-1] != '\n' {
			last.body = append(last.body, []byte(a.newline)...)
		}
	}
	a.docs = append(a.docs, &yamlDocument{id: id, classID: classID, header: []byte(fmt.Sprintf("--- !u!%d &%s%s", classID, id, a.newline)), body: []byte(text)})
	return a.refresh()
}
func baseFields() map[string]any {
	return map[string]any{"m_ObjectHideFlags": int64(0), "m_CorrespondingSourceObject": reference("0"), "m_PrefabInstance": reference("0"), "m_PrefabAsset": reference("0")}
}
func (a *yamlAsset) rootDocument() *yamlDocument {
	for _, d := range a.docs {
		if d.classID == 1660057539 {
			return d
		}
	}
	return nil
}
func (a *yamlAsset) appendChild(parentTransform, id string) error {
	if parentTransform == "0" {
		if roots := a.rootDocument(); roots != nil {
			return a.setTopLevel(roots.id, "m_Roots", referenceList(append(idsFromList(roots.data["m_Roots"]), id)))
		}
		return nil
	}
	parent := a.find(parentTransform)
	if parent == nil {
		return failure("invalid_graph", "parent transform absent", nil)
	}
	return a.setTopLevel(parent.id, "m_Children", referenceList(append(idsFromList(parent.data["m_Children"]), id)))
}
func (a *yamlAsset) detachChild(parentTransform, id string) error {
	if parentTransform == "0" {
		if roots := a.rootDocument(); roots != nil {
			return a.setTopLevel(roots.id, "m_Roots", referenceList(removeID(idsFromList(roots.data["m_Roots"]), id)))
		}
		return nil
	}
	parent := a.find(parentTransform)
	if parent == nil {
		return failure("invalid_graph", "parent transform absent", nil)
	}
	return a.setTopLevel(parent.id, "m_Children", referenceList(removeID(idsFromList(parent.data["m_Children"]), id)))
}
func (a *yamlAsset) createObject(operation, key, name, parentID string) (string, string, error) {
	return a.createObjectWithTransform(operation, key, name, parentID, 4, nil)
}

func (a *yamlAsset) createObjectWithTransform(operation, key, name, parentID string, classID int, fields map[string]any) (string, string, error) {
	parentTransform := "0"
	if parentID != "0" {
		_, transform, err := a.plainObject(parentID)
		if err != nil {
			return "", "", err
		}
		parentTransform = transform.id
	}
	objectID, transformID := a.newID(operation, key), a.newID(operation, key+".transform")
	object := baseFields()
	object["serializedVersion"] = int64(6)
	object["m_Component"] = []any{map[string]any{"component": reference(transformID)}}
	object["m_Layer"] = int64(0)
	object["m_Name"] = name
	object["m_TagString"] = "Untagged"
	object["m_IsActive"] = int64(1)
	object["m_Icon"] = reference("0")
	object["m_NavMeshLayer"] = int64(0)
	object["m_StaticEditorFlags"] = int64(0)
	if err := a.appendDocument(1, objectID, "GameObject", object); err != nil {
		return "", "", err
	}
	transform := baseFields()
	transform["serializedVersion"] = int64(2)
	transform["m_GameObject"] = reference(objectID)
	transform["m_LocalRotation"] = map[string]any{"x": 0, "y": 0, "z": 0, "w": 1}
	transform["m_LocalPosition"] = map[string]any{"x": 0, "y": 0, "z": 0}
	transform["m_LocalScale"] = map[string]any{"x": 1, "y": 1, "z": 1}
	transform["m_ConstrainProportionsScale"] = 0
	transform["m_Children"] = []any{}
	transform["m_Father"] = reference(parentTransform)
	transform["m_LocalEulerAnglesHint"] = map[string]any{"x": 0, "y": 0, "z": 0}
	for key, value := range fields {
		transform[key] = value
	}
	typeName := "Transform"
	if classID == 224 {
		typeName = "RectTransform"
	}
	if err := a.appendDocument(classID, transformID, typeName, transform); err != nil {
		return "", "", err
	}
	if err := a.appendChild(parentTransform, transformID); err != nil {
		return "", "", err
	}
	return objectID, transformID, nil
}
func (a *yamlAsset) reparent(id, parentID string) (string, error) {
	_, transform, err := a.plainObject(id)
	if err != nil {
		return "", err
	}
	transformID := transform.id
	if err := a.requireResolvedSubtree(transformID); err != nil {
		return "", err
	}
	oldParent := idReference(transform.data["m_Father"])
	oldGameObject := "0"
	if parent := a.find(oldParent); parent != nil {
		oldGameObject = idReference(parent.data["m_GameObject"])
	}
	newParent := "0"
	if parentID != "0" {
		_, parent, err := a.plainObject(parentID)
		if err != nil {
			return "", err
		}
		newParent = parent.id
	}
	seen := map[string]bool{}
	for current := newParent; current != "0"; {
		if current == transformID || seen[current] {
			return "", failure("transform_cycle", "reparent would create a Transform cycle", nil)
		}
		seen[current] = true
		ancestor := a.find(current)
		if ancestor == nil {
			return "", failure("invalid_graph", "ancestor Transform absent", nil)
		}
		current = idReference(ancestor.data["m_Father"])
	}
	if oldParent == newParent {
		return oldGameObject, nil
	}
	if err := a.detachChild(oldParent, transformID); err != nil {
		return "", err
	}
	if err := a.appendChild(newParent, transformID); err != nil {
		return "", err
	}
	if err := a.setTopLevel(transformID, "m_Father", reference(newParent)); err != nil {
		return "", err
	}
	return oldGameObject, nil
}
func (a *yamlAsset) deleteObject(id string, recursive bool) ([]string, error) {
	_, transform, err := a.plainObject(id)
	if err != nil {
		return nil, err
	}
	if err := a.requireResolvedSubtree(transform.id); err != nil {
		return nil, err
	}
	if !recursive && len(idsFromList(transform.data["m_Children"])) != 0 {
		return nil, failure("has_children", "recursive deletion must be requested for GameObjects with children", nil)
	}
	removed := map[string]bool{}
	var collect func(string) error
	collect = func(objectID string) error {
		if removed[objectID] {
			return failure("transform_cycle", "cycle in deleted hierarchy", nil)
		}
		object, t, err := a.plainObject(objectID)
		if err != nil {
			return err
		}
		removed[objectID] = true
		components, _ := object.data["m_Component"].([]any)
		for _, entry := range components {
			mapping, _ := entry.(map[string]any)
			removed[idReference(mapping["component"])] = true
		}
		for _, childID := range idsFromList(t.data["m_Children"]) {
			child := a.find(childID)
			if child == nil {
				return failure("invalid_graph", "child Transform absent", nil)
			}
			if err := collect(idReference(child.data["m_GameObject"])); err != nil {
				return err
			}
		}
		return nil
	}
	if err := collect(id); err != nil {
		return nil, err
	}
	if err := a.detachChild(idReference(transform.data["m_Father"]), transform.id); err != nil {
		return nil, err
	}
	docs := make([]*yamlDocument, 0, len(a.docs))
	for _, d := range a.docs {
		if !removed[d.id] {
			docs = append(docs, d)
		}
	}
	a.docs = docs
	ids := make([]string, 0, len(removed))
	for id := range removed {
		ids = append(ids, id)
	}
	sort.Strings(ids)
	return ids, a.refresh()
}
func (a *yamlAsset) addComponent(operation, key, objectID, scriptGUID string, fields map[string]any) (string, error) {
	object, _, err := a.plainObject(objectID)
	if err != nil {
		return "", err
	}
	data := baseFields()
	data["m_GameObject"] = reference(objectID)
	data["m_Enabled"] = int64(1)
	data["m_EditorHideFlags"] = int64(0)
	data["m_Script"] = map[string]any{"fileID": "11500000", "guid": scriptGUID, "type": 3}
	data["m_Name"] = ""
	data["m_EditorClassIdentifier"] = ""
	for field, value := range fields {
		if _, reserved := data[field]; reserved || protectedProperty(field) || !regexpField.MatchString(field) {
			return "", failure("protected_property", "component fields cannot replace Unity identity or ownership fields", map[string]any{"property": field})
		}
		normalized, err := normalizeJSON(value)
		if err != nil {
			return "", err
		}
		data[field] = normalized
	}
	id := a.newID(operation, key)
	if err := a.appendDocument(114, id, "MonoBehaviour", data); err != nil {
		return "", err
	}
	components, _ := object.data["m_Component"].([]any)
	components = append(append([]any{}, components...), map[string]any{"component": reference(id)})
	if err := a.setTopLevel(objectID, "m_Component", components); err != nil {
		return "", err
	}
	return id, nil
}
func (a *yamlAsset) removeComponent(id string) error {
	component := a.find(id)
	if component == nil {
		return failure("not_found", "component fileID is absent", nil)
	}
	if component.classID == 1 || component.classID == 4 || component.classID == 224 || idReference(component.data["m_GameObject"]) == "0" {
		return failure("protected_property", "GameObject and Transform must use graph operations", nil)
	}
	object, _, err := a.plainObject(idReference(component.data["m_GameObject"]))
	if err != nil {
		return err
	}
	components, _ := object.data["m_Component"].([]any)
	remaining := []any{}
	for _, entry := range components {
		mapping, _ := entry.(map[string]any)
		if idReference(mapping["component"]) != id {
			remaining = append(remaining, entry)
		}
	}
	if err := a.setTopLevel(object.id, "m_Component", remaining); err != nil {
		return err
	}
	docs := make([]*yamlDocument, 0, len(a.docs))
	for _, d := range a.docs {
		if d.id != id {
			docs = append(docs, d)
		}
	}
	a.docs = docs
	return a.refresh()
}

func walkReferences(value any, visit func(map[string]any) error) error {
	switch v := value.(type) {
	case map[string]any:
		if _, isReference := v["fileID"]; isReference {
			return visit(v)
		}
		for _, child := range v {
			if err := walkReferences(child, visit); err != nil {
				return err
			}
		}
	case []any:
		for _, child := range v {
			if err := walkReferences(child, visit); err != nil {
				return err
			}
		}
	}
	return nil
}
func (a *yamlAsset) validate(assetGUID string) error {
	transforms := map[string]*yamlDocument{}
	for _, d := range a.docs {
		if err := walkReferences(d.data, func(ref map[string]any) error {
			id := idReference(ref)
			guid, _ := ref["guid"].(string)
			if id != "0" && (!externalReference(ref) || guid == assetGUID) && a.find(id) == nil {
				return failure("dangling_reference", "edit would leave a reference to an absent local object", map[string]any{"sourceFileId": d.id, "targetFileId": id})
			}
			return nil
		}); err != nil {
			return err
		}
		if d.classID == 4 || d.classID == 224 {
			transforms[d.id] = d
		}
		if d.classID == 1 && !d.stripped {
			components, _ := d.data["m_Component"].([]any)
			seen := map[string]bool{}
			transformCount := 0
			for _, entry := range components {
				mapping, _ := entry.(map[string]any)
				id := idReference(mapping["component"])
				component := a.find(id)
				if component == nil || seen[id] || (!component.stripped && idReference(component.data["m_GameObject"]) != d.id) {
					return failure("invalid_graph", "component ownership or identity is invalid", map[string]any{"gameObjectId": d.id, "componentId": id})
				}
				seen[id] = true
				if component.classID == 4 || component.classID == 224 {
					transformCount++
				}
			}
			if transformCount != 1 {
				return failure("invalid_graph", "GameObject requires exactly one Transform", map[string]any{"gameObjectId": d.id})
			}
		}
	}
	for id, t := range transforms {
		if t.stripped {
			continue
		}
		parent := idReference(t.data["m_Father"])
		if parent != "0" {
			p := transforms[parent]
			if p == nil {
				return failure("invalid_graph", "Transform parent is not a Transform", nil)
			}
			found := 0
			for _, child := range idsFromList(p.data["m_Children"]) {
				if child == id {
					found++
				}
			}
			if !p.stripped && found != 1 {
				return failure("invalid_graph", "Transform parent and children disagree", map[string]any{"fileId": id})
			}
		}
		seen := map[string]bool{}
		for current := id; current != "0"; {
			if seen[current] {
				return failure("transform_cycle", "Transform ancestry contains a cycle", nil)
			}
			seen[current] = true
			ancestor := transforms[current]
			if ancestor == nil {
				break
			}
			current = idReference(ancestor.data["m_Father"])
		}
		seen = map[string]bool{}
		for _, child := range idsFromList(t.data["m_Children"]) {
			c := transforms[child]
			if seen[child] || c == nil || (!c.stripped && idReference(c.data["m_Father"]) != id) {
				return failure("invalid_graph", "Transform children and parent disagree", map[string]any{"fileId": id, "childId": child})
			}
			seen[child] = true
		}
	}
	if roots := a.rootDocument(); roots != nil {
		seen := map[string]bool{}
		for _, root := range idsFromList(roots.data["m_Roots"]) {
			t := transforms[root]
			identity := root
			instance := a.find(root)
			if t != nil && t.stripped {
				instance = a.find(idReference(t.data["m_PrefabInstance"]))
			}
			if instance != nil && instance.classID == 1001 {
				modification, _ := instance.data["m_Modification"].(map[string]any)
				if idReference(modification["m_TransformParent"]) != "0" {
					return failure("invalid_graph", "SceneRoots contains a parented prefab instance", nil)
				}
				identity = "prefab:" + instance.id
			} else if t == nil || !t.stripped && idReference(t.data["m_Father"]) != "0" {
				return failure("invalid_graph", "SceneRoots contains an invalid root", nil)
			}
			if seen[identity] {
				return failure("invalid_graph", "SceneRoots contains a duplicate root identity", nil)
			}
			seen[identity] = true
		}
		for id, t := range transforms {
			if !t.stripped && idReference(t.data["m_Father"]) == "0" && !seen[id] {
				return failure("invalid_graph", "root Transform is absent from SceneRoots", map[string]any{"fileId": id})
			}
		}
		for _, instance := range a.docs {
			if instance.classID != 1001 {
				continue
			}
			modification, _ := instance.data["m_Modification"].(map[string]any)
			if idReference(modification["m_TransformParent"]) == "0" && !seen["prefab:"+instance.id] {
				return failure("invalid_graph", "root prefab instance is absent from SceneRoots", map[string]any{"fileId": instance.id})
			}
		}
	}
	return nil
}

// Unity intentionally omits GameObject/parent/children fields from stripped
// prefab documents. Preserve those records instead of treating omitted fields
// as zero or inventing ownership. Only the runtime prefab expander resolves them.
func (a *yamlAsset) preservePrefabDocuments(original *yamlAsset) error {
	return a.preservePrefabDocumentsExcept(original, nil)
}

func (a *yamlAsset) preservePrefabDocumentsExcept(original *yamlAsset, allowed map[string]bool) error {
	for _, before := range original.docs {
		if !before.stripped && before.classID != 1001 {
			continue
		}
		if before.classID == 1001 && allowed[before.id] {
			continue
		}
		after := a.find(before.id)
		if after == nil || string(after.header) != string(before.header) || string(after.body) != string(before.body) {
			return failure("unsupported_prefab_write", "prefab instance and stripped records must remain byte-for-byte unchanged", map[string]any{"fileId": before.id})
		}
	}
	return nil
}
func (a *yamlAsset) requireResolvedSubtree(transformID string) error {
	seen := map[string]bool{}
	var visit func(string) error
	visit = func(id string) error {
		if seen[id] {
			return failure("transform_cycle", "cycle in authored Transform subtree", nil)
		}
		seen[id] = true
		transform := a.find(id)
		if transform == nil {
			return failure("invalid_graph", "Transform subtree references an absent object", nil)
		}
		if transform.stripped || idReference(transform.data["m_PrefabInstance"]) != "0" {
			return failure("unsupported_prefab_write", "structural changes to a subtree containing unresolved prefab instances are unsupported", map[string]any{"fileId": id})
		}
		for _, child := range idsFromList(transform.data["m_Children"]) {
			if err := visit(child); err != nil {
				return err
			}
		}
		return nil
	}
	return visit(transformID)
}
func (s *Store) checkExternalReferences(source Source, removed map[string]bool) error {
	return s.checkExternalReferencesOverlay(source, removed, nil)
}

func (s *Store) checkExternalReferencesOverlay(source Source, removed map[string]bool, overlay map[string]Source) error {
	info, err := s.inspect()
	if err != nil {
		return err
	}
	paths := append(append([]string{}, info.Scenes...), info.Prefabs...)
	// Include arbitrary text materials and ScriptableObjects, not just scene graphs.
	err = filepath.WalkDir(filepath.Join(s.root, "Assets"), func(path string, entry os.DirEntry, walkErr error) error {
		if walkErr != nil {
			return walkErr
		}
		if entry.IsDir() || entry.Type()&os.ModeSymlink != 0 {
			return nil
		}
		if ext := strings.ToLower(filepath.Ext(path)); ext == ".mat" || ext == ".asset" {
			relative, _ := filepath.Rel(s.root, path)
			paths = append(paths, filepath.ToSlash(relative))
		}
		return nil
	})
	if err != nil {
		return err
	}
	for path := range overlay {
		if filepath.Ext(path) != ".cs" {
			paths = append(paths, path)
		}
	}
	seen := map[string]bool{}
	for _, path := range paths {
		if path == source.Path || seen[path] {
			continue
		}
		seen[path] = true
		other, exists := overlay[path]
		if !exists {
			var err error
			other, err = s.readSource(path)
			if err != nil {
				return err
			}
		}
		if !strings.Contains(other.Content, source.GUID) {
			continue
		}
		asset, err := parseAsset([]byte(other.Content))
		if err != nil {
			return err
		}
		for _, doc := range asset.docs {
			if err := walkReferences(doc.data, func(ref map[string]any) error {
				guid, _ := ref["guid"].(string)
				if guid == source.GUID && removed[idReference(ref)] {
					return failure("referenced_object", "deleted object is referenced by another authored asset", map[string]any{"path": path, "sourceFileId": doc.id, "targetFileId": idReference(ref)})
				}
				return nil
			}); err != nil {
				return err
			}
		}
	}
	return nil
}
