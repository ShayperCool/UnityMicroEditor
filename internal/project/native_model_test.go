package goproject

import (
	"errors"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

func newNativeModelFixture(t *testing.T) (*Session, NativeReplacement, string) {
	t.Helper()
	s, output, replacement, _, _ := nativeFixture(t, ".fbx")
	s.assets = map[string]map[string]any{}
	s.catalog.Assets = nil
	s.catalog.ModelPrefabs = nil
	s.models = map[string][]Document{}
	s.baseline["assets"] = []any{}
	replacement.ModelGraphSHA256 = nativeModelGraphSHA256(replacement.ModelGraph)
	return s, replacement, output
}

func TestNewStaticNativeModelPublishesVerifiedHierarchyAndReplaysAsPrefab(t *testing.T) {
	s, replacement, output := newNativeModelFixture(t)
	s.dependencies = map[string]dependency{}
	_, err := s.documentSource(replacement.SourcePath)
	var needs *NeedsImportError
	if !errors.As(err, &needs) || needs.Path != replacement.SourcePath {
		t.Fatalf("missing model did not request importer: %v", err)
	}
	before := nativeModelGraphSHA256(replacement.ModelGraph)
	if err := s.ReplaceNativeContent(replacement); err != nil {
		t.Fatal(err)
	}
	if len(s.models[replacement.GUID]) != 2 {
		t.Fatal("new source model hierarchy was not registered")
	}
	documents, err := s.documentSource(replacement.SourcePath)
	if err != nil || len(documents) != 2 {
		t.Fatalf("new native model cannot expand: %v", err)
	}
	if str(documents[0].Data["m_Name"]) != "Original" || fileID(object(array(documents[0].Data["m_Component"])[0])["component"]) != "11" {
		t.Fatal("native hierarchy contents were changed")
	}
	graph, err := s.nativeModelGraph(replacement.GUID)
	if err != nil || matchNativeModelGraph(replacement.ModelGraph, graph) != nil {
		t.Fatalf("projection did not round-trip native fields: %v", err)
	}
	replacement.ModelGraph[0].Scalars["m_Name"] = "caller mutation"
	if str(s.models[replacement.GUID][0].Data["m_Name"]) != "Original" {
		t.Fatal("caller retained mutable ownership of accepted hierarchy")
	}
	// The scene consumes an imported source model using its native local IDs.
	scene := filepath.Join(s.project, "Assets/Scenes/Sample.unity")
	writeFixtureFile(t, scene, `--- !u!1001 &100
PrefabInstance:
  m_SourcePrefab: {fileID: 100100000, guid: 22222222222222222222222222222222, type: 3}
  m_Modification:
    m_TransformParent: {fileID: 0}
    m_Modifications: []
--- !u!4 &200 stripped
Transform:
  m_CorrespondingSourceObject: {fileID: 11, guid: 22222222222222222222222222222222, type: 3}
  m_PrefabInstance: {fileID: 100}
--- !u!1660057539 &9223372036854775807
SceneRoots:
  m_Roots:
  - {fileID: 200}
`)
	prepared, err := s.Prepare(output)
	if err != nil || prepared.Objects != 3 {
		t.Fatalf("new native FBX scene failed: %+v %v", prepared, err)
	}
	if before != nativeModelGraphSHA256(graph) {
		t.Fatal("accepted model topology/scalars changed")
	}
}

func TestNewNativeModelRejectsTamperingAndInvalidHierarchyBeforePublication(t *testing.T) {
	cases := map[string]func(*NativeReplacement){
		"graph digest": func(r *NativeReplacement) { r.ModelGraph[0].Scalars["m_Name"] = "tampered" },
		"local owner": func(r *NativeReplacement) {
			r.ModelGraph[1].References[0].Targets[0].FileID = "999"
			r.ModelGraphSHA256 = nativeModelGraphSHA256(r.ModelGraph)
		},
		"duplicate component": func(r *NativeReplacement) {
			r.ModelGraph[0].References[0].Targets = append(r.ModelGraph[0].References[0].Targets, r.ModelGraph[0].References[0].Targets[0])
			r.ModelGraphSHA256 = nativeModelGraphSHA256(r.ModelGraph)
		},
		"cyclic parent": func(r *NativeReplacement) {
			r.ModelGraph[1].References[2].Targets[0].FileID = "11"
			r.ModelGraphSHA256 = nativeModelGraphSHA256(r.ModelGraph)
		},
		"unknown scalar": func(r *NativeReplacement) {
			r.ModelGraph[0].Scalars["invented"] = 0
			r.ModelGraphSHA256 = nativeModelGraphSHA256(r.ModelGraph)
		},
		"unknown object": func(r *NativeReplacement) {
			r.Content.Objects = append(r.Content.Objects, NativeContentObject{FileID: "90", TypeName: "UnityEngine.Avatar"})
		},
		"unresolved external": func(r *NativeReplacement) {
			r.ModelGraph[1].References[0].Targets[0].GUID = "33333333333333333333333333333333"
			r.ModelGraphSHA256 = nativeModelGraphSHA256(r.ModelGraph)
		},
		"invalid layer": func(r *NativeReplacement) {
			r.ModelGraph[0].Scalars["m_Layer"] = 32
			r.ModelGraphSHA256 = nativeModelGraphSHA256(r.ModelGraph)
		},
	}
	for name, mutate := range cases {
		t.Run(name, func(t *testing.T) {
			s, replacement, _ := newNativeModelFixture(t)
			critical := clone(s.critical)
			mutate(&replacement)
			if err := s.ReplaceNativeContent(replacement); err == nil {
				t.Fatal("invalid new model was accepted")
			}
			if len(s.models) != 0 || len(s.nativeOverlays) != 0 || len(s.assets) != 0 || !reflect.DeepEqual(critical, clone(s.critical)) {
				t.Fatal("rejected hierarchy changed session state")
			}
		})
	}
}

func TestNewNativeModelPreservesChildOrderAndRejectsAsymmetricLinks(t *testing.T) {
	s, replacement, _ := newNativeModelFixture(t)
	rootTransform := replacement.ModelGraph[1]
	childGO := replacement.ModelGraph[0]
	childGO.FileID = "20"
	childGO.Scalars = clone(childGO.Scalars).(map[string]any)
	childGO.Scalars["m_Name"] = "Child"
	childGO.References = []NativeModelGraphReference{{Property: "m_Component", Targets: []NativeModelGraphTarget{{FileID: "21"}}}}
	child := rootTransform
	child.FileID = "21"
	child.Scalars = clone(child.Scalars).(map[string]any)
	child.References = []NativeModelGraphReference{{Property: "m_GameObject", Targets: []NativeModelGraphTarget{{FileID: "20"}}}, {Property: "m_Children", Targets: []NativeModelGraphTarget{}}, {Property: "m_Father", Targets: []NativeModelGraphTarget{{FileID: "11"}}}}
	rootTransform.References = []NativeModelGraphReference{{Property: "m_GameObject", Targets: []NativeModelGraphTarget{{FileID: "10"}}}, {Property: "m_Children", Targets: []NativeModelGraphTarget{{FileID: "21"}}}, {Property: "m_Father", Targets: []NativeModelGraphTarget{{FileID: "0"}}}}
	replacement.ModelGraph[1] = rootTransform
	replacement.ModelGraph = append(replacement.ModelGraph, childGO, child)
	replacement.Content.Objects = append(replacement.Content.Objects, NativeContentObject{FileID: "20", TypeName: "UnityEngine.GameObject"}, NativeContentObject{FileID: "21", TypeName: "UnityEngine.Transform"})
	replacement.ModelGraphSHA256 = nativeModelGraphSHA256(replacement.ModelGraph)
	docs, err := s.staticNativeModelDocuments(replacement)
	if err != nil || fileID(array(docs[1].Data["m_Children"])[0]) != "21" {
		t.Fatalf("authored child order changed: %v", err)
	}
	replacement.ModelGraph[1].References[1].Targets = nil
	if _, err := s.staticNativeModelDocuments(replacement); err == nil || !strings.Contains(err.Error(), "parent omits") {
		t.Fatalf("asymmetric links accepted: %v", err)
	}
}

func TestNativeModelEditorEulerHintDoesNotChangeRuntimeRotationOrFingerprint(t *testing.T) {
	s, replacement, _ := newNativeModelFixture(t)
	documents, err := s.staticNativeModelDocuments(replacement)
	if err != nil {
		t.Fatal(err)
	}
	before := clone(documents[1].Data["m_LocalRotation"])
	if err := applyOverride(documents[1].Data, "m_LocalEulerAnglesHint.x", "180", map[string]any{"fileID": int64(0)}); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(before, documents[1].Data["m_LocalRotation"]) || number(object(documents[1].Data["m_LocalEulerAnglesHint"])["x"]) != 180 {
		t.Fatal("Inspector hint changed authoritative native rotation")
	}
	s.models[replacement.GUID] = documents
	graph, err := s.nativeModelGraph(replacement.GUID)
	if err != nil || matchNativeModelGraph(replacement.ModelGraph, graph) != nil {
		t.Fatalf("Inspector hint contaminated native graph fingerprint: %v", err)
	}
}
