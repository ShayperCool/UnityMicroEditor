package goproject

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

const nativeFixtureGUID = "22222222222222222222222222222222"

func nativeFixtureHash(t *testing.T, path string) string {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	sum := sha256.Sum256(data)
	return hex.EncodeToString(sum[:])
}
func nativeFixture(t *testing.T, extension string) (*Session, string, NativeReplacement, string, string) {
	t.Helper()
	initial, _, output := fixtureSession(t)
	logical := "Assets/Native" + extension
	source := filepath.Join(initial.project, logical)
	writeFixtureFile(t, source, "original imported source")
	writeFixtureFile(t, source+".meta", "fileFormatVersion: 2\nguid: "+nativeFixtureGUID+"\n")
	asset := map[string]any{"kind": "imported", "guid": nativeFixtureGUID, "path": logical, "fileId": "8300000", "typeName": "UnityEngine.AudioClip", "classId": 83}
	if extension == ".fbx" {
		asset["fileId"], asset["typeName"], asset["classId"] = "4300000", "UnityEngine.Mesh", 43
	}
	initial.baseline["assets"] = []any{asset}
	initial.baseline["sourceDependencies"] = []any{
		map[string]any{"path": logical, "sha256": nativeFixtureHash(t, source)},
		map[string]any{"path": logical + ".meta", "sha256": nativeFixtureHash(t, source+".meta")},
	}
	baseline := filepath.Join(initial.project, "baseline.json")
	encoded, _ := json.Marshal(initial.baseline)
	if err := os.WriteFile(baseline, encoded, 0600); err != nil {
		t.Fatal(err)
	}
	catalogPath := filepath.Join(initial.project, "catalog.json")
	cat := map[string]any{"schemaVersion": 1, "engineVersion": "6000.6.3f1", "project": initial.project, "assets": []any{asset}}
	if extension == ".fbx" {
		model := strings.Split(fixtureScene, "--- !u!1660057539")[0]
		docs, err := ParseUnityYAML([]byte(model))
		if err != nil {
			t.Fatal(err)
		}
		for i := range docs {
			docs[i].TypeName = "UnityEngine." + docs[i].TypeName
		}
		docs[0].Data["m_Layer"], docs[0].Data["m_TagString"], docs[0].Data["m_StaticEditorFlags"] = 0, "Untagged", 0
		cat["modelPrefabs"] = []any{map[string]any{"guid": nativeFixtureGUID, "path": logical, "objects": docs}}
	}
	encoded, _ = json.Marshal(cat)
	if err := os.WriteFile(catalogPath, encoded, 0600); err != nil {
		t.Fatal(err)
	}
	s, err := NewSession(Options{Project: initial.project, Manifest: baseline, NativeCatalog: catalogPath})
	if err != nil {
		t.Fatal(err)
	}
	runtime := filepath.Join(output, "native/runtime.bin")
	resource := filepath.Join(output, "native/audio.resource")
	writeFixtureFile(t, runtime, "independently adapted immutable runtime file")
	writeFixtureFile(t, resource, "immutable streamed audio payload")
	replacement := NativeReplacement{GUID: nativeFixtureGUID, SourcePath: logical, SourceSHA256: nativeFixtureHash(t, source), MetaSHA256: nativeFixtureHash(t, source+".meta"), Content: NativeContentFile{GUID: nativeFixtureGUID, Path: runtime, SHA256: nativeFixtureHash(t, runtime), Objects: []NativeContentObject{{FileID: str(asset["fileId"]), TypeName: str(asset["typeName"])}}}}
	if extension == ".wav" {
		replacement.Content.Resources = []NativeContentResource{{Path: resource, VirtualPath: "VirtualArtifacts/Primary/" + nativeFixtureGUID + ".resource", SHA256: nativeFixtureHash(t, resource)}}
	} else {
		for _, doc := range s.models[nativeFixtureGUID] {
			replacement.Content.Objects = append(replacement.Content.Objects, NativeContentObject{FileID: doc.FileID, TypeName: doc.TypeName})
		}
		replacement.ModelGraph, err = s.nativeModelGraph(nativeFixtureGUID)
		if err != nil {
			t.Fatal(err)
		}
	}
	return s, output, replacement, baseline, catalogPath
}

