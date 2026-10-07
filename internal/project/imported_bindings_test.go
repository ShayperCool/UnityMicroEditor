package goproject

import (
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
)

const bindingOwnerA = "22222222222222222222222222222222"
const bindingOwnerB = "33333333333333333333333333333333"
const bindingPrefabA = "44444444444444444444444444444444"
const bindingPrefabB = "55555555555555555555555555555555"
const bindingUnusedOwner = "66666666666666666666666666666666"
const bindingScript = "77777777777777777777777777777777"
const bindingComponentID = "8107781485188562762"
const bindingRootID = "8695755991542870949"

func bindingFixture(t *testing.T) (*Session, string) {
	t.Helper()
	s, _, output := fixtureSession(t)
	writeFixtureFile(t, filepath.Join(s.project, "Assets/Binding.cs"), "public class Binding {}")
	writeFixtureFile(t, filepath.Join(s.project, "Assets/Binding.cs.meta"), "guid: "+bindingScript+"\n")
	s.scriptTypes[bindingScript] = "Fixture.Binding"
	for name, guid := range map[string]string{"OwnerA.asset": bindingOwnerA, "OwnerB.asset": bindingOwnerB} {
		writeFixtureFile(t, filepath.Join(s.project, "Assets", name), "--- !u!114 &11400000\nMonoBehaviour:\n  m_Name: Owner\n")
		writeFixtureFile(t, filepath.Join(s.project, "Assets", name)+".meta", "guid: "+guid+"\n")
	}
	ownerA := map[string]any{"kind": "imported", "classId": 114, "typeName": "Fixture.ImportedOwner", "guid": bindingOwnerA, "fileId": "11400000", "path": "Assets/OwnerA.asset"}
	ownerB := map[string]any{"kind": "imported", "classId": 114, "typeName": "Fixture.ImportedOwner", "guid": bindingOwnerB, "fileId": "11400000", "path": "Assets/OwnerB.asset"}
	s.assets[assetKey(bindingOwnerA, "11400000")] = ownerA
	s.catalog.SchemaVersion = 1
	s.catalog.Assets = []map[string]any{ownerA, ownerB}
	recordImportedSourceProof(t, s, "Assets/OwnerA.asset", "Assets/OwnerB.asset")
	prefab := func(root, transform, component, owner string) string {
		return fmt.Sprintf(`--- !u!1 &%s
GameObject:
  m_Component:
  - component: {fileID: %s}
  - component: {fileID: %s}
  m_Name: Target
  m_IsActive: 1
--- !u!4 &%s
Transform:
  m_GameObject: {fileID: %s}
  m_LocalPosition: {x: 0, y: 0, z: 0}
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalScale: {x: 1, y: 1, z: 1}
  m_Father: {fileID: 0}
  m_Children: []
--- !u!114 &%s
MonoBehaviour:
  m_GameObject: {fileID: %s}
  m_Script: {fileID: 11500000, guid: %s, type: 3}
  linkedOwner: {fileID: 11400000, guid: %s, type: 2}
`, root, transform, component, transform, root, component, root, bindingScript, owner)
	}
	writeFixtureFile(t, filepath.Join(s.project, "Assets/TargetA.prefab"), prefab(bindingRootID, "-8679921383154817045", "901", bindingOwnerB))
	writeFixtureFile(t, filepath.Join(s.project, "Assets/TargetA.prefab.meta"), "guid: "+bindingPrefabA+"\n")
	writeFixtureFile(t, filepath.Join(s.project, "Assets/TargetB.prefab"), prefab("10", "11", bindingComponentID, bindingOwnerA))
	writeFixtureFile(t, filepath.Join(s.project, "Assets/TargetB.prefab.meta"), "guid: "+bindingPrefabB+"\n")
	s.catalog.DeferredBindings = []map[string]any{
		{"ownerGuid": bindingOwnerA, "ownerFileId": "11400000", "propertyPath": "characterPrefab", "targetGuid": bindingPrefabA, "targetFileId": bindingRootID, "targetPath": "Assets/TargetA.prefab"},
		{"ownerGuid": bindingOwnerB, "ownerFileId": "11400000", "propertyPath": "rootScope", "targetGuid": bindingPrefabB, "targetFileId": bindingComponentID, "targetPath": "Assets/TargetB.prefab"},
		{"ownerGuid": bindingUnusedOwner, "ownerFileId": "11400000", "propertyPath": "unused", "targetGuid": "99999999999999999999999999999999", "targetFileId": "123", "targetPath": "Assets/Absent.prefab"},
	}
	s.baseline["importedAssetBindings"] = []any{s.catalog.DeferredBindings[0], s.catalog.DeferredBindings[1], s.catalog.DeferredBindings[2]}
	s.indexDirty = true
	return s, output
}

