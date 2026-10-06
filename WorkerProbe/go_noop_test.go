package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
)

func TestRuntimeEqualitySkipsRepeatedSourceWriteButKeepsAssetChanges(t *testing.T) {
	root := t.TempDir()
	write := func(name string, value map[string]any) string {
		t.Helper()
		path := filepath.Join(root, name)
		data, err := json.Marshal(value)
		if err != nil {
			t.Fatal(err)
		}
		if err = os.WriteFile(path, data, 0600); err != nil {
			t.Fatal(err)
		}
		return path
	}
	previous := write("old.json", map[string]any{"revision": "old", "manifest": "old.json", "contentHash": "a", "objects": []any{}, "sourceDependencies": []any{map[string]any{"path": "Assets/Texture.png", "sha256": "same"}}})
	same := write("same.json", map[string]any{"revision": "new", "manifest": "same.json", "contentHash": "b", "objects": []any{}, "sourceDependencies": []any{map[string]any{"path": "Assets/Texture.png", "sha256": "same"}}})
	changed := write("changed.json", map[string]any{"revision": "new", "objects": []any{}, "sourceDependencies": []any{map[string]any{"path": "Assets/Texture.png", "sha256": "different"}}})
	if equal, err := sceneRuntimeEqual(previous, same); err != nil || !equal {
		t.Fatalf("repeated write equal=%v: %v", equal, err)
	}
	if equal, err := sceneRuntimeEqual(previous, changed); err != nil || equal {
		t.Fatalf("asset byte change equal=%v: %v", equal, err)
	}
	settings := write("settings.json", map[string]any{"revision": "old", "manifest": "old.json", "contentHash": "a", "objects": []any{}, "sourceDependencies": []any{map[string]any{"path": "Assets/Texture.png", "sha256": "same"}}, "renderPipelineGuid": "changed"})
	if equal, err := sceneRuntimeEqual(previous, settings); err != nil || equal {
		t.Fatalf("pipeline change equal=%v: %v", equal, err)
	}
}

func TestRuntimeEqualityWhitespaceOnlySceneAndPrefabWritesAreNoOp(t *testing.T) {
	root := t.TempDir()
	makeManifest := func(sceneSource, prefabSource string) map[string]any {
		return map[string]any{
			"revision": "original",
			"objects":  []any{map[string]any{"fileId": "10", "classId": 1, "dataJson": `{"m_Name":"Scene object","m_IsActive":1}`}},
			"assets":   []any{map[string]any{"guid": "11111111111111111111111111111111", "kind": "prefab", "objects": []any{map[string]any{"fileId": "20", "dataJson": `{"m_Name":"Prefab object"}`}}}},
			"sourceDependencies": []any{
				map[string]any{"path": "Assets/Scenes/Sample.unity", "sha256": bytesSHA([]byte(sceneSource)), "bytes": len(sceneSource)},
				map[string]any{"path": "Assets/Prefab.prefab", "sha256": bytesSHA([]byte(prefabSource)), "bytes": len(prefabSource)},
				map[string]any{"path": "Assets/Texture.png", "sha256": "unchanged texture"},
			},
		}
	}
	write := func(name string, doc map[string]any) string {
		t.Helper()
		path := filepath.Join(root, name)
		if err := atomicJSON(path, doc); err != nil {
			t.Fatal(err)
		}
		return path
	}
	source := "--- !u!1 &10\nGameObject:\n  m_Name: Scene object\n"
	prefab := "--- !u!1 &20\nGameObject:\n  m_Name: Prefab object\n"
	previous := write("previous.json", makeManifest(source, prefab))
	for _, kind := range []string{"scene", "prefab", "both"} {
		t.Run(kind, func(t *testing.T) {
			sceneNext, prefabNext := source, prefab
			if kind != "prefab" {
				sceneNext += "\n\n"
			}
			if kind != "scene" {
				prefabNext += "\n\n"
			}
			next := makeManifest(sceneNext, prefabNext)
			next["revision"] = "whitespace changed"
			if equal, err := sceneRuntimeEqual(previous, write(kind+".json", next)); err != nil || !equal {
				t.Fatalf("whitespace-only %s write reset runtime graph: equal=%v error=%v", kind, equal, err)
			}
		})
	}
	// Ignoring scene provenance may not hide either changed scene fields or raw
	// runtime prefab fields. Both require a new runtime revision.
	changedScene := makeManifest(source+"\n", prefab)
	changedScene["objects"].([]any)[0].(map[string]any)["dataJson"] = `{"m_Name":"Edited scene object","m_IsActive":1}`
	changedPrefab := makeManifest(source, prefab+"\n")
	changedPrefab["assets"].([]any)[0].(map[string]any)["objects"].([]any)[0].(map[string]any)["dataJson"] = `{"m_Name":"Edited prefab object"}`
	for name, doc := range map[string]map[string]any{"scene runtime": changedScene, "prefab runtime": changedPrefab} {
		if equal, err := sceneRuntimeEqual(previous, write(name+".json", doc)); err != nil || equal {
			t.Fatalf("changed %s fields were treated as no-op: equal=%v error=%v", name, equal, err)
		}
	}
}
