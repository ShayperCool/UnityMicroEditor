package authoring

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"sort"
	"strings"

	goproject "unitymicroeditor/project"
)

type PrefabEditRequest struct {
	Action           string                  `json:"action"`
	Path             string                  `json:"path"`
	ExpectedRevision string                  `json:"expectedRevision"`
	OperationID      string                  `json:"operationId"`
	FileID           string                  `json:"fileId,omitempty"`
	TargetPath       string                  `json:"targetPath,omitempty"`
	PrefabPath       string                  `json:"prefabPath,omitempty"`
	OutputPath       string                  `json:"outputPath,omitempty"`
	ParentFileID     string                  `json:"parentFileId,omitempty"`
	ParentPath       string                  `json:"parentPath,omitempty"`
	Name             string                  `json:"name,omitempty"`
	Completely       bool                    `json:"completely,omitempty"`
	ComponentType    string                  `json:"componentType,omitempty"`
	Property         string                  `json:"property,omitempty"`
	Value            any                     `json:"value,omitempty"`
	Fields           map[string]any          `json:"fields,omitempty"`
	DefaultsTemplate *NativeDefaultsTemplate `json:"defaultsTemplate,omitempty"`
	BeforePublish    func() error            `json:"-"`
}

type PrefabEditResult struct {
	Backend       string            `json:"backend"`
	OperationID   string            `json:"operationId"`
	Action        string            `json:"action"`
	Publication   PublicationResult `json:"publication"`
	CreatedIDs    map[string]string `json:"createdIds,omitempty"`
	GeneratedPath string            `json:"generatedPath,omitempty"`
}

type prefabSourcePlan struct {
	SchemaVersion int                      `json:"schemaVersion"`
	Fingerprint   string                   `json:"fingerprint"`
	Request       PrefabEditRequest        `json:"request"`
	Publication   StagedPublicationRequest `json:"publication"`
	CreatedIDs    map[string]string        `json:"createdIds,omitempty"`
}

// PrefabEdit plans only authored serialization. Immutable plans and the existing
// publication journal allow exact operation replay without reopening Unity.
func (s *Store) PrefabEdit(ctx context.Context, request PrefabEditRequest) (result PrefabEditResult, err error) {
	if _, err := s.assetPath(request.Path, ".unity", ".prefab"); err != nil {
		return result, err
	}
	if request.OperationID == "" || len(request.OperationID) > 128 || strings.TrimSpace(request.OperationID) != request.OperationID || request.ExpectedRevision == "" {
		return result, failure("invalid_request", "prefab source authoring requires operationId and expectedRevision", nil)
	}
	switch request.Action {
	case "instantiate", "create_prefab", "create_variant", "set_override", "apply", "revert", "unpack", "add_component":
	default:
		return result, failure("unsupported_operation", "unsupported source prefab operation", nil)
	}
	encoded, err := json.Marshal(request)
	if err != nil {
		return result, err
	}
	hash := sha256.Sum256(encoded)
	fingerprint := hex.EncodeToString(hash[:])
	idHash := sha256.Sum256([]byte(request.OperationID))
	var plan prefabSourcePlan
	err = s.locked(ctx, func() error {
		relative, _ := filepath.Rel(s.root, s.state)
		directory, err := s.safePath(filepath.ToSlash(relative)+"/prefab-plans", true)
		if err != nil {
			return err
		}
		if err := os.MkdirAll(directory, 0700); err != nil {
			return err
		}
		filename, err := s.safePath(filepath.ToSlash(relative)+"/prefab-plans/"+hex.EncodeToString(idHash[:])+".json", false)
		if err != nil {
			return err
		}
		file, readErr := os.Open(filename)
		if readErr == nil {
			defer file.Close()
			info, err := file.Stat()
			if err != nil || !info.Mode().IsRegular() || info.Size() > 96<<20 {
				return failure("invalid_plan", "prefab plan exceeds bounded regular source plan profile", nil)
			}
			data, err := io.ReadAll(io.LimitReader(file, 96<<20+1))
			if err != nil {
				return err
			}
			if len(data) > 96<<20 || json.Unmarshal(data, &plan) != nil || plan.SchemaVersion != 1 || plan.Publication.OperationID != request.OperationID {
				return failure("invalid_plan", "invalid durable prefab source plan", nil)
			}
			if plan.Fingerprint != fingerprint {
				return failure("idempotency_conflict", "operationId was already used for another prefab request", nil)
			}
			return nil
		}
		if !errors.Is(readErr, os.ErrNotExist) {
			return readErr
		}
		planner, err := s.newPrefabPlanner(request)
		if err != nil {
			return err
		}
		if err = planner.build(request); err != nil {
			return err
		}
		plan = prefabSourcePlan{SchemaVersion: 1, Fingerprint: fingerprint, Request: request, Publication: planner.publication(request.OperationID), CreatedIDs: planner.created}
		data, err := json.Marshal(plan)
		if err != nil {
			return err
		}
		if len(data) > 96<<20 {
			return failure("response_too_large", "prefab source plan exceeds96 MiB", nil)
		}
		return atomicWrite(filename, append(data, '\n'), 0600)
	})
	if err != nil {
		return result, err
	}
	// The callback is deliberately not persisted. Each caller/retry must bind
	// the current external native-default context immediately before writing.
	plan.Publication.BeforePublish = request.BeforePublish
	publication, err := s.PublishStaged(ctx, plan.Publication)
	result = PrefabEditResult{Backend: "raw_source", OperationID: request.OperationID, Action: request.Action, Publication: publication, CreatedIDs: plan.CreatedIDs, GeneratedPath: request.OutputPath}
	return result, err
}