func TestImportedBindingsPublishOnlyActiveOwnersAndRecursiveCyclicTargets(t *testing.T) {
	s, output := bindingFixture(t)
	before := map[string]string{}
	for _, path := range []string{"Assets/TargetA.prefab", "Assets/TargetA.prefab.meta", "Assets/TargetB.prefab", "Assets/TargetB.prefab.meta"} {
		b, _ := os.ReadFile(filepath.Join(s.project, path))
		before[path] = string(b)
	}
	for pass := 0; pass < 2; pass++ {
		prepared, err := s.Prepare(output)
		if err != nil {
			t.Fatal(err)
		}
		manifest, err := ManifestJSON(prepared.Manifest)
		if err != nil {
			t.Fatal(err)
		}
		bindings := array(manifest["importedAssetBindings"])
		if len(bindings) != 2 {
			t.Fatalf("inactive owner binding or recursive cycle was published incorrectly: %+v", bindings)
		}
		assets := map[string]map[string]any{}
		for _, value := range array(manifest["assets"]) {
			a := object(value)
			assets[assetKey(str(a["guid"]), str(a["fileId"]))] = a
		}
		for _, value := range bindings {
			b := object(value)
			owner := assets[assetKey(str(b["ownerGuid"]), str(b["ownerFileId"]))]
			if owner == nil || owner["kind"] != "imported" || !preparedAssetIdentity(assets, str(b["targetGuid"]), str(b["targetFileId"])) {
				t.Fatalf("binding owner/target lacks exact published identity: %+v", b)
			}
		}
		if !preparedAssetIdentity(assets, bindingPrefabA, bindingRootID) || !preparedAssetIdentity(assets, bindingPrefabB, bindingComponentID) || assets[assetKey(bindingOwnerB, "11400000")] == nil {
			t.Fatal("recursive imported-owner/component target closure was lost")
		}
	}
	for path, expected := range before {
		b, _ := os.ReadFile(filepath.Join(s.project, path))
		if string(b) != expected {
			t.Fatalf("source changed %s", path)
		}
	}
}

func TestImportedBindingRawOwnerUsesCurrentAuthoredReferences(t *testing.T) {
	s, output := bindingFixture(t)
	path := "Assets/RawOwner.asset"
	writeFixtureFile(t, filepath.Join(s.project, path), "--- !u!114 &11400000\nMonoBehaviour:\n  m_Script: {fileID: 11500000, guid: "+bindingScript+", type: 3}\n  ownTarget: {fileID: "+bindingRootID+", guid: "+bindingPrefabA+", type: 3}\n")
	writeFixtureFile(t, filepath.Join(s.project, path)+".meta", "guid: "+bindingUnusedOwner+"\n")
	s.assets[assetKey(bindingUnusedOwner, "11400000")] = map[string]any{"kind": "scriptableObject", "path": path, "guid": bindingUnusedOwner, "fileId": "11400000"}
	s.indexDirty = true
	prepared, err := s.Prepare(output)
	if err != nil {
		t.Fatal(err)
	}
	manifest, _ := ManifestJSON(prepared.Manifest)
	for _, value := range array(manifest["importedAssetBindings"]) {
		if object(value)["ownerGuid"] == bindingUnusedOwner {
			t.Fatal("raw owner received stale native deferred binding instead of its source reference")
		}
	}
}

