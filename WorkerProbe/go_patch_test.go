package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
)

func patchTestRecord(id string, class int, owner string, data map[string]any) map[string]any {
	b, _ := json.Marshal(data)
	name := "GameObject"
	if class == 4 {
		name = "Transform"
	} else if class == sceneRootsClass {
		name = "SceneRoots"
	}
	return map[string]any{"fileId": id, "classId": class, "gameObjectId": owner, "sourcePath": "Assets/Scene.unity", "guid": "11111111111111111111111111111111", "typeName": name, "skipMissingScript": false, "dataJson": string(b), "scalarDataJson": "{}", "references": []any{}}
}
func patchTestReference(id string) any { return map[string]any{"fileID": id} }
func patchTestObject(id, transform string) map[string]any {
	return patchTestRecord(id, 1, "0", map[string]any{"m_Name": "Original", "m_Layer": 0, "m_IsActive": 1, "m_TagString": "Untagged", "m_Component": []any{map[string]any{"component": patchTestReference(transform)}}})
}
func patchTestTransform(id, owner, parent string) map[string]any {
	return patchTestRecord(id, 4, owner, map[string]any{"m_GameObject": patchTestReference(owner), "m_Father": patchTestReference(parent), "m_Children": []any{}, "m_LocalPosition": map[string]any{"x": 0, "y": 0, "z": 0}, "m_LocalRotation": map[string]any{"x": 0, "y": 0, "z": 0, "w": 1}, "m_LocalScale": map[string]any{"x": 1, "y": 1, "z": 1}})
}
func patchTestScene() map[string]any {
	return map[string]any{"schemaVersion": 1, "supported": true, "revision": "old", "assemblies": []any{"Game.dll"}, "assets": []any{}, "renderPipelineGuid": "pipeline", "objects": []any{patchTestObject("10", "11"), patchTestTransform("11", "10", "0"), patchTestRecord("roots", sceneRootsClass, "0", map[string]any{"m_Roots": []any{patchTestReference("11")}})}}
}
func patchTestClone(m map[string]any) map[string]any {
	b, _ := json.Marshal(m)
	var out map[string]any
	_ = json.Unmarshal(b, &out)
	return out
}
func patchTestEdit(scene map[string]any, id string, edit func(map[string]any)) {
	for _, v := range scene["objects"].([]any) {
		r := v.(map[string]any)
		if r["fileId"] == id {
			var data map[string]any
			_ = decodePatchJSON([]byte(r["dataJson"].(string)), &data)
			edit(data)
			b, _ := json.Marshal(data)
			r["dataJson"] = string(b)
		}
	}
}
func patchTestMake(t *testing.T, old, next map[string]any) (string, string, error) {
	t.Helper()
	dir := t.TempDir()
	a, b := filepath.Join(dir, "old.json"), filepath.Join(dir, "new.json")
	if err := atomicJSON(a, old); err != nil {
		t.Fatal(err)
	}
	if err := atomicJSON(b, next); err != nil {
		t.Fatal(err)
	}
	return makeScenePatch(a, b)
}

func TestScenePatchNameAndPoseContainOnlyChangedAuthoredRecords(t *testing.T) {
	old := patchTestScene()
	next := patchTestClone(old)
	next["revision"] = "next"
	patchTestEdit(next, "10", func(m map[string]any) { m["m_Name"] = "Renamed" })
	path, hash, err := patchTestMake(t, old, next)
	if err != nil || path == "" {
		t.Fatalf("rename rejected %s %v", path, err)
	}
	data, err := os.ReadFile(path)
	if err != nil || hash != bytesSHA(data) {
		t.Fatal("patch hash does not identify emitted bytes")
	}
	var patch map[string]any
	_ = json.Unmarshal(data, &patch)
	objects := patch["objects"].([]any)
	if len(objects) != 1 || objects[0].(map[string]any)["fileId"] != "10" || patch["baseRevision"] != "old" || patch["revision"] != "next" {
		t.Fatal("unrelated objects or wrong revision in patch")
	}
	patchTestEdit(next, "11", func(m map[string]any) { m["m_LocalPosition"] = map[string]any{"x": 3, "y": 2, "z": 1} })
	if path, _, err := patchTestMake(t, old, next); err != nil || path == "" {
		t.Fatalf("pose patch rejected: %v", err)
	}
}

func TestScenePatchPlainRootAndChildAdditions(t *testing.T) {
	for _, parent := range []string{"0", "11"} {
		t.Run(parent, func(t *testing.T) {
			old := patchTestScene()
			next := patchTestClone(old)
			next["revision"] = "next"
			next["objects"] = append(next["objects"].([]any), patchTestObject("20", "21"), patchTestTransform("21", "20", parent))
			if parent == "0" {
				patchTestEdit(next, "roots", func(m map[string]any) { m["m_Roots"] = append(m["m_Roots"].([]any), patchTestReference("21")) })
			} else {
				patchTestEdit(next, "11", func(m map[string]any) { m["m_Children"] = []any{patchTestReference("21")} })
			}
			path, _, err := patchTestMake(t, old, next)
			if err != nil || path == "" {
				t.Fatalf("plain GameObject addition fell back: %v", err)
			}
			var patch map[string]any
			_ = readJSONFile(path, &patch)
			if len(patch["objects"].([]any)) != 3 {
				t.Fatalf("expected GO/Transform/hierarchy records, got %+v", patch)
			}
		})
	}
}