func TestNativeWAVOverlayRefreshesHashAndPreservesBaselineAndRevisions(t *testing.T) {
	s, output, r, baseline, catalogPath := nativeFixture(t, ".wav")
	originalBaseline, _ := os.ReadFile(baseline)
	originalCatalog, _ := os.ReadFile(catalogPath)
	before, err := s.Prepare(output)
	if err != nil {
		t.Fatal(err)
	}
	oldRevision, _ := os.ReadFile(before.Manifest)
	source := filepath.Join(s.project, r.SourcePath)
	writeFixtureFile(t, source, "changed source audio")
	r.SourceSHA256 = nativeFixtureHash(t, source)
	if _, err := s.Prepare(output); err == nil {
		t.Fatal("published changed WAV without importer")
	}
	if err := s.ReplaceNativeContent(r); err != nil {
		t.Fatal(err)
	}
	// Ownership of all nested replacement fields belongs to the session.
	r.Content.Objects[0].TypeName = "modified by caller"
	prepared, err := s.Prepare(output)
	if err != nil {
		t.Fatal(err)
	}
	manifest, err := ManifestJSON(prepared.Manifest)
	if err != nil {
		t.Fatal(err)
	}
	content := object(array(manifest["nativeContentFiles"])[0])
	if str(content["guid"]) != nativeFixtureGUID || str(object(array(content["objects"])[0])["typeName"]) != "UnityEngine.AudioClip" {
		t.Fatalf("wrong native overlay: %+v", content)
	}
	found := false
	for _, v := range array(manifest["sourceDependencies"]) {
		d := object(v)
		if d["path"] == r.SourcePath {
			found = d["sha256"] == r.SourceSHA256
		}
	}
	if !found {
		t.Fatal("source dependency retained stale hash")
	}
	unchangedBaseline, _ := os.ReadFile(baseline)
	unchangedCatalog, _ := os.ReadFile(catalogPath)
	unchangedRevision, _ := os.ReadFile(before.Manifest)
	if string(originalBaseline) != string(unchangedBaseline) || string(originalCatalog) != string(unchangedCatalog) || string(oldRevision) != string(unchangedRevision) {
		t.Fatal("an immutable baseline, catalog, or published revision changed")
	}
	warm, err := s.Prepare(output)
	if err != nil {
		t.Fatal(err)
	}
	if warm.Cache.FileReads != 0 {
		t.Fatalf("warm overlay verification reread immutable files: %+v", warm.Cache)
	}
	writeFixtureFile(t, source, "changed a second time")
	var needs *NeedsImportError
	if _, err := s.Prepare(output); !errors.As(err, &needs) {
		t.Fatalf("subsequent source changes must require an import: %v", err)
	}
}

func TestNativeReplacementFailsClosedBeforeChangingOverlay(t *testing.T) {
	cases := map[string]func(*NativeReplacement){
		"source hash":   func(r *NativeReplacement) { r.SourceSHA256 = strings.Repeat("0", 64) },
		"metadata hash": func(r *NativeReplacement) { r.MetaSHA256 = strings.Repeat("0", 64) },
		"unknown GUID": func(r *NativeReplacement) {
			r.GUID, r.Content.GUID = "33333333333333333333333333333333", "33333333333333333333333333333333"
		},
		"wrong source":     func(r *NativeReplacement) { r.SourcePath = "Assets/Other.wav" },
		"missing object":   func(r *NativeReplacement) { r.Content.Objects = nil },
		"duplicate object": func(r *NativeReplacement) { r.Content.Objects = append(r.Content.Objects, r.Content.Objects[0]) },
		"changed type":     func(r *NativeReplacement) { r.Content.Objects[0].TypeName = "UnityEngine.Mesh" },
		"changed ID":       func(r *NativeReplacement) { r.Content.Objects[0].FileID = "8300001" },
		"runtime hash":     func(r *NativeReplacement) { r.Content.SHA256 = strings.Repeat("0", 64) },
		"resource hash":    func(r *NativeReplacement) { r.Content.Resources[0].SHA256 = strings.Repeat("0", 64) },
		"resource alias":   func(r *NativeReplacement) { r.Content.Resources[0].VirtualPath = "../../other" },
		"unknown dependency": func(r *NativeReplacement) {
			r.Content.Dependencies = []NativeContentDependency{{GUID: "33333333333333333333333333333333"}}
		},
		"fake global": func(r *NativeReplacement) {
			r.Content.Dependencies = []NativeContentDependency{{GUID: "33333333333333333333333333333333", Global: true}}
		},
	}
	for name, change := range cases {
		t.Run(name, func(t *testing.T) {
			s, output, r, _, _ := nativeFixture(t, ".wav")
			before := clone(s.critical)
			change(&r)
			if err := s.ReplaceNativeContent(r); err == nil {
				t.Fatal("accepted invalid native replacement")
			}
			if len(s.nativeOverlays) != 0 || !reflect.DeepEqual(before, s.critical) {
				t.Fatal("failed replacement changed session expectations")
			}
			if _, err := s.Prepare(output); err != nil {
				t.Fatal("failed replacement invalidated original source", err)
			}
		})
	}
}