func TestImportedBindingProvesInheritedComponentAlias(t *testing.T) {
	s, output := bindingFixture(t)
	variantGUID := "88888888888888888888888888888888"
	writeFixtureFile(t, filepath.Join(s.project, "Assets/Variant.prefab"), "--- !u!1001 &900\nPrefabInstance:\n  m_SourcePrefab: {fileID: 100100000, guid: "+bindingPrefabB+", type: 3}\n  m_Modification:\n    m_TransformParent: {fileID: 0}\n    m_Modifications: []\n")
	writeFixtureFile(t, filepath.Join(s.project, "Assets/Variant.prefab.meta"), "guid: "+variantGUID+"\n")
	component, _ := strconv.ParseInt(bindingComponentID, 10, 64)
	alias := strconv.FormatInt((component^900)&0x7fffffffffffffff, 10)
	s.catalog.DeferredBindings[1]["targetGuid"], s.catalog.DeferredBindings[1]["targetFileId"], s.catalog.DeferredBindings[1]["targetPath"] = variantGUID, alias, "Assets/Variant.prefab"
	s.indexDirty = true
	prepared, err := s.Prepare(output)
	if err != nil {
		t.Fatal(err)
	}
	manifest, _ := ManifestJSON(prepared.Manifest)
	verified := false
	for _, value := range array(manifest["assets"]) {
		a := object(value)
		if a["guid"] != variantGUID {
			continue
		}
		for _, entry := range array(a["objectAliases"]) {
			b := object(entry)
			if b["fileId"] == alias && b["objectId"] == "900:"+bindingComponentID {
				verified = true
			}
		}
	}
	if !verified {
		t.Fatal("inherited component target was not proven by the actual expanded prefab source alias")
	}
}

func TestImportedBindingRejectsForgedTargetsAndConflictingProperties(t *testing.T) {
	cases := map[string]func(*Session){
		"unknown source object": func(s *Session) { s.catalog.DeferredBindings[0]["targetFileId"] = "123456" },
		"foreign target path":   func(s *Session) { s.catalog.DeferredBindings[0]["targetPath"] = "Assets/Other.prefab" },
		"rounded target ID":     func(s *Session) { s.catalog.DeferredBindings[0]["targetFileId"] = "9223372036854775808" },
		"invalid property":      func(s *Session) { s.catalog.DeferredBindings[0]["propertyPath"] = "eval()" },
		"conflicting property": func(s *Session) {
			b := clone(s.catalog.DeferredBindings[0]).(map[string]any)
			b["targetGuid"], b["targetFileId"], b["targetPath"] = bindingPrefabB, bindingComponentID, "Assets/TargetB.prefab"
			s.catalog.DeferredBindings = append(s.catalog.DeferredBindings, b)
		},
	}
	for name, change := range cases {
		t.Run(name, func(t *testing.T) {
			s, output := bindingFixture(t)
			good, err := s.Prepare(output)
			if err != nil {
				t.Fatal(err)
			}
			before, _ := os.ReadFile(good.Manifest)
			change(s)
			if _, err := s.Prepare(output); err == nil {
				t.Fatal("invalid imported binding target was silently dropped or published")
			}
			after, _ := os.ReadFile(good.Manifest)
			if string(before) != string(after) {
				t.Fatal("failed closure changed prior valid manifest")
			}
		})
	}
}

func TestImportedBindingsClearStaleBaselineListWithEmptyCurrentCatalog(t *testing.T) {
	s, output := bindingFixture(t)
	s.catalog.DeferredBindings = nil
	prepared, err := s.Prepare(output)
	if err != nil {
		t.Fatal(err)
	}
	manifest, _ := ManifestJSON(prepared.Manifest)
	if len(array(manifest["importedAssetBindings"])) != 0 {
		t.Fatal("cached baseline resurrected stale removed native bindings")
	}
	if _, ok := manifest["importedAssetBindings"]; !ok {
		t.Fatal("current binding list was left implicit")
	}
	if strings.Contains(str(manifest["importedAssetBindings"]), bindingUnusedOwner) {
		t.Fatal("inactive owner persisted")
	}
}

func TestImportedBindingRejectsReplacedRootInCachedRawPrefab(t *testing.T) {
	s, output := bindingFixture(t)
	good, err := s.Prepare(output)
	if err != nil {
		t.Fatal(err)
	}
	manifest, err := ManifestJSON(good.Manifest)
	if err != nil {
		t.Fatal(err)
	}
	for _, value := range array(manifest["assets"]) {
		a := object(value)
		s.assets[assetKey(str(a["guid"]), str(a["fileId"]))] = a
	}
	path := filepath.Join(s.project, "Assets/TargetA.prefab")
	bytes, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	writeFixtureFile(t, path, strings.ReplaceAll(string(bytes), bindingRootID, "8695755991542870950"))
	if _, err := s.Prepare(output); err == nil || !strings.Contains(err.Error(), "has no object") {
		t.Fatalf("cached old root ID was accepted after actual prefab root replacement: %v", err)
	}
	if current, err := ManifestJSON(good.Manifest); err != nil || len(array(current["importedAssetBindings"])) != 2 {
		t.Fatal("failed root replacement changed prior valid binding manifest")
	}
}
