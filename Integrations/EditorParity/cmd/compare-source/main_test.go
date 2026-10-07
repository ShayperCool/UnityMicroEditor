package main

import (
	"encoding/json"
	"strings"
	"testing"

	goproject "unitymicroeditor/project"
)

func TestSourceReadbackDetectsNativeValueOwnershipAndHierarchyDifferences(t *testing.T) {
	source := hierarchy{Total: 1, Objects: []object{{FileID: "10", Name: "Box", ParentID: "0", Active: true, Components: []component{{FileID: "11", ClassID: 65, Type: "BoxCollider", Fields: map[string]any{"m_GameObject": map[string]any{"fileID": "10"}, "m_IsTrigger": 1, "m_Size": map[string]any{"x": 3, "y": 1, "z": 1}}}}}}}
	reference := editorReport{SchemaVersion: 1, UnityVersion: "6000.6.3f1", Objects: []editorObject{{Path: "Box", SourceFileID: "10", Active: true, Components: []editorComponent{{Type: "UnityEngine.BoxCollider", SourceFileID: "11", Properties: []property{{Path: "m_GameObject", Type: "ObjectReference", ReferenceFileID: "10"}, {Path: "m_IsTrigger", Type: "Boolean", Value: "1"}, {Path: "m_Size", Type: "Vector3", Value: "3|1|1"}}}}}}}
	if r := compare(source, reference); !r.Success || r.Properties != 3 {
		t.Fatalf("valid public readback differs: %+v", r)
	}
	reference.Objects[0].Components[0].Properties[2].Value = "4|1|1"
	if r := compare(source, reference); r.Success || !strings.Contains(strings.Join(r.Differences, " "), "m_Size") {
		t.Fatal("wrong native size passed comparison")
	}
	reference.Objects[0].Components[0].Properties[2].Value = "3|1|1"
	reference.Objects[0].Components[0].Properties[0].ReferenceFileID = "99"
	if r := compare(source, reference); r.Success {
		t.Fatal("wrong component owner passed comparison")
	}
	reference.Objects[0].Components[0].Properties[0].ReferenceFileID = "10"
	reference.Objects[0].Path = "Different/Box"
	if r := compare(source, reference); r.Success {
		t.Fatal("different parent path passed comparison")
	}
}

func TestSourceReadbackPreservesIntegerPrecision(t *testing.T) {
	h, err := readHierarchy([]byte(`{"objects":[{"fileId":"1","components":[{"fileId":"2","fields":{"counter":9007199254740993}}]}],"total":1}`))
	if err != nil {
		t.Fatal(err)
	}
	value := h.Objects[0].Components[0].Fields["counter"]
	if number, ok := value.(json.Number); !ok || number.String() != "9007199254740993" {
		t.Fatalf("integer rounded while reading MCP source: %v", value)
	}
	if !propertyEqual(value, property{Type: "Integer", Value: "9007199254740993"}) {
		t.Fatal("equal long integer differs")
	}
	if propertyEqual(value, property{Type: "Integer", Value: "9007199254740992"}) || propertyEqual(1000000000, property{Type: "Integer", Value: "1000000001"}) {
		t.Fatal("unequal integer passed approximate float comparison")
	}
}

