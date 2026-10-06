package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func overlayFixture() map[string]any {
	return map[string]any{
		"scenePath": "Assets/Net/Boot.unity",
		"objects": []any{
			map[string]any{"fileId": "8172564688416556036", "classId": json.Number("1"), "dataJson": `{"m_Name":"Session root","m_Component":[{"component":{"fileID":3135080828069794750}},{"component":{"fileID":1088385461078338481}}]}`, "references": []any{}},
			map[string]any{"fileId": "1088385461078338481", "classId": json.Number("114"), "gameObjectId": "8172564688416556036", "typeName": "Dungeon.Net.NetLifetimeScope", "dataJson": `{"m_GameObject":{"fileID":8172564688416556036},"settings":{"fileID":11400000,"guid":"11111111111111111111111111111111"},"contentProvider":{"fileID":0}}`, "scalarDataJson": `{"autoRun":true}`, "references": []any{map[string]any{"property": "settings", "fileId": "11400000", "guid": "11111111111111111111111111111111"}, map[string]any{"property": "contentProvider", "fileId": "0"}}},
		},
	}
}

func TestProviderOverlayPreservesLargeIDsAndExistingSourceReferences(t *testing.T) {
	manifest := overlayFixture()
	if err := wireProvider(manifest); err != nil {
		t.Fatal(err)
	}
	objects := asArray(manifest["objects"])
	if len(objects) != 3 {
		t.Fatal("overlay did not add exactly one component")
	}
	owner, scope, provider := asMap(objects[0]), asMap(objects[1]), asMap(objects[2])
	if provider["gameObjectId"] != "8172564688416556036" || provider["typeName"] != providerType {
		t.Fatal("provider owner/type changed")
	}
	if scope["scalarDataJson"] != `{"autoRun":true}` {
		t.Fatal("unrelated scope settings changed")
	}
	scopeData, err := parseData(scope["dataJson"])
	if err != nil {
		t.Fatal(err)
	}
	if text(asMap(scopeData["m_GameObject"])["fileID"]) != "8172564688416556036" {
		t.Fatal("large GameObject ID lost precision")
	}
	if text(asMap(scopeData["settings"])["guid"]) != "11111111111111111111111111111111" {
		t.Fatal("existing serialized source asset lost")
	}
	providerID := text(provider["fileId"])
	if text(asMap(scopeData["contentProvider"])["fileID"]) != providerID {
		t.Fatal("scope does not reference its provider")
	}
	ownerData, _ := parseData(owner["dataJson"])
	components := asArray(ownerData["m_Component"])
	if len(components) != 3 || text(asMap(asMap(components[2])["component"])["fileID"]) != providerID {
		t.Fatal("provider missing from owner component graph")
	}
	if len(asArray(scope["references"])) != 2 {
		t.Fatal("duplicate provider reference retained")
	}
	if err := wireProvider(manifest); err == nil {
		t.Fatal("double overlay silently produced duplicate providers")
	}
}

func TestAssemblyRegistrationRetainsEngineAndGameFlags(t *testing.T) {
	path := filepath.Join(t.TempDir(), "ScriptingAssemblies.json")
	if err := os.WriteFile(path, []byte(`{"names":["UnityEngine.CoreModule.dll","Dungeon.CLIPlay.dll"],"types":[2,16]}`), 0600); err != nil {
		t.Fatal(err)
	}
	if err := registerHelper(path); err != nil {
		t.Fatal(err)
	}
	var result struct {
		Names []string
		Types []int
	}
	if err := readJSON(path, &result); err != nil {
		t.Fatal(err)
	}
	if len(result.Names) != 3 || result.Names[2] != helperAssembly+".dll" || result.Types[0] != 2 || result.Types[1] != 16 || result.Types[2] != 16 {
		t.Fatalf("registry flags changed: %+v", result)
	}
	if err := registerHelper(path); err == nil {
		t.Fatal("double registration accepted")
	}
}

func TestPublishedRoutesKeepRuntimeJSONFieldNames(t *testing.T) {
	var routes routesFile
	if err := json.Unmarshal([]byte(`{"schemaVersion":1,"project":"/project","scenes":[{"guid":"11111111111111111111111111111111","manifest":"/manifest","sha256":"a"}]}`), &routes); err != nil {
		t.Fatal(err)
	}
	encoded, err := json.Marshal(routes)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(encoded), `"GUID"`) || !strings.Contains(string(encoded), `"guid":"11111111111111111111111111111111"`) {
		t.Fatal("Unity JsonUtility cannot read published route fields")
	}
}