func TestNativeOverlayRejectsArtifactsChangedAfterAcceptance(t *testing.T) {
	for _, file := range []string{"runtime", "resource"} {
		t.Run(file, func(t *testing.T) {
			s, output, r, _, _ := nativeFixture(t, ".wav")
			if err := s.ReplaceNativeContent(r); err != nil {
				t.Fatal(err)
			}
			first, err := s.Prepare(output)
			if err != nil {
				t.Fatal(err)
			}
			path := r.Content.Path
			if file == "resource" {
				path = r.Content.Resources[0].Path
			}
			writeFixtureFile(t, path, "changed after acceptance")
			if _, err := s.Prepare(output); err == nil {
				t.Fatal("published changed native artifact")
			}
			revisions, _ := os.ReadDir(filepath.Dir(first.Manifest))
			if len(revisions) != 1 {
				t.Fatal("previous manifest disappeared")
			}
			all, _ := os.ReadDir(filepath.Dir(filepath.Dir(first.Manifest)))
			if len(all) != 1 {
				t.Fatal("failed verification published a new revision")
			}
		})
	}
}

func TestNativeFBXReplacementRequiresIdenticalReferencesAndAuthoredFields(t *testing.T) {
	cases := map[string]func(*NativeReplacement){
		"unchanged":         func(*NativeReplacement) {},
		"missing graph":     func(r *NativeReplacement) { r.ModelGraph = nil },
		"changed owner":     func(r *NativeReplacement) { r.ModelGraph[1].References[0].Targets[0].FileID = "20" },
		"changed name":      func(r *NativeReplacement) { r.ModelGraph[0].Scalars["m_Name"] = "different" },
		"changed transform": func(r *NativeReplacement) { object(r.ModelGraph[1].Scalars["m_LocalPosition"])["x"] = 1 },
		"unknown scalar":    func(r *NativeReplacement) { r.ModelGraph[0].Scalars["unexpected"] = 1 },
		"missing ID":        func(r *NativeReplacement) { r.Content.Objects = r.Content.Objects[:2] },
	}
	for name, change := range cases {
		t.Run(name, func(t *testing.T) {
			s, output, r, _, _ := nativeFixture(t, ".fbx")
			change(&r)
			err := s.ReplaceNativeContent(r)
			if name != "unchanged" {
				if err == nil {
					t.Fatal("accepted changed model graph")
				}
				return
			}
			if err != nil {
				t.Fatal(err)
			}
			if _, err := s.Prepare(output); err != nil {
				t.Fatal(err)
			}
		})
	}
	// Unity exports binary32 values through decimal JSON; those decimal spellings
	// need to compare at the original native precision.
	a := map[string]any{"x": json.Number("0.1")}
	b := map[string]any{"x": float64(float32(0.1))}
	if !nativeScalarEqual(a, b, true) || nativeScalarEqual(a, b, false) {
		t.Fatal("model precision normalizer is incorrect")
	}
}

