package goproject

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

const authoredMaterialFixture = `%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!21 &2100000
Material:
  serializedVersion: 8
  m_Name: Authored
  m_Parent: {fileID: 0}
  m_Shader: {fileID: 4800000, guid: 33333333333333333333333333333333, type: 3}
  m_SavedProperties:
    m_TexEnvs:
    - _BaseMap:
        m_Texture: {fileID: 2800000, guid: 44444444444444444444444444444444, type: 3}
        m_Scale: {x: 2, y: 3}
        m_Offset: {x: 0.25, y: 0.5}
    m_Ints: []
    m_Floats:
    - _Metallic: 0.35
    m_Colors:
    - _EmissionColor: {r: 3, g: 2, b: 1, a: 1}
--- !u!114 &2561904340540659671
MonoBehaviour:
  m_EditorClassIdentifier: Unity.RenderPipelines.Universal.Editor::UnityEditor.Rendering.Universal.AssetVersion
  version: 10
`

func TestAuthoredMaterialKeepsPropertiesAndNativeReferences(t *testing.T) {
	session, _, _ := fixtureSession(t)
	session.dependencies = map[string]dependency{}
	path := "Assets/Material.mat"
	physical := filepath.Join(session.project, path)
	writeFixtureFile(t, physical, authoredMaterialFixture)
	writeFixtureFile(t, physical+".meta", "guid: 22222222222222222222222222222222\n")
	asset := map[string]any{"path": path, "kind": "authoredMaterial"}
	if err := session.refreshAsset(asset); err != nil {
		t.Fatal(err)
	}
	documents, err := ParseUnityYAML([]byte(authoredMaterialFixture))
	if err != nil {
		t.Fatal(err)
	}
	expected, _ := compactJSON(documents[0].Data)
	if asset["dataJson"] != expected || asset["fileId"] != "2100000" {
		t.Fatal("authored material data/identity was changed")
	}
	refs := array(asset["references"])
	if len(refs) != 3 {
		t.Fatalf("lost source shader/texture/parent references: %v", refs)
	}
	if bytes, _ := os.ReadFile(physical); string(bytes) != authoredMaterialFixture {
		t.Fatal("source material was overwritten")
	}
}

func TestRejectUnsupportedAuthoredMaterialVariantAndRuntimeSubasset(t *testing.T) {
	for _, content := range []string{
		strings.Replace(authoredMaterialFixture, "m_Parent: {fileID: 0}", "m_Parent: {fileID: 2100000, guid: 55555555555555555555555555555555, type: 2}", 1),
		strings.Replace(authoredMaterialFixture, "Unity.RenderPipelines.Universal.Editor::UnityEditor.Rendering.Universal.AssetVersion", "Unknown.RuntimeAsset", 1),
	} {
		session, _, _ := fixtureSession(t)
		session.dependencies = map[string]dependency{}
		path := "Assets/Material.mat"
		writeFixtureFile(t, filepath.Join(session.project, path), content)
		writeFixtureFile(t, filepath.Join(session.project, path)+".meta", "guid: 22222222222222222222222222222222\n")
		if err := session.refreshAsset(map[string]any{"path": path, "kind": "authoredMaterial"}); err == nil {
			t.Fatal("accepted unresolved material inheritance/runtime subasset")
		}
	}
}

const authoredMaterialVariantFixture = `%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!21 &2100000
Material:
  serializedVersion: 8
  m_Name: Variant
  m_Parent: {fileID: 2100000, guid: 55555555555555555555555555555555, type: 2}
  m_Shader: {fileID: 4800000, guid: 33333333333333333333333333333333, type: 3}
  m_ModifiedSerializedProperties: 0
  m_ValidKeywords: []
  m_LightmapFlags: 4
  m_EnableInstancingVariants: 0
  m_DoubleSidedGI: 0
  m_CustomRenderQueue: -1
  stringTagMap: {}
  disabledShaderPasses: []
  m_SavedProperties:
    m_TexEnvs: []
    m_Ints: []
    m_Floats: []
    m_Colors:
    - _BaseColor: {r: 0.35520002, g: 0.3096, b: 0.21000001, a: 1}
`

