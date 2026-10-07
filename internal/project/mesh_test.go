package goproject

import (
	"encoding/json"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

const simpleAuthoredMesh = `%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!43 &4300000
Mesh:
  m_Name: AuthoredMesh
  serializedVersion: 12
  m_MeshCompression: 0
  m_IsReadable: 1
  m_IndexFormat: 0
  m_IndexBuffer: 000000000000y
  m_VertexData:
    serializedVersion: 3
    m_VertexCount: 1
    m_DataSize: 12
    _typelessdata: 000000000000000000000000
    m_Channels:
    - {stream: 0, offset: 0, format: 0, dimension: 3}
    - {stream: 0, offset: 0, format: 0, dimension: 0}
    - {stream: 0, offset: 0, format: 0, dimension: 0}
    - {stream: 0, offset: 0, format: 0, dimension: 0}
    - {stream: 0, offset: 0, format: 0, dimension: 0}
    - {stream: 0, offset: 0, format: 0, dimension: 0}
    - {stream: 0, offset: 0, format: 0, dimension: 0}
    - {stream: 0, offset: 0, format: 0, dimension: 0}
    - {stream: 0, offset: 0, format: 0, dimension: 0}
    - {stream: 0, offset: 0, format: 0, dimension: 0}
    - {stream: 0, offset: 0, format: 0, dimension: 0}
    - {stream: 0, offset: 0, format: 0, dimension: 0}
    - {stream: 0, offset: 0, format: 0, dimension: 0}
    - {stream: 0, offset: 0, format: 0, dimension: 0}
  m_SubMeshes:
  - {serializedVersion: 2, firstByte: 0, indexCount: 3, topology: 0, baseVertex: 0, firstVertex: 0, vertexCount: 1}
`

func TestAuthoredMeshKeepsUnityIdentityAndBytesInManifest(t *testing.T) {
	s, _, _ := fixtureSession(t)
	s.dependencies = map[string]dependency{}
	path := "Assets/Meshes/Shape.asset"
	writeFixtureFile(t, filepath.Join(s.project, path), simpleAuthoredMesh)
	asset := map[string]any{"kind": "scriptableObject", "guid": "22222222222222222222222222222222", "path": path}
	if err := s.refreshAsset(asset); err != nil {
		t.Fatal(err)
	}
	if str(asset["kind"]) != "mesh" || number(asset["classId"]) != 43 || str(asset["typeName"]) != "UnityEngine.Mesh" || str(asset["fileId"]) != "4300000" {
		t.Fatalf("Authored Mesh classified incorrectly: %#v", asset)
	}
	var data map[string]any
	if err := json.Unmarshal([]byte(str(asset["dataJson"])), &data); err != nil {
		t.Fatal(err)
	}
	if str(data["m_IndexBuffer"]) != "000000000000y" || str(object(data["m_VertexData"])["_typelessdata"]) != "000000000000000000000000" {
		t.Fatal("Authored Mesh byte scalar changed")
	}
}

func TestAuthoredMeshRejectsUnsupportedContent(t *testing.T) {
	documents, err := ParseUnityYAML([]byte(simpleAuthoredMesh))
	if err != nil {
		t.Fatal(err)
	}
	for name, edit := range map[string]func(map[string]any){
		"compressed": func(data map[string]any) { data["m_MeshCompression"] = 1 },
		"packed": func(data map[string]any) {
			data["m_CompressedMesh"] = map[string]any{"m_Vertices": map[string]any{"m_NumItems": 3}}
		},
		"skinned":         func(data map[string]any) { data["m_BindPose"] = []any{map[string]any{}} },
		"blend shapes":    func(data map[string]any) { data["m_Shapes"] = map[string]any{"vertices": []any{map[string]any{}}} },
		"streamed":        func(data map[string]any) { data["m_StreamData"] = map[string]any{"path": "external.bin", "size": 12} },
		"wrong data size": func(data map[string]any) { object(data["m_VertexData"])["m_DataSize"] = 11 },
		"invalid hex":     func(data map[string]any) { data["m_IndexBuffer"] = "zzzz" },
		"external source": func(data map[string]any) {
			data["m_PrefabAsset"] = map[string]any{"fileID": "1", "guid": "11111111111111111111111111111111"}
		},
	} {
		t.Run(name, func(t *testing.T) {
			data := clone(documents[0].Data).(map[string]any)
			edit(data)
			if err := validateAuthoredMesh(data); err == nil {
				t.Fatal("Unsupported source Mesh was accepted")
			}
		})
	}
}

func TestAllRealProjectAuthoredMeshes(t *testing.T) {
	root := os.Getenv("UNITY_GO_MESH_SOURCE_PROJECT")
	if root == "" {
		t.Skip("Set UNITY_GO_MESH_SOURCE_PROJECT to source project")
	}
	count := 0
	err := filepath.WalkDir(filepath.Join(root, "Assets"), func(path string, entry fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if entry.IsDir() || !strings.HasSuffix(path, ".asset") {
			return nil
		}
		data, err := os.ReadFile(path)
		if err != nil {
			return err
		}
		documents, err := ParseUnityYAML(data)
		if err != nil {
			return err
		}
		if len(documents) != 1 || documents[0].ClassID != 43 {
			return nil
		}
		count++
		if err := validateAuthoredMesh(documents[0].Data); err != nil {
			t.Errorf("%s: %v", path, err)
		}
		return nil
	})
	if err != nil {
		t.Fatal(err)
	}
	if count < 100 {
		t.Fatalf("Expected actual project mesh inventory, observed %d", count)
	}
	t.Logf("Validated %d source-authored static/fixed-four skinned Mesh assets", count)
}