func TestSerializedAssetReadbackChecksIdentityValuesAndMaterialPairs(t *testing.T) {
	content := "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!21 &2100000\nMaterial:\n  m_Name: Checker\n  m_SavedProperties:\n    m_Colors:\n    - _Color: {r: 0.2, g: 0.6, b: 0.9, a: 1}\n"
	source := []sourceAsset{{Path: "Assets/Checker.mat", GUID: "11111111111111111111111111111111", Content: content}}
	public := []editorAsset{{Path: source[0].Path, GUID: source[0].GUID, FileID: "2100000", Type: "UnityEngine.Material", Properties: []property{
		{Path: "m_Name", Type: "String", Value: "Checker"},
		{Path: "m_SavedProperties.m_Colors.Array.data[0].first", Type: "String", Value: "_Color"},
		{Path: "m_SavedProperties.m_Colors.Array.data[0].second", Type: "Color", Value: "0.2|0.6|0.9|1"},
	}}}
	r := result{Differences: []string{}}
	if err := compareAssets(&r, source, public); err != nil || !r.Success || r.Assets != 1 || r.Properties != 3 {
		t.Fatalf("valid serialized material differs %+v %v", r, err)
	}
	public[0].Properties[2].Value = "0.2|0.7|0.9|1"
	r = result{}
	if err := compareAssets(&r, source, public); err != nil || r.Success {
		t.Fatal("incorrect public material color accepted")
	}
	public[0].GUID = "22222222222222222222222222222222"
	r = result{}
	if err := compareAssets(&r, source, public); err != nil || r.Success {
		t.Fatal("incorrect public asset GUID accepted")
	}
	source[0].Truncated = true
	if err := compareAssets(&r, source, public); err == nil {
		t.Fatal("truncated asset snapshot accepted")
	}
}

func TestPrefabPublicSourceBindingsMustMatchResolvedProvenance(t *testing.T) {
	guid := "11111111111111111111111111111111"
	source := hierarchy{Total: 1, Objects: []object{{FileID: "500:100", Name: "Instance", ParentID: "0", Active: true, Components: []component{{FileID: "500:101", ClassID: 4, Type: "Transform", Fields: map[string]any{}}}}}, Provenance: map[string]provenance{
		"500:100": {Identities: []goproject.AuthoredIdentity{{GUID: guid, FileID: "100"}}},
		"500:101": {Identities: []goproject.AuthoredIdentity{{GUID: guid, FileID: "101"}}},
	}}
	public := editorReport{SchemaVersion: 1, UnityVersion: "6000.6.3f1", Objects: []editorObject{{Path: "Instance", Active: true, PrefabSourceGUID: guid, PrefabSourceFileID: "100", Components: []editorComponent{{Type: "UnityEngine.Transform", PrefabSourceGUID: guid, PrefabSourceFileID: "101"}}}}}
	if r := compare(source, public); !r.Success || r.PrefabBindings != 2 {
		t.Fatalf("public source bindings differ %+v", r)
	}
	public.Objects[0].Components[0].PrefabSourceFileID = "999"
	if r := compare(source, public); r.Success {
		t.Fatal("wrong public component prefab binding passed")
	}
	public.Objects[0].Components[0].PrefabSourceFileID = "101"
	public.Objects[0].PrefabSourceGUID = "22222222222222222222222222222222"
	if r := compare(source, public); r.Success {
		t.Fatal("wrong public GameObject prefab GUID passed")
	}
}

func TestSourceReadbackRejectsPagedOrFieldlessSnapshots(t *testing.T) {
	for _, data := range []string{`{"objects":[],"total":1,"nextOffset":1}`, `{"objects":[{"fileId":"1","components":[{"fileId":"2","fields":null}]}],"total":1}`, `{"result":{"structuredContent":{"success":false,"error":{"code":"failed"}}}}`} {
		if _, err := readHierarchy([]byte(data)); err == nil {
			t.Fatalf("incomplete/error snapshot accepted %s", data)
		}
	}
	value, ok := sourceProperty(map[string]any{"materials": []any{map[string]any{"fileID": "5"}}}, "materials.Array.data[0].fileID")
	if !ok || value != "5" {
		t.Fatal("public array path did not resolve")
	}
	if propertyEqual(map[string]any{"x": 1, "y": 2, "z": 3, "w": 4}, property{Type: "Vector4", Value: "1|2|3|4|5"}) {
		t.Fatal("malformed vector reference accepted")
	}
	if !propertyEqual(nil, property{Type: "String", Value: ""}) || propertyEqual(nil, property{Type: "String", Value: "nonempty"}) {
		t.Fatal("empty Unity serialized string normalization is incorrect")
	}
}
