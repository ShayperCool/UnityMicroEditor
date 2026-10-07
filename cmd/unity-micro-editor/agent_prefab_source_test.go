package main

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"unitymicroeditor/internal/project/authoring"
)

func TestAgentPrefabSourceNeverStartsEditorAndReplaysExactNumbers(t *testing.T) {
	project := agentReviewProject(t)
	path := "Assets/Main.unity"
	content := `%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &100
GameObject:
  m_CorrespondingSourceObject: {fileID: 0}
  m_PrefabInstance: {fileID: 0}
  m_PrefabAsset: {fileID: 0}
  m_Component:
  - component: {fileID: 101}
  m_Name: Root
  m_IsActive: 1
  exactCounter: 0 # preserve
--- !u!4 &101
Transform:
  m_CorrespondingSourceObject: {fileID: 0}
  m_PrefabInstance: {fileID: 0}
  m_PrefabAsset: {fileID: 0}
  m_GameObject: {fileID: 100}
  m_LocalPosition: {x: 0, y: 0, z: 0}
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalScale: {x: 1, y: 1, z: 1}
  m_Children: []
  m_Father: {fileID: 0}
--- !u!1660057539 &9223372036854775807
SceneRoots:
  m_Roots:
  - {fileID: 101}
`
	if err := os.WriteFile(filepath.Join(project, path), []byte(content), 0600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(project, path)+".meta", []byte("guid: 33333333333333333333333333333333\n"), 0600); err != nil {
		t.Fatal(err)
	}
	store, err := authoring.NewStore(project)
	if err != nil {
		t.Fatal(err)
	}
	call := func(service *agentProjectService, args map[string]any) map[string]any {
		t.Helper()
		source, err := store.Read(context.Background(), path)
		if err != nil {
			t.Fatal(err)
		}
		args["path"], args["expectedRevision"] = path, source.Revision
		raw, _ := json.Marshal(args)
		result, err := service.HandleSync(context.Background(), "prefab_edit", raw)
		if err != nil {
			t.Fatal(err)
		}
		value := result.(map[string]any)
		if value["backend"] != "raw_source" || value["editorStarted"] != false {
			t.Fatalf("wrong execution backend %+v", value)
		}
		return value
	}
	service := &agentProjectService{project: project, editor: "/definitely-no-editor", lifetime: context.Background()}
	call(service, map[string]any{"action": "create_prefab", "fileId": "100", "outputPath": "Assets/Base.prefab", "operationId": "own-create"})
	call(service, map[string]any{"action": "instantiate", "prefabPath": "Assets/Base.prefab", "name": "Linked", "operationId": "own-instantiate"})
	source, _ := store.Read(context.Background(), path)
	args := map[string]any{"action": "set_override", "path": path, "expectedRevision": source.Revision, "targetPath": "Linked", "property": "exactCounter", "value": json.Number("9223372036854775807"), "operationId": "own-exact"}
	raw, _ := json.Marshal(args)
	for iteration := 0; iteration < 2; iteration++ {
		restarted := &agentProjectService{project: project, editor: "/definitely-no-editor", lifetime: context.Background()}
		result, err := restarted.HandleSync(context.Background(), "prefab_edit", raw)
		restarted.Close()
		if err != nil {
			t.Fatal(err)
		}
		publication := result.(map[string]any)["publication"].(authoring.PublicationResult)
		if publication.Replayed != (iteration == 1) {
			t.Fatal("wrong replay", publication)
		}
	}
	updated, _ := store.Read(context.Background(), path)
	if !strings.Contains(updated.Content, "9223372036854775807") || !strings.Contains(updated.Content, "exactCounter: 0 # preserve") || updated.Metadata != source.Metadata {
		t.Fatal("exact override or unrelated bytes were changed")
	}
	args["value"] = json.Number("9223372036854775806")
	raw, _ = json.Marshal(args)
	var conflict *authoring.Error
	if _, err := service.HandleSync(context.Background(), "prefab_edit", raw); !errors.As(err, &conflict) || conflict.Code != "idempotency_conflict" {
		t.Fatalf("adjacent int64 reused operation: %v", err)
	}
	if _, err := os.Stat(filepath.Join(project, "Library/UnityMicroEditor/authoring-stages")); !os.IsNotExist(err) {
		t.Fatal("source authoring created an Editor stage")
	}
}