func TestNativeReplacementRejectsCrossOverlayDependencyCycle(t *testing.T) {
	s, _, first, _, _ := nativeFixture(t, ".wav")
	if err := s.ReplaceNativeContent(first); err != nil {
		t.Fatal(err)
	}
	guid := "33333333333333333333333333333333"
	path := "Assets/Second.wav"
	source := filepath.Join(s.project, path)
	writeFixtureFile(t, source, "second source")
	writeFixtureFile(t, source+".meta", "guid: "+guid+"\n")
	s.assets[assetKey(guid, "8300000")] = map[string]any{"kind": "imported", "guid": guid, "path": path, "fileId": "8300000", "typeName": "UnityEngine.AudioClip"}
	second := NativeReplacement{GUID: guid, SourcePath: path, SourceSHA256: nativeFixtureHash(t, source), MetaSHA256: nativeFixtureHash(t, source+".meta"), Content: first.Content}
	second.Content.GUID = guid
	second.Content.Resources = nil
	second.Content.Dependencies = []NativeContentDependency{{GUID: first.GUID}}
	if err := s.ReplaceNativeContent(second); err != nil {
		t.Fatal(err)
	}
	first.Content.Dependencies = []NativeContentDependency{{GUID: guid}}
	if err := s.ReplaceNativeContent(first); err == nil {
		t.Fatal("accepted a cycle between previously valid overlays")
	}
	if len(s.nativeOverlays[first.GUID].Content.Dependencies) != 0 {
		t.Fatal("failed cycle validation replaced valid overlay")
	}
}

func newNativeTextureFixture(t *testing.T) (*Session, string, NativeReplacement) {
	t.Helper()
	s, _, output := fixtureSession(t)
	logical := "Assets/NewTexture.png"
	source := filepath.Join(s.project, logical)
	writeFixtureFile(t, source, "native texture source bytes")
	writeFixtureFile(t, source+".meta", "fileFormatVersion: 2\nguid: "+nativeFixtureGUID+"\nTextureImporter:\n  textureType: 0\n")
	runtime := filepath.Join(output, "native/texture.bin")
	writeFixtureFile(t, runtime, "independently validated native texture artifact")
	r := NativeReplacement{GUID: nativeFixtureGUID, SourcePath: logical, SourceSHA256: nativeFixtureHash(t, source), MetaSHA256: nativeFixtureHash(t, source+".meta"),
		Content: NativeContentFile{GUID: nativeFixtureGUID, Path: runtime, SHA256: nativeFixtureHash(t, runtime), Objects: []NativeContentObject{{FileID: "2800000", TypeName: "UnityEngine.Texture2D"}}}}
	return s, output, r
}

func TestNewNativeProjectTexturePublishesWithoutMutatingBaseline(t *testing.T) {
	s, output, r := newNativeTextureFixture(t)
	baseline := clone(s.baseline)
	before, err := s.Prepare(output)
	if err != nil {
		t.Fatal(err)
	}
	oldRevision, _ := os.ReadFile(before.Manifest)
	if err := s.ReplaceNativeContent(r); err != nil {
		t.Fatal(err)
	}
	r.Content.Objects[0].TypeName = "caller mutated descriptor"
	prepared, err := s.Prepare(output)
	if err != nil {
		t.Fatal(err)
	}
	manifest, err := ManifestJSON(prepared.Manifest)
	if err != nil {
		t.Fatal(err)
	}
	var texture map[string]any
	for _, value := range array(manifest["assets"]) {
		asset := object(value)
		if str(asset["guid"]) == nativeFixtureGUID {
			texture = asset
		}
	}
	if texture == nil || str(texture["kind"]) != "imported" || str(texture["fileId"]) != "2800000" || str(texture["typeName"]) != "UnityEngine.Texture2D" || str(texture["path"]) != r.SourcePath {
		t.Fatalf("new source did not publish verified native identity: %+v", texture)
	}
	if len(array(manifest["nativeContentFiles"])) != 1 || !reflect.DeepEqual(baseline, s.baseline) {
		t.Fatal("native append changed baseline or omitted runtime descriptor")
	}
	unchanged, _ := os.ReadFile(before.Manifest)
	if string(unchanged) != string(oldRevision) {
		t.Fatal("native append changed existing immutable revision")
	}
	writeFixtureFile(t, filepath.Join(s.project, r.SourcePath), "changed texture pixels")
	var needs *NeedsImportError
	if _, err := s.Prepare(output); !errors.As(err, &needs) {
		t.Fatalf("accepted texture change without an importer: %v", err)
	}
}

