package goproject

import (
	"path/filepath"
	"strings"
	"testing"
)

func TestSelectAuthoredSceneWithExistingHost(t *testing.T) {
	baseline, _, output := fixtureSession(t)
	other := "Assets/Net/Boot.unity"
	writeFixtureFile(t, filepath.Join(baseline.project, other), strings.ReplaceAll(fixtureScene, "Original", "Network boot"))
	writeFixtureFile(t, filepath.Join(baseline.project, other)+".meta", "guid: 22222222222222222222222222222222\n")
	session, err := NewSession(Options{Project: baseline.project, Manifest: filepath.Join(baseline.project, "baseline.json"), Scene: other})
	if err != nil {
		t.Fatal(err)
	}
	prepared, err := session.Prepare(output)
	if err != nil {
		t.Fatal(err)
	}
	manifest, err := ManifestJSON(prepared.Manifest)
	if err != nil {
		t.Fatal(err)
	}
	if prepared.Scene != other || manifest["scenePath"] != other || manifest["sceneName"] != "Boot" {
		t.Fatalf("selected source was not published: %v %+v", manifest["scenePath"], prepared)
	}
	if !strings.Contains(str(object(array(manifest["objects"])[0])["dataJson"]), "Network boot") {
		t.Fatal("selected scene contents were not loaded")
	}
	if array(manifest["assemblies"])[0] != "existing.dll" {
		t.Fatal("prepared native/managed host binding changed")
	}
}

func TestRejectInvalidSceneSelection(t *testing.T) {
	baseline, _, _ := fixtureSession(t)
	for _, scene := range []string{"/tmp/Boot.unity", "Assets/../Outside.unity", "Assets/Scenes/Missing.unity", "Library/Cached.unity", "Assets/Scenes/Sample.prefab"} {
		if _, err := NewSession(Options{Project: baseline.project, Manifest: filepath.Join(baseline.project, "baseline.json"), Scene: scene}); err == nil {
			t.Fatalf("accepted invalid source %s", scene)
		}
	}
}

func TestRuntimeAddressedPrefabIncludesExactRootAndSourceClosure(t *testing.T) {
	baseline, _, output := fixtureSession(t)
	path := "Assets/Net/Player.prefab"
	writeFixtureFile(t, filepath.Join(baseline.project, path), strings.Split(fixtureScene, "--- !u!1660057539")[0])
	writeFixtureFile(t, filepath.Join(baseline.project, path)+".meta", "guid: 33333333333333333333333333333333\n")
	session, err := NewSession(Options{Project: baseline.project, Manifest: filepath.Join(baseline.project, "baseline.json"), ExtraAssets: []string{path}})
	if err != nil {
		t.Fatal(err)
	}
	prepared, err := session.Prepare(output)
	if err != nil {
		t.Fatal(err)
	}
	manifest, _ := ManifestJSON(prepared.Manifest)
	assets := array(manifest["assets"])
	if len(assets) != 1 || object(assets[0])["kind"] != "prefab" || object(assets[0])["rootFileId"] != "10" {
		t.Fatalf("runtime prefab lost its graph/root: %+v", assets)
	}
	found := false
	for _, value := range array(manifest["sourceDependencies"]) {
		found = found || str(object(value)["path"]) == path
	}
	if !found {
		t.Fatal("runtime-addressed prefab was omitted from source identity")
	}
}