func TestScenePatchSemanticJSONFormattingIsNoOp(t *testing.T) {
	old := patchTestScene()
	next := patchTestClone(old)
	next["revision"] = "next"
	records := next["objects"].([]any)
	records[0].(map[string]any)["dataJson"] = `{"m_TagString":"Untagged","m_Name":"Original","m_Layer":0.0,"m_IsActive":1.0,"m_Component":[{"component":{"fileID":"11"}}]}`
	if path, _, err := patchTestMake(t, old, next); err != nil || path != "" {
		t.Fatalf("JSON key/number formatting created a false patch: %s %v", path, err)
	}
}

func TestScenePatchRequiresCompleteLoaderForSettingsNativeAndDeletion(t *testing.T) {
	for _, change := range []func(map[string]any){func(m map[string]any) { m["renderPipelineGuid"] = "changed" }, func(m map[string]any) {
		m["nativeContentFiles"] = []any{map[string]any{"guid": "native", "sha256": "different"}}
	}, func(m map[string]any) { m["assemblies"] = []any{"New.dll"} }, func(m map[string]any) { m["objects"] = m["objects"].([]any)[1:] }} {
		old := patchTestScene()
		next := patchTestClone(old)
		next["revision"] = "next"
		patchTestEdit(next, "10", func(m map[string]any) { m["m_Name"] = "Renamed" })
		change(next)
		if path, _, err := patchTestMake(t, old, next); err != nil || path != "" {
			t.Fatalf("unsupported manifest change accepted: %s %v", path, err)
		}
	}
}

func TestScenePatchRejectsMalformedIdentityAndHierarchyWithoutPanic(t *testing.T) {
	for _, change := range []func(map[string]any){func(m map[string]any) { m["objects"].([]any)[0].(map[string]any)["fileId"] = nil }, func(m map[string]any) { m["objects"] = append(m["objects"].([]any), m["objects"].([]any)[0]) }, func(m map[string]any) {
		patchTestEdit(m, "11", func(d map[string]any) { d["m_Children"] = []any{patchTestReference("999")} })
	}, func(m map[string]any) {
		patchTestEdit(m, "11", func(d map[string]any) {
			d["m_Father"] = patchTestReference("11")
			d["m_Children"] = []any{patchTestReference("11")}
		})
	}, func(m map[string]any) { patchTestEdit(m, "10", func(d map[string]any) { d["m_Layer"] = 32 }) }} {
		old := patchTestScene()
		next := patchTestClone(old)
		next["revision"] = "next"
		change(next)
		if path, _, err := patchTestMake(t, old, next); err == nil || path != "" {
			t.Fatalf("malformed graph was accepted: %s %v", path, err)
		}
	}
}

func TestScenePatchImmutableUint64ValuesDoNotRoundToFloat(t *testing.T) {
	old := patchTestScene()
	patchTestEdit(old, "10", func(m map[string]any) { m["m_StaticEditorFlags"] = json.Number("12934340311651418136") })
	next := patchTestClone(old)
	next["revision"] = "next"
	patchTestEdit(next, "10", func(m map[string]any) {
		m["m_StaticEditorFlags"] = json.Number("12934340311651418137")
		m["m_Name"] = "Renamed"
	})
	if path, _, err := patchTestMake(t, old, next); err != nil || path != "" {
		t.Fatalf("different immutable uint64 flags were rounded into a safe patch: %s %v", path, err)
	}
}

func TestScenePatchHeavySourceChangeAndRenameRequiresFullReload(t *testing.T) {
	for _, asset := range []string{"Assets/Texture.png", "Assets/Material.mat", "Assets/Audio.wav", "Assets/Model.fbx"} {
		t.Run(asset, func(t *testing.T) {
			previous := patchTestScene()
			previous["sourceDependencies"] = []any{map[string]any{"path": asset, "sha256": "previous source", "bytes": 100}}
			next := patchTestClone(previous)
			next["revision"] = "next"
			next["sourceDependencies"].([]any)[0].(map[string]any)["sha256"] = "new source"
			patchTestEdit(next, "10", func(data map[string]any) { data["m_Name"] = "Authored rename" })
			path, hash, err := patchTestMake(t, previous, next)
			if err != nil || path != "" || hash != "" {
				t.Fatalf("combined rename/native source change bypassed full loader: path=%s hash=%s err=%v", path, hash, err)
			}
		})
	}
}

func TestScenePatchSceneAndPrefabProvenanceChangeKeepsSupportedRename(t *testing.T) {
	for _, asset := range []string{"Assets/Scene.unity", "Assets/Prefab.prefab"} {
		t.Run(asset, func(t *testing.T) {
			previous := patchTestScene()
			previous["sourceDependencies"] = []any{map[string]any{"path": asset, "sha256": "previous source", "bytes": 100}}
			next := patchTestClone(previous)
			next["revision"] = "next"
			dependency := next["sourceDependencies"].([]any)[0].(map[string]any)
			dependency["sha256"], dependency["bytes"] = "new source", 200
			patchTestEdit(next, "10", func(data map[string]any) { data["m_Name"] = "Authored rename" })
			path, hash, err := patchTestMake(t, previous, next)
			if err != nil || path == "" || hash == "" {
				t.Fatalf("authored scene/prefab provenance disabled the supported rename patch: path=%s err=%v", path, err)
			}
			var patch struct {
				Objects []map[string]any `json:"objects"`
			}
			if err := readJSONFile(path, &patch); err != nil {
				t.Fatal(err)
			}
			if len(patch.Objects) != 1 || patch.Objects[0]["fileId"] != "10" {
				t.Fatalf("provenance changed unrelated runtime records: %+v", patch.Objects)
			}
		})
	}
}