func materialVariantSession(t *testing.T, child, parent string) (*Session, map[string]any) {
	t.Helper()
	session, _, _ := fixtureSession(t)
	session.dependencies = map[string]dependency{}
	childPath, parentPath := "Assets/Child.mat", "Assets/Parent.mat"
	writeFixtureFile(t, filepath.Join(session.project, childPath), child)
	writeFixtureFile(t, filepath.Join(session.project, childPath)+".meta", "guid: 22222222222222222222222222222222\n")
	writeFixtureFile(t, filepath.Join(session.project, parentPath), parent)
	writeFixtureFile(t, filepath.Join(session.project, parentPath)+".meta", "guid: 55555555555555555555555555555555\n")
	session.byGUID["22222222222222222222222222222222"] = childPath
	session.byGUID["55555555555555555555555555555555"] = parentPath
	return session, map[string]any{"path": childPath, "kind": "authoredMaterial"}
}

func TestMaterialVariantKeepsSparseOverridesAndTracksParent(t *testing.T) {
	parent := strings.Replace(authoredMaterialFixture, "m_Name: Authored", "m_Name: Parent\n  m_EnableInstancingVariants: 1\n  m_CustomRenderQueue: 2450\n  stringTagMap: {RenderType: TransparentCutout}", 1)
	session, asset := materialVariantSession(t, authoredMaterialVariantFixture, parent)
	if err := session.refreshAsset(asset); err != nil {
		t.Fatal(err)
	}
	documents, err := ParseUnityYAML([]byte(authoredMaterialVariantFixture))
	if err != nil {
		t.Fatal(err)
	}
	expected, _ := compactJSON(documents[0].Data)
	if asset["dataJson"] != expected {
		t.Fatal("variant header defaults or sparse overrides were rewritten")
	}
	if _, exists := session.dependencies["Assets/Parent.mat"]; !exists {
		t.Fatal("parent changes will not invalidate variant preparation")
	}
	for path, source := range map[string]string{"Assets/Child.mat": authoredMaterialVariantFixture, "Assets/Parent.mat": parent} {
		actual, err := os.ReadFile(filepath.Join(session.project, path))
		if err != nil || string(actual) != source {
			t.Fatal("source material changed", path, err)
		}
	}
}

func TestRejectMaterialVariantUnknownFlagsDifferentShaderAndCycles(t *testing.T) {
	for _, test := range []struct{ name, child, parent string }{
		{"header-mask", strings.Replace(authoredMaterialVariantFixture, "m_ModifiedSerializedProperties: 0", "m_ModifiedSerializedProperties: 8", 1), authoredMaterialFixture},
		{"bad-mask", strings.Replace(authoredMaterialVariantFixture, "m_ModifiedSerializedProperties: 0", "m_ModifiedSerializedProperties: unknown", 1), authoredMaterialFixture},
		{"shader-mismatch", authoredMaterialVariantFixture, strings.Replace(authoredMaterialFixture, "33333333333333333333333333333333", "66666666666666666666666666666666", 1)},
		{"parent-cycle", authoredMaterialVariantFixture, strings.Replace(authoredMaterialVariantFixture, "55555555555555555555555555555555", "22222222222222222222222222222222", 1)},
		{"self-cycle", strings.Replace(authoredMaterialVariantFixture, "55555555555555555555555555555555", "22222222222222222222222222222222", 1), authoredMaterialFixture},
		{"missing-parent-local-id", strings.Replace(authoredMaterialVariantFixture, "m_Parent: {fileID: 2100000", "m_Parent: {fileID: 2100001", 1), authoredMaterialFixture},
	} {
		t.Run(test.name, func(t *testing.T) {
			session, asset := materialVariantSession(t, test.child, test.parent)
			if err := session.refreshAsset(asset); err == nil {
				t.Fatal("accepted invalid material variant", test.name)
			}
		})
	}
}

func TestMaterialVariantPreservesNativeParentReference(t *testing.T) {
	session, asset := materialVariantSession(t, authoredMaterialVariantFixture, authoredMaterialFixture)
	session.byGUID["55555555555555555555555555555555"] = "Assets/ImportedModel.fbx"
	if err := session.refreshAsset(asset); err != nil {
		t.Fatal(err)
	}
	parentFound := false
	for _, reference := range array(asset["references"]) {
		data := object(reference)
		if str(data["property"]) == "m_Parent" && str(data["guid"]) == "55555555555555555555555555555555" && str(data["fileId"]) == "2100000" {
			parentFound = true
		}
	}
	if !parentFound {
		t.Fatal("native parent material reference disappeared")
	}
}
