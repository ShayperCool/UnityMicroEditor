package goproject

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

const multiScriptableGUID = "22222222222222222222222222222222"
const multiScriptableScriptGUID = "33333333333333333333333333333333"

const multiScriptableFixture = `%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!114 &11400000
MonoBehaviour:
  m_Name: Profile
  m_Script: {fileID: 11500000, guid: 33333333333333333333333333333333, type: 3}
  components:
  - {fileID: -8901537253420550725}
  - {fileID: 9223372036854775807}
  external: {fileID: 0}
--- !u!114 &-8901537253420550725
MonoBehaviour:
  m_Name: First
  m_Script: {fileID: 11500000, guid: 33333333333333333333333333333333, type: 3}
  sibling: {fileID: 9223372036854775807}
  profile: {fileID: 11400000}
--- !u!114 &9223372036854775807
MonoBehaviour:
  m_Name: Second
  m_Script: {fileID: 11500000, guid: 33333333333333333333333333333333, type: 3}
  sibling: {fileID: -8901537253420550725}
  profile: {fileID: 11400000}
`

func TestMultiScriptableObjectPublishesCompleteCyclicSameFileClosure(t *testing.T) {
	s, _, output := fixtureSession(t)
	path := "Assets/Data/Profile.asset"
	physical := filepath.Join(s.project, path)
	writeFixtureFile(t, physical, multiScriptableFixture)
	writeFixtureFile(t, physical+".meta", "guid: "+multiScriptableGUID+"\n")
	writeFixtureFile(t, filepath.Join(s.project, "Assets/Profile.cs"), "public class Profile {}")
	writeFixtureFile(t, filepath.Join(s.project, "Assets/Profile.cs.meta"), "guid: "+multiScriptableScriptGUID+"\n")
	s.scriptTypes[multiScriptableScriptGUID] = "Fixture.Profile"
	s.assets[assetKey(multiScriptableGUID, "11400000")] = map[string]any{
		"kind": "scriptableObject", "path": path, "guid": multiScriptableGUID, "fileId": "11400000",
	}
	prepared, err := s.Prepare(output)
	if err != nil {
		t.Fatal(err)
	}
	manifest, err := ManifestJSON(prepared.Manifest)
	if err != nil {
		t.Fatal(err)
	}
	objects := map[string]map[string]any{}
	for _, value := range array(manifest["assets"]) {
		asset := object(value)
		if str(asset["guid"]) != multiScriptableGUID {
			continue
		}
		id := str(asset["fileId"])
		if objects[id] != nil {
			t.Fatalf("same-file object %s published twice", id)
		}
		objects[id] = asset
		if str(asset["kind"]) != "scriptableObject" || str(asset["typeName"]) != "Fixture.Profile" {
			t.Fatalf("invalid subasset type descriptor: %#v", asset)
		}
		for _, reference := range array(asset["references"]) {
			ref := object(reference)
			if str(ref["property"]) == "m_Script" || str(ref["fileId"]) == "0" {
				continue
			}
			if str(ref["guid"]) != multiScriptableGUID {
				t.Fatalf("same-file reference did not receive source GUID: %#v", ref)
			}
		}
	}
	if len(objects) != 3 || objects["11400000"] == nil || objects["-8901537253420550725"] == nil || objects["9223372036854775807"] == nil {
		t.Fatalf("complete signed64 source identities not preserved: %#v", objects)
	}
	unchanged, err := os.ReadFile(physical)
	if err != nil || string(unchanged) != multiScriptableFixture {
		t.Fatal("source asset was modified")
	}
	warm, err := s.Prepare(output)
	if err != nil {
		t.Fatal(err)
	}
	if warm.Cache.YAMLParses != 0 || warm.Cache.FileReads != 0 {
		t.Fatalf("warm closure preparation reread unchanged SO sources: %+v", warm.Cache)
	}
}

