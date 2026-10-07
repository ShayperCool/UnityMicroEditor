package goproject

import (
	"bytes"
	"image"
	"image/png"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

func TestNativeDiscoveryTraversesAuthoredClosureAndSkipsUnrelatedAssets(t *testing.T) {
	session, scene, _ := fixtureSession(t)
	guids := map[string]string{
		"Chain.prefab": "22222222222222222222222222222222",
		"Data.asset":   "33333333333333333333333333333333",
		"Mesh.fbx":     "44444444444444444444444444444444",
		"Sound.wav":    "55555555555555555555555555555555",
		"Other.fbx":    "66666666666666666666666666666666",
		"Extra.prefab": "77777777777777777777777777777777",
		"Extra.fbx":    "88888888888888888888888888888888",
	}
	for filename, guid := range guids {
		path := filepath.Join(session.project, "Assets", filename)
		writeFixtureFile(t, path, "native source "+filename)
		writeFixtureFile(t, path+".meta", "guid: "+guid+"\n")
	}
	writeFixtureFile(t, scene, fixtureScene+"--- !u!1001 &90\nPrefabInstance:\n  m_SourcePrefab: {fileID: 100100000, guid: "+guids["Chain.prefab"]+", type: 3}\n")
	writeFixtureFile(t, filepath.Join(session.project, "Assets/Chain.prefab"), "--- !u!114 &91\nMonoBehaviour:\n  data: {fileID: 11400000, guid: "+guids["Data.asset"]+", type: 2}\n")
	// A cyclic data reference is bounded by visited source identity; it does not
	// trigger model expansion or import unrelated metadata-indexed FBX files.
	writeFixtureFile(t, filepath.Join(session.project, "Assets/Data.asset"), "--- !u!114 &11400000\nMonoBehaviour:\n  prefab: {fileID: 100100000, guid: "+guids["Chain.prefab"]+", type: 3}\n  mesh: {fileID: 4300000, guid: "+guids["Mesh.fbx"]+", type: 3}\n  sound: {fileID: 8300000, guid: "+guids["Sound.wav"]+", type: 3}\n")
	writeFixtureFile(t, filepath.Join(session.project, "Assets/Extra.prefab"), "--- !u!33 &92\nMeshFilter:\n  m_Mesh: {fileID: 4300000, guid: "+guids["Extra.fbx"]+", type: 3}\n")
	session.extraAssets = []string{"Assets/Extra.prefab"}
	actual, err := session.PendingNativeImportSources()
	expected := []string{"Assets/Extra.fbx", "Assets/Mesh.fbx", "Assets/Sound.wav"}
	if err != nil || !reflect.DeepEqual(actual, expected) {
		t.Fatalf("closure native discovery=%v, err=%v; expected %v", actual, err, expected)
	}
	for _, filename := range []string{"Assets/Extra.fbx", "Assets/Mesh.fbx", "Assets/Sound.wav"} {
		_, sourceSHA, _ := session.read(filepath.Join(session.project, filename), false)
		_, metaSHA, _ := session.read(filepath.Join(session.project, filename+".meta"), true)
		session.nativeOverlays[session.byPath[filename]] = NativeReplacement{SourceSHA256: sourceSHA, MetaSHA256: metaSHA}
	}
	if actual, err = session.PendingNativeImportSources(); err != nil || len(actual) != 0 {
		t.Fatalf("current native overlays were rediscovered: %v %v", actual, err)
	}
	writeFixtureFile(t, filepath.Join(session.project, "Assets/Mesh.fbx"), "changed native model")
	writeFixtureFile(t, filepath.Join(session.project, "Assets/Sound.wav.meta"), "guid: "+guids["Sound.wav"]+"\nchanged: 1\n")
	if actual, err = session.PendingNativeImportSources(); err != nil || !reflect.DeepEqual(actual, []string{"Assets/Mesh.fbx", "Assets/Sound.wav"}) {
		t.Fatalf("changed source or importer metadata was not rediscovered: %v %v", actual, err)
	}
}

func TestNativeDiscoveryKeepsRawPNGFastAndSelectsNativeNPOTResize(t *testing.T) {
	session, scene, _ := fixtureSession(t)
	texture := filepath.Join(session.project, "Assets/Texture.png")
	writeFixtureFile(t, texture+".meta", authoredTextureMeta)
	writeFixtureFile(t, scene, fixtureScene+"--- !u!114 &99\nMonoBehaviour:\n  image: {fileID: 2800000, guid: 22222222222222222222222222222222, type: 3}\n")
	writePNG := func(width int) {
		var encoded bytes.Buffer
		if err := png.Encode(&encoded, image.NewNRGBA(image.Rect(0, 0, width, 4))); err != nil {
			t.Fatal(err)
		}
		writeFixtureFile(t, texture, encoded.String())
	}
	writePNG(4)
	if pending, err := session.PendingNativeImportSources(); err != nil || len(pending) != 0 {
		t.Fatalf("raw uncompressed texture needlessly requires native import: %v %v", pending, err)
	}
	writePNG(3)
	if pending, err := session.PendingNativeImportSources(); err != nil || !reflect.DeepEqual(pending, []string{"Assets/Texture.png"}) {
		t.Fatalf("NPOT resampling did not select actual TextureImporter: %v %v", pending, err)
	}
	writePNG(4)
	writeFixtureFile(t, texture+".meta", strings.Replace(authoredTextureMeta, "overridden: 0", "overridden: 1", 1))
	if pending, err := session.PendingNativeImportSources(); err != nil || !reflect.DeepEqual(pending, []string{"Assets/Texture.png"}) {
		t.Fatalf("platform compression did not select native importer: %v %v", pending, err)
	}
}