type prefabPlanner struct {
	store     *Store
	source    Source
	asset     *yamlAsset
	graph     goproject.AuthoredGraph
	revisions map[string]string
	objects   map[string]*goproject.AuthoredGraphObject
	inputs    map[string]string
	outputs   map[string]PublicationOutput
	created   map[string]string
}

func (s *Store) newPrefabPlanner(request PrefabEditRequest) (*prefabPlanner, error) {
	source, err := s.readSource(request.Path)
	if err != nil {
		return nil, err
	}
	if source.Revision != request.ExpectedRevision {
		return nil, failure("revision_conflict", "prefab source or metadata changed", map[string]any{"actualRevision": source.Revision})
	}
	asset, err := parseAsset([]byte(source.Content))
	if err != nil {
		return nil, err
	}
	graph, revisions, _, err := s.resolvedGraph(request.Path)
	if err != nil {
		return nil, err
	}
	if revisions[request.Path] != source.Revision {
		return nil, failure("revision_conflict", "source changed during prefab resolution", nil)
	}
	p := &prefabPlanner{store: s, source: source, asset: asset, graph: graph, revisions: revisions, objects: map[string]*goproject.AuthoredGraphObject{}, inputs: map[string]string{}, outputs: map[string]PublicationOutput{}, created: map[string]string{}}
	for path, revision := range revisions {
		p.inputs[path] = revision
	}
	for i := range p.graph.Objects {
		value := &p.graph.Objects[i]
		p.objects[value.FileID] = value
	}
	return p, nil
}
func (p *prefabPlanner) publication(id string) StagedPublicationRequest {
	result := StagedPublicationRequest{OperationID: id}
	keys := []string{}
	for path := range p.inputs {
		keys = append(keys, path)
	}
	sort.Strings(keys)
	for _, path := range keys {
		result.Inputs = append(result.Inputs, PublicationInput{Path: path, ExpectedRevision: p.inputs[path]})
	}
	keys = nil
	for path := range p.outputs {
		keys = append(keys, path)
	}
	sort.Strings(keys)
	for _, path := range keys {
		result.Outputs = append(result.Outputs, p.outputs[path])
	}
	return result
}
func (p *prefabPlanner) writeAsset(path string, asset *yamlAsset, metadata *string) error {
	if _, err := p.store.assetPath(path, ".unity", ".prefab"); err != nil {
		return err
	}
	if p.inputs[path] == "" {
		revision, err := p.store.dependencyRevision(path)
		if err != nil {
			return err
		}
		p.inputs[path] = revision
	}
	p.outputs[path] = PublicationOutput{Path: path, Content: string(asset.bytes()), Metadata: metadata}
	return nil
}
func (p *prefabPlanner) includeGraph(path string) (goproject.AuthoredGraph, map[string]string, error) {
	graph, revisions, _, err := p.store.resolvedGraph(path)
	if err != nil {
		return graph, nil, err
	}
	for path, revision := range revisions {
		if old := p.inputs[path]; old != "" && old != revision {
			return graph, nil, failure("revision_conflict", "prefab dependencies changed while planning", nil)
		}
		p.inputs[path] = revision
	}
	return graph, revisions, nil
}
func (p *prefabPlanner) target(fileID, path string) (*goproject.AuthoredGraphObject, error) {
	var found *goproject.AuthoredGraphObject
	for _, value := range p.graph.Objects {
		object := p.objects[value.FileID]
		if object.ClassID != 1 {
			continue
		}
		matches := false
		if fileID != "" {
			expanded := p.graph.LocalIDs[fileID]
			matches = object.FileID == fileID || expanded != "" && object.FileID == expanded
		}
		if path != "" {
			logical, err := p.objectPath(object.FileID, map[string]bool{})
			if err != nil {
				return nil, err
			}
			if fileID != "" {
				matches = matches && logical == path
			} else {
				matches = logical == path
			}
		}
		if matches {
			if found != nil {
				return nil, failure("ambiguous_target", "prefab target path/fileID identifies multiple GameObjects", nil)
			}
			found = object
		}
	}
	if found == nil {
		return nil, failure("not_found", "prefab target requires an existing GameObject fileId or targetPath", nil)
	}
	return found, nil
}
func (p *prefabPlanner) transform(owner string) *goproject.AuthoredGraphObject {
	for _, value := range p.graph.Objects {
		if (value.ClassID == 4 || value.ClassID == 224) && idReference(value.Data["m_GameObject"]) == owner {
			return p.objects[value.FileID]
		}
	}
	return nil
}
func (p *prefabPlanner) objectPath(owner string, seen map[string]bool) (string, error) {
	if seen[owner] {
		return "", failure("invalid_graph", "cycle while resolving prefab target path", nil)
	}
	seen[owner] = true
	object := p.objects[owner]
	if object == nil || object.ClassID != 1 {
		return "", failure("invalid_graph", "prefab hierarchy owner is absent", nil)
	}
	name, _ := object.Data["m_Name"].(string)
	transform := p.transform(owner)
	if transform == nil {
		return "", failure("invalid_graph", "prefab GameObject lacks transform", nil)
	}
	parent := idReference(transform.Data["m_Father"])
	if parent == "0" {
		return name, nil
	}
	parentTransform := p.objects[parent]
	if parentTransform == nil {
		return "", failure("invalid_graph", "prefab parent transform is absent", nil)
	}
	prefix, err := p.objectPath(idReference(parentTransform.Data["m_GameObject"]), seen)
	return prefix + "/" + name, err
}
func (p *prefabPlanner) selected(owner string) (map[string]bool, error) {
	selected := map[string]bool{}
	var visit func(string) error
	visit = func(owner string) error {
		if selected[owner] {
			return failure("invalid_graph", "duplicate or cyclic prefab subtree", nil)
		}
		selected[owner] = true
		object := p.objects[owner]
		if object == nil || object.ClassID != 1 {
			return failure("invalid_graph", "prefab child owner is absent", nil)
		}
		for _, id := range componentIDs(object.Data) {
			if p.objects[id] == nil {
				return failure("invalid_graph", "prefab component is absent", nil)
			}
			selected[id] = true
		}
		t := p.transform(owner)
		if t == nil {
			return failure("invalid_graph", "prefab transform is absent", nil)
		}
		for _, child := range idsFromList(t.Data["m_Children"]) {
			transform := p.objects[child]
			if transform == nil {
				return failure("invalid_graph", "prefab child transform is absent", nil)
			}
			if err := visit(idReference(transform.Data["m_GameObject"])); err != nil {
				return err
			}
		}
		return nil
	}
	return selected, visit(owner)
}
func componentIDs(data map[string]any) []string {
	result := []string{}
	items, _ := data["m_Component"].([]any)
	for _, item := range items {
		mapping, _ := item.(map[string]any)
		result = append(result, idReference(mapping["component"]))
	}
	return result
}
func (p *prefabPlanner) instance(owner *goproject.AuthoredGraphObject) (*yamlDocument, error) {
	prefix := strings.Split(owner.FileID, ":")[0]
	if doc := p.asset.find(prefix); doc != nil && doc.classID == 1001 {
		return doc, nil
	}
	for _, local := range p.asset.docs {
		if local.classID == 1 && p.graph.LocalIDs[local.id] == owner.FileID {
			if instance := p.asset.find(idReference(local.data["m_PrefabInstance"])); instance != nil && instance.classID == 1001 {
				return instance, nil
			}
		}
	}
	return nil, failure("not_prefab_instance", "target is not part of an authored prefab instance", nil)
}
func sourceIdentity(object *goproject.AuthoredGraphObject, guid string) (string, error) {
	id := ""
	for _, identity := range object.Identities {
		if identity.GUID == guid {
			if id != "" && id != identity.FileID {
				return "", failure("ambiguous_identity", "prefab source has multiple local aliases for one target", nil)
			}
			id = identity.FileID
		}
	}
	if id == "" {
		return "", failure("invalid_identity", "target lacks the requested prefab source identity", map[string]any{"guid": guid, "fileId": object.FileID})
	}
	return id, nil
}
func (p *prefabPlanner) build(request PrefabEditRequest) error {
	switch request.Action {
	case "instantiate":
		return p.instantiate(request)
	case "create_variant":
		return p.createVariant(request)
	case "create_prefab":
		return p.createPrefab(request)
	case "set_override":
		return p.setOverride(request)
	case "apply", "revert":
		return p.applyRevert(request)
	case "unpack":
		return p.unpack(request)
	case "add_component":
		return p.addComponent(request)
	}
	return fmt.Errorf("unsupported prefab action")
}
