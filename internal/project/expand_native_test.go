package goproject

import (
	"fmt"
	"path/filepath"
	"strings"
	"testing"
)

func TestReimportedModelIgnoresOnlyOverridesForProvenRemovedObjects(t *testing.T) {
	for _, target := range []string{"-5824371076177060002", "4300000"} {
		t.Run(target, func(t *testing.T) {
			s, replacement, output := newNativeModelFixture(t)
			s.dependencies = map[string]dependency{}
			if err := s.ReplaceNativeContent(replacement); err != nil {
				t.Fatal(err)
			}
			writeFixtureFile(t, filepath.Join(s.project, "Assets/Scenes/Sample.unity"), fmt.Sprintf(`--- !u!1001 &100
PrefabInstance:
  m_SourcePrefab: {fileID: 100100000, guid: 22222222222222222222222222222222, type: 3}
  m_Modification:
    m_TransformParent: {fileID: 0}
    m_Modifications:
    - target: {fileID: %s, guid: 22222222222222222222222222222222, type: 3}
      propertyPath: m_Name
      value: Removed object
      objectReference: {fileID: 0}
    - target: {fileID: 10, guid: 22222222222222222222222222222222, type: 3}
      propertyPath: m_Name
      value: Current instance
      objectReference: {fileID: 0}
`, target))
			_, err := s.Prepare(output)
			if target == "4300000" {
				if err == nil || !strings.Contains(err.Error(), "unresolved prefab override") {
					t.Fatalf("existing native mesh override must remain rejected: %v", err)
				}
				return
			}
			if err != nil {
				t.Fatalf("proven stale override prevented loading current model: %v", err)
			}
			objects, _, err := s.expand("Assets/Scenes/Sample.unity", "", nil)
			if err != nil || len(objects) != 2 || str(objects[0].Data["m_Name"]) != "Current instance" {
				t.Fatalf("current object override was lost: %+v, %v", objects, err)
			}
		})
	}
}

func TestFreshImportedModelKeepsSignedNativeMeshIdentityOutsideAuthoredGraph(t *testing.T) {
	s, replacement, _ := newNativeModelFixture(t)
	s.dependencies = map[string]dependency{}
	meshID, filterID := "-2986133483422379211", "-5459214052098992906"
	replacement.Content.Objects[0].FileID = meshID
	replacement.Content.Objects = append(replacement.Content.Objects, NativeContentObject{FileID: filterID, TypeName: "UnityEngine.MeshFilter"})
	replacement.ModelGraph[0].References[0].Targets = append(replacement.ModelGraph[0].References[0].Targets, NativeModelGraphTarget{FileID: filterID})
	replacement.ModelGraph = append(replacement.ModelGraph, NativeModelGraphObject{FileID: filterID, ClassID: 33, Scalars: map[string]any{}, References: []NativeModelGraphReference{{Property: "m_GameObject", Targets: []NativeModelGraphTarget{{FileID: "10"}}}, {Property: "m_Mesh", Targets: []NativeModelGraphTarget{{FileID: meshID}}}}})
	replacement.ModelGraphSHA256 = nativeModelGraphSHA256(replacement.ModelGraph)
	if err := s.ReplaceNativeContent(replacement); err != nil {
		t.Fatal(err)
	}
	documents, err := s.documentSource(replacement.SourcePath)
	if err != nil {
		t.Fatal(err)
	}
	found := false
	for _, document := range documents {
		if document.FileID != filterID {
			continue
		}
		mesh := object(document.Data["m_Mesh"])
		if fileID(mesh) != meshID || str(mesh["guid"]) != replacement.GUID || str(object(document.Data["m_GameObject"])["guid"]) != "" {
			t.Fatalf("fresh mesh reference/local owner remapped incorrectly: %+v", document.Data)
		}
		found = true
	}
	if !found {
		t.Fatal("verified MeshFilter record disappeared")
	}
	// Cached source documents remain local-ID native data; GUID qualification is
	// applied only to the copy used by this authored scene/prefab expansion.
	if str(object(s.models[replacement.GUID][2].Data["m_Mesh"])["guid"]) != "" {
		t.Fatal("model cache changed during expansion")
	}
}