func TestScriptableObjectRejectsMissingLocalReferencesAndSourceIdentity(t *testing.T) {
	for name, fixture := range map[string]string{
		"dangling local reference": strings.Replace(multiScriptableFixture, "components:\n  - {fileID: -8901537253420550725}", "components:\n  - {fileID: 123}", 1),
		"unsupported mixed class":  strings.Replace(multiScriptableFixture, "--- !u!114 &9223372036854775807", "--- !u!43 &9223372036854775807", 1),
	} {
		t.Run(name, func(t *testing.T) {
			documents, err := ParseUnityYAML([]byte(fixture))
			if err != nil {
				t.Fatal(err)
			}
			s, _, _ := fixtureSession(t)
			s.scriptTypes[multiScriptableScriptGUID] = "Fixture.Profile"
			asset := map[string]any{"path": "Assets/Profile.asset", "guid": multiScriptableGUID, "fileId": "11400000"}
			if err := s.refreshScriptableObject(asset, documents); err == nil {
				t.Fatal("invalid same-file source graph was accepted")
			}
		})
	}
	documents, err := ParseUnityYAML([]byte(multiScriptableFixture))
	if err != nil {
		t.Fatal(err)
	}
	s, _, _ := fixtureSession(t)
	s.scriptTypes[multiScriptableScriptGUID] = "Fixture.Profile"
	for _, id := range []string{"123", ""} {
		asset := map[string]any{"path": "Assets/Profile.asset", "guid": multiScriptableGUID, "fileId": id}
		if err := s.refreshScriptableObject(asset, documents); err == nil {
			t.Fatalf("missing/ambiguous source identity %q accepted", id)
		}
	}
}

func TestScriptableObjectPreservesExternalReferenceIdentity(t *testing.T) {
	documents, err := ParseUnityYAML([]byte(strings.Replace(multiScriptableFixture, "external: {fileID: 0}", "external: {fileID: 123, guid: 44444444444444444444444444444444, type: 2}", 1)))
	if err != nil {
		t.Fatal(err)
	}
	s, _, _ := fixtureSession(t)
	s.scriptTypes[multiScriptableScriptGUID] = "Fixture.Profile"
	asset := map[string]any{"path": "Assets/Profile.asset", "guid": multiScriptableGUID, "fileId": "11400000"}
	if err := s.refreshScriptableObject(asset, documents); err != nil {
		t.Fatal(err)
	}
	if str(object(documents[0].Data["external"])["guid"]) != "44444444444444444444444444444444" {
		t.Fatal("external asset GUID was rewritten as a same-file reference")
	}
}

func TestPanelSettingsRequiresPinnedBuiltInScriptIdentity(t *testing.T) {
	const panelFixture = `--- !u!114 &11400000
MonoBehaviour:
  m_Name: Panel
  m_Script: {fileID: 19101, guid: 0000000000000000e000000000000000, type: 0}
  m_EditorClassIdentifier: UnityEngine.dll::UnityEngine.UIElements.PanelSettings
`
	for name, fixture := range map[string]string{
		"valid":           panelFixture,
		"wrong script ID": strings.Replace(panelFixture, "19101", "19102", 1),
		"wrong class":     strings.Replace(panelFixture, "UnityEngine.UIElements.PanelSettings", "UnityEngine.OtherType", 1),
	} {
		t.Run(name, func(t *testing.T) {
			documents, err := ParseUnityYAML([]byte(fixture))
			if err != nil {
				t.Fatal(err)
			}
			s, _, _ := fixtureSession(t)
			asset := map[string]any{"path": "Assets/Panel.asset", "guid": multiScriptableGUID, "fileId": "11400000"}
			err = s.refreshScriptableObject(asset, documents)
			if name == "valid" {
				if err != nil || str(asset["typeName"]) != "UnityEngine.UIElements.PanelSettings" {
					t.Fatalf("pinned PanelSettings mapping failed: %v, %#v", err, asset)
				}
			} else if err == nil {
				t.Fatal("unverified engine MonoScript identity accepted")
			}
		})
	}
}
