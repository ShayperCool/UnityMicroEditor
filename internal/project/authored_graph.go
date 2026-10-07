package goproject

import (
	"fmt"
	"path/filepath"
	"sort"
)

// AuthoredGraph exposes the same recursive authored prefab expansion used by
// scene preparation. It does not construct a runtime manifest or import native
// model prefabs; those require a prepared Session with a verified native catalog.
type AuthoredGraph struct {
	Path                             string                     `json:"path"`
	GUID                             string                     `json:"guid"`
	Objects                          []AuthoredGraphObject      `json:"objects"`
	LocalIDs                         map[string]string          `json:"localIds"`
	Dependencies                     []AuthoredDependency       `json:"dependencies"`
	IgnoredPrefabOverrideDiagnostics []PrefabOverrideDiagnostic `json:"ignoredPrefabOverrideDiagnostics"`
}

type AuthoredIdentity struct {
	GUID   string `json:"guid"`
	FileID string `json:"fileId"`
}

type AuthoredGraphObject struct {
	Document
	SourcePath string             `json:"sourcePath"`
	GUID       string             `json:"guid"`
	Identities []AuthoredIdentity `json:"identities"`
}

type AuthoredDependency struct {
	Path   string `json:"path"`
	SHA256 string `json:"sha256"`
}

func ResolveAuthoredGraph(project, path string) (AuthoredGraph, error) {
	if err := validateAuthoredPath(path, filepath.Ext(path)); err != nil || filepath.Ext(path) != ".unity" && filepath.Ext(path) != ".prefab" {
		return AuthoredGraph{}, fmt.Errorf("authored graph requires a normalized Assets scene or prefab")
	}
	root, err := filepath.Abs(project)
	if err != nil {
		return AuthoredGraph{}, err
	}
	root, err = filepath.EvalSymlinks(root)
	if err != nil {
		return AuthoredGraph{}, err
	}
	s := &Session{project: root, files: map[string]fileRecord{}, documents: map[string]yamlRecord{}, observed: map[string]identity{}, byGUID: map[string]string{}, byPath: map[string]string{}, models: map[string][]Document{}, assets: map[string]map[string]any{}, scriptTypes: map[string]string{}, dependencies: map[string]dependency{}, critical: map[string]string{}, prefabObservations: map[string]map[string]any{}, ignoredOverrides: map[string]bool{}, explicitSources: map[string]string{}, nativeOverlays: map[string]NativeReplacement{}, indexDirty: true}
	return s.AuthoredGraph(path)
}

func (s *Session) AuthoredGraph(path string) (AuthoredGraph, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if err := validateAuthoredPath(path, filepath.Ext(path)); err != nil || filepath.Ext(path) != ".unity" && filepath.Ext(path) != ".prefab" {
		return AuthoredGraph{}, fmt.Errorf("authored graph requires a normalized Assets scene or prefab")
	}
	if err := s.indexGUIDs(); err != nil {
		return AuthoredGraph{}, err
	}
	previousDependencies := s.dependencies
	previousObserved := s.observed
	previousDiagnostics := s.ignoredPrefabOverrideDiagnostics
	s.dependencies = map[string]dependency{}
	s.observed = map[string]identity{}
	s.ignoredPrefabOverrideDiagnostics = nil
	defer func() {
		s.dependencies = previousDependencies
		s.observed = previousObserved
		s.ignoredPrefabOverrideDiagnostics = previousDiagnostics
	}()
	// A prepared Session can also inspect native-model instances through its
	// validated catalog, without publishing or loading native AssetBundle scenes.
	objects, mapping, err := s.expand(path, "", nil)
	if err != nil {
		return AuthoredGraph{}, err
	}
	result := AuthoredGraph{Path: path, GUID: s.byPath[path], Objects: []AuthoredGraphObject{}, LocalIDs: mapping, Dependencies: []AuthoredDependency{}}
	result.IgnoredPrefabOverrideDiagnostics = s.sortedIgnoredPrefabOverrideDiagnostics()
	for _, value := range objects {
		object := AuthoredGraphObject{Document: value.Document, SourcePath: value.SourcePath, GUID: value.GUID, Identities: []AuthoredIdentity{}}
		for target := range value.Targets {
			for i, char := range target {
				if char == '/' {
					object.Identities = append(object.Identities, AuthoredIdentity{GUID: target[:i], FileID: target[i+1:]})
					break
				}
			}
		}
		sort.Slice(object.Identities, func(i, j int) bool {
			if object.Identities[i].GUID != object.Identities[j].GUID {
				return object.Identities[i].GUID < object.Identities[j].GUID
			}
			return object.Identities[i].FileID < object.Identities[j].FileID
		})
		result.Objects = append(result.Objects, object)
	}
	for _, value := range s.dependencies {
		result.Dependencies = append(result.Dependencies, AuthoredDependency{Path: value.Path, SHA256: value.SHA256})
	}
	sort.Slice(result.Dependencies, func(i, j int) bool { return result.Dependencies[i].Path < result.Dependencies[j].Path })
	return result, nil
}

// ApplyAuthoredOverride exposes the exact typed override semantics used by the
// runtime expander for authoring validation. The supplied map is modified in
// memory; this function performs no file writes.
func ApplyAuthoredOverride(data map[string]any, path string, value any, reference map[string]any) error {
	return applyOverride(data, path, value, reference)
}