func TestNewNativeProjectSourceRejectsInvalidOwnershipAndRuntimeIdentity(t *testing.T) {
	for _, name := range []string{"wrong GUID", "duplicate GUID", "wrong type", "wrong ID", "extra object", "graph", "stale source"} {
		t.Run(name, func(t *testing.T) {
			s, _, r := newNativeTextureFixture(t)
			before := clone(s.assets)
			switch name {
			case "wrong GUID":
				r.GUID = "33333333333333333333333333333333"
				r.Content.GUID = r.GUID
			case "duplicate GUID":
				writeFixtureFile(t, filepath.Join(s.project, "Assets/Duplicate.png"), "duplicate source")
				writeFixtureFile(t, filepath.Join(s.project, "Assets/Duplicate.png.meta"), "guid: "+r.GUID+"\n")
			case "wrong type":
				r.Content.Objects[0].TypeName = "UnityEngine.AudioClip"
			case "wrong ID":
				r.Content.Objects[0].FileID = "2800001"
			case "extra object":
				r.Content.Objects = append(r.Content.Objects, NativeContentObject{FileID: "21300000", TypeName: "UnityEngine.Sprite"})
			case "graph":
				r.ModelGraph = []NativeModelGraphObject{{FileID: "1", ClassID: 1}}
			case "stale source":
				writeFixtureFile(t, filepath.Join(s.project, r.SourcePath), "pixels changed after native import")
			}
			if err := s.ReplaceNativeContent(r); err == nil {
				t.Fatal("accepted invalid new native source")
			}
			if len(s.nativeOverlays) != 0 || !reflect.DeepEqual(before, s.assets) {
				t.Fatal("failed new source registration changed published native identities")
			}
		})
	}
}

func TestNewNativeUIAndFontSourcesPublishExactObjects(t *testing.T) {
	profiles := map[string][]NativeContentObject{
		".uss":  {{FileID: "7433441132597879392", TypeName: "UnityEngine.UIElements.StyleSheet"}},
		".uxml": {{FileID: "9197481963319205126", TypeName: "UnityEngine.UIElements.VisualTreeAsset"}, {FileID: "1068529831357515114", TypeName: "UnityEngine.UIElements.StyleSheet"}},
		".ttf":  {{FileID: "12800000", TypeName: "UnityEngine.Font"}, {FileID: "2100000", TypeName: "UnityEngine.Material"}, {FileID: "2800000", TypeName: "UnityEngine.Texture2D"}},
	}
	for extension, identities := range profiles {
		t.Run(extension, func(t *testing.T) {
			s, output, r := newNativeTextureFixture(t)
			old := filepath.Join(s.project, r.SourcePath)
			r.SourcePath = "Assets/NewNativeUI" + extension
			source := filepath.Join(s.project, r.SourcePath)
			if err := os.Rename(old, source); err != nil {
				t.Fatal(err)
			}
			if err := os.Rename(old+".meta", source+".meta"); err != nil {
				t.Fatal(err)
			}
			r.Content.Objects = identities
			if err := s.ReplaceNativeContent(r); err != nil {
				t.Fatal(err)
			}
			prepared, err := s.Prepare(output)
			if err != nil {
				t.Fatal(err)
			}
			manifest, err := ManifestJSON(prepared.Manifest)
			if err != nil {
				t.Fatal(err)
			}
			seen := 0
			for _, value := range array(manifest["assets"]) {
				asset := object(value)
				if str(asset["guid"]) == r.GUID {
					seen++
					if str(asset["kind"]) != "imported" || nativeContentClass(str(asset["typeName"])) == 0 {
						t.Fatal("native UI asset publication omitted exact type")
					}
				}
			}
			if seen != len(identities) {
				t.Fatal("native UI subasset identities were lost")
			}
			bad := r
			bad.Content.Objects = append([]NativeContentObject(nil), r.Content.Objects...)
			bad.Content.Objects[0].TypeName = "UnityEditor.StyleSheetImporter"
			if err := s.ReplaceNativeContent(bad); err == nil {
				t.Fatal("Editor importer identity became a runtime object")
			}
		})
	}
}

func TestNativeTextureTakesOverExistingRawTextureSettings(t *testing.T) {
	s, output, r := newNativeTextureFixture(t)
	key := assetKey(r.GUID, "2800000")
	s.assets[key] = map[string]any{"kind": "texture", "guid": r.GUID, "fileId": "2800000", "path": r.SourcePath, "classId": 28, "typeName": "UnityEngine.Texture2D"}
	baseline := clone(s.baseline)
	if err := s.ReplaceNativeContent(r); err != nil {
		t.Fatal(err)
	}
	if str(s.assets[key]["kind"]) != "imported" {
		t.Fatal("verified native importer output did not replace the raw texture path")
	}
	if _, err := s.Prepare(output); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(baseline, s.baseline) {
		t.Fatal("native texture transition mutated the immutable baseline")
	}
}
