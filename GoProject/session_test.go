package goproject

import (
	"encoding/json"
	"errors"
	"math/big"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

const fixtureGUID = "11111111111111111111111111111111"
const fixtureScene = `%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &10
GameObject:
  m_Component:
  - component: {fileID: 11}
  m_Name: Original
  m_IsActive: 1
--- !u!4 &11
Transform:
  m_GameObject: {fileID: 10}
  m_LocalPosition: {x: 0, y: 0, z: 0}
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalScale: {x: 1, y: 1, z: 1}
  m_Father: {fileID: 0}
  m_Children: []
--- !u!1660057539 &9223372036854775807
SceneRoots:
  m_Roots:
  - {fileID: 11}
`

func writeFixtureFile(t *testing.T, path, content string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(content), 0644); err != nil {
		t.Fatal(err)
	}
}

func fixtureSession(t *testing.T) (*Session, string, string) {
	t.Helper()
	project := t.TempDir()
	scene := filepath.Join(project, "Assets/Scenes/Sample.unity")
	writeFixtureFile(t, scene, fixtureScene)
	writeFixtureFile(t, scene+".meta", "guid: "+fixtureGUID+"\n")
	manifest := map[string]any{"schemaVersion": 1, "supported": true, "unityVersion": "6000.6.3f1", "projectRoot": project, "scenePath": "Assets/Scenes/Sample.unity", "sceneName": "Sample", "objects": []any{}, "assets": []any{}, "assemblies": []string{"existing.dll"}, "sourceDependencies": []any{}}
	b, _ := json.Marshal(manifest)
	path := filepath.Join(project, "baseline.json")
	if err := os.WriteFile(path, b, 0644); err != nil {
		t.Fatal(err)
	}
	s, err := NewSession(Options{Project: project, Manifest: path})
	if err != nil {
		t.Fatal(err)
	}
	return s, scene, filepath.Join(project, "output")
}

func TestSessionRefreshesSceneAndAddsGameObjectWithoutModifyingPublishedRevision(t *testing.T) {
	s, scene, output := fixtureSession(t)
	first, err := s.Prepare(output)
	if err != nil {
		t.Fatal(err)
	}
	oldBytes, _ := os.ReadFile(first.Manifest)
	changed := strings.Replace(fixtureScene, "m_Name: Original", "m_Name: Modified", 1)
	changed = strings.Replace(changed, "  - {fileID: 11}\n", "  - {fileID: 11}\n  - {fileID: 21}\n", 1)
	changed += `--- !u!1 &20
GameObject:
  m_Component:
  - component: {fileID: 21}
  m_Name: Added
  m_IsActive: 1
--- !u!4 &21
Transform:
  m_GameObject: {fileID: 20}
  m_LocalPosition: {x: 3, y: 2, z: 1}
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalScale: {x: 1, y: 1, z: 1}
  m_Father: {fileID: 0}
  m_Children: []
`
	writeFixtureFile(t, scene, changed)
	second, err := s.Prepare(output)
	if err != nil {
		t.Fatal(err)
	}
	if second.Objects != 5 || second.Cache.YAMLParses != 1 {
		t.Fatalf("addition was not refreshed: %+v", second)
	}
	newManifest, _ := ManifestJSON(second.Manifest)
	found := false
	for _, v := range array(newManifest["objects"]) {
		o := object(v)
		if str(o["fileId"]) == "10" {
			var data map[string]any
			jsonDecodeString(str(o["dataJson"]), &data)
			found = data["m_Name"] == "Modified"
		}
	}
	if !found {
		t.Fatal("authored name did not update")
	}
	stillOld, _ := os.ReadFile(first.Manifest)
	if string(oldBytes) != string(stillOld) {
		t.Fatal("previous revision changed")
	}
	warm, err := s.Prepare(output)
	if err != nil {
		t.Fatal(err)
	}
	if warm.Cache.YAMLParses != 0 || warm.Cache.FileReads != 0 {
		t.Fatalf("warm preparation reread source: %+v", warm.Cache)
	}
}

func TestSessionRejectsDanglingAndCyclicScene(t *testing.T) {
	s, scene, output := fixtureSession(t)
	for _, changed := range []string{strings.Replace(fixtureScene, "m_GameObject: {fileID: 10}", "m_GameObject: {fileID: 99}", 1), strings.Replace(fixtureScene, "m_Father: {fileID: 0}", "m_Father: {fileID: 11}", 1)} {
		writeFixtureFile(t, scene, changed)
		if _, err := s.Prepare(output); err == nil {
			t.Fatal("published invalid graph")
		}
	}
}

func TestSessionRejectsStaleNativeSourceAndKeepsPreviousRevision(t *testing.T) {
	s, _, output := fixtureSession(t)
	native := filepath.Join(s.project, "Assets/Audio.wav")
	writeFixtureFile(t, native, "original")
	_, expected, err := s.read(native, false)
	if err != nil {
		t.Fatal(err)
	}
	s.critical["Assets/Audio.wav"] = expected
	s.baseline["sourceDependencies"] = []any{map[string]any{"path": "Assets/Audio.wav", "sha256": expected, "bytes": 8}}
	good, err := s.Prepare(output)
	if err != nil {
		t.Fatal(err)
	}
	writeFixtureFile(t, native, "modified")
	_, err = s.Prepare(output)
	var importError *NeedsImportError
	if !errors.As(err, &importError) {
		t.Fatalf("expected importer diagnostic, got %v", err)
	}
	if _, err := os.Stat(good.Manifest); err != nil {
		t.Fatal("valid revision lost")
	}
}

func TestPrefabInstanceOverridesAndStrippedReferences(t *testing.T) {
	s, scene, output := fixtureSession(t)
	prefab := filepath.Join(s.project, "Assets/Base.prefab")
	writeFixtureFile(t, prefab, strings.Replace(fixtureScene, "--- !u!1660057539 &9223372036854775807\nSceneRoots:\n  m_Roots:\n  - {fileID: 11}\n", "", 1))
	guid := "22222222222222222222222222222222"
	writeFixtureFile(t, prefab+".meta", "guid: "+guid+"\n")
	sceneText := `--- !u!1001 &100
PrefabInstance:
  m_SourcePrefab: {fileID: 100100000, guid: 22222222222222222222222222222222, type: 3}
  m_Modification:
    m_TransformParent: {fileID: 0}
    m_Modifications:
    - target: {fileID: 11, guid: 22222222222222222222222222222222, type: 3}
      propertyPath: m_LocalPosition.x
      value: 42
      objectReference: {fileID: 0}
    - target: {fileID: 10, guid: 22222222222222222222222222222222, type: 3}
      propertyPath: m_Name
      value: Authored Instance
      objectReference: {fileID: 0}
--- !u!4 &200 stripped
Transform:
  m_CorrespondingSourceObject: {fileID: 11, guid: 22222222222222222222222222222222, type: 3}
  m_PrefabInstance: {fileID: 100}
--- !u!1660057539 &9223372036854775807
SceneRoots:
  m_Roots:
  - {fileID: 200}
`
	writeFixtureFile(t, scene, sceneText)
	s.Invalidate([]string{prefab + ".meta"})
	prepared, err := s.Prepare(output)
	if err != nil {
		t.Fatal(err)
	}
	manifest, _ := ManifestJSON(prepared.Manifest)
	foundName, foundPosition := false, false
	for _, v := range array(manifest["objects"]) {
		o := object(v)
		var data map[string]any
		jsonDecodeString(str(o["dataJson"]), &data)
		if str(o["fileId"]) == "100:10" {
			foundName = data["m_Name"] == "Authored Instance"
		}
		if str(o["fileId"]) == "100:11" {
			foundPosition = number(object(data["m_LocalPosition"])["x"]) == 42
		}
	}
	if !foundName || !foundPosition {
		t.Fatal("instance overrides did not apply")
	}
}

// Opt-in parity check reads the real Dungeon scene and import catalog without
// mutating either. This is also used for full-scene preparation measurements.
func TestActualSampleSceneParity(t *testing.T) {
	project := os.Getenv("UNITY_PLAY_PARITY_PROJECT")
	if project == "" {
		t.Skip("set UNITY_PLAY_PARITY_PROJECT, _MANIFEST, _CATALOG for full SampleScene parity")
	}
	manifestPath := os.Getenv("UNITY_PLAY_PARITY_MANIFEST")
	catalogPath := os.Getenv("UNITY_PLAY_PARITY_CATALOG")
	s, err := NewSession(Options{Project: project, Manifest: manifestPath, NativeCatalog: catalogPath})
	if err != nil {
		t.Fatal(err)
	}
	prepared, err := s.Prepare(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	actual, err := ManifestJSON(prepared.Manifest)
	if err != nil {
		t.Fatal(err)
	}
	compareManifestObjects(t, array(s.baseline["objects"]), array(actual["objects"]))
	oldAssets, newAssets := array(s.baseline["assets"]), array(actual["assets"])
	if len(oldAssets) != len(newAssets) {
		t.Fatalf("asset count %d != %d", len(newAssets), len(oldAssets))
	}
	for i, a := range oldAssets {
		old, new := object(a), object(newAssets[i])
		if str(old["guid"]) != str(new["guid"]) || str(old["fileId"]) != str(new["fileId"]) {
			t.Fatal("asset identities differ")
		}
		if str(old["kind"]) == "prefab" {
			compareManifestObjects(t, array(old["objects"]), array(new["objects"]))
		}
	}
	warm, err := s.Prepare(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	if warm.Cache.FileReads != 0 || warm.Cache.YAMLParses != 0 {
		t.Fatalf("warm preparation did not reuse cache: %+v", warm.Cache)
	}
	t.Logf("full scene %d objects %d assets: first %.3fs, warm %.3fs %+v", prepared.Objects, prepared.Assets, prepared.ElapsedSeconds, warm.ElapsedSeconds, warm.Cache)
}

func compareManifestObjects(t *testing.T, want, got []any) {
	t.Helper()
	if len(want) != len(got) {
		t.Fatalf("object count %d != %d", len(got), len(want))
	}
	for i := range want {
		if str(object(want[i])["fileId"]) != str(object(got[i])["fileId"]) {
			t.Fatalf("object ordering differs at %d: %s != %s", i, object(got[i])["fileId"], object(want[i])["fileId"])
		}
	}
	byID := map[string]map[string]any{}
	for _, v := range want {
		o := object(v)
		byID[str(o["fileId"])] = o
	}
	for _, v := range got {
		o := object(v)
		old := byID[str(o["fileId"])]
		if old == nil {
			t.Fatalf("new identity %s", str(o["fileId"]))
		}
		for _, field := range []string{"classId", "gameObjectId", "sourcePath", "guid", "typeName", "skipMissingScript"} {
			if str(old[field]) != str(o[field]) {
				t.Fatalf("object %s field %s changed", o["fileId"], field)
			}
		}
		for _, field := range []string{"dataJson", "scalarDataJson"} {
			var a, b any
			if err := jsonDecodeString(str(old[field]), &a); err != nil {
				t.Fatal(err)
			}
			if err := jsonDecodeString(str(o[field]), &b); err != nil {
				t.Fatal(err)
			}
			if !reflect.DeepEqual(normalizeNumbers(a), normalizeNumbers(b)) {
				t.Fatalf("object %s %s differs", o["fileId"], field)
			}
		}
	}
}

func normalizeNumbers(value any) any {
	switch v := value.(type) {
	case json.Number:
		r, ok := new(big.Rat).SetString(string(v))
		if ok {
			return "number:" + r.RatString()
		}
		return v
	case map[string]any:
		r := map[string]any{}
		for k, item := range v {
			r[k] = normalizeNumbers(item)
		}
		return r
	case []any:
		r := make([]any, len(v))
		for i, item := range v {
			r[i] = normalizeNumbers(item)
		}
		return r
	default:
		return value
	}
}

func TestActualSampleSceneIterationInReflinkFixture(t *testing.T) {
	project := os.Getenv("UNITY_PLAY_PARITY_PROJECT")
	if project == "" {
		t.Skip("set full SampleScene parity environment to run isolated source iteration")
	}
	fixtureRoot := filepath.Join(project, "Temp/UnityPlay/go-project")
	if err := os.MkdirAll(fixtureRoot, 0755); err != nil {
		t.Fatal(err)
	}
	fixture, err := os.MkdirTemp(fixtureRoot, "sample-iteration-")
	if err != nil {
		t.Fatal(err)
	}
	defer os.RemoveAll(fixture)
	command := exec.Command("cp", "-a", "--reflink=always", filepath.Join(project, "Assets"), filepath.Join(project, "Packages"), filepath.Join(project, "ProjectSettings"), fixture)
	if output, err := command.CombinedOutput(); err != nil {
		t.Fatalf("isolated XFS source fixture: %v %s", err, output)
	}
	var baseline, native map[string]any
	if err := readJSON(os.Getenv("UNITY_PLAY_PARITY_MANIFEST"), &baseline); err != nil {
		t.Fatal(err)
	}
	if err := readJSON(os.Getenv("UNITY_PLAY_PARITY_CATALOG"), &native); err != nil {
		t.Fatal(err)
	}
	baseline["projectRoot"] = fixture
	native["project"] = fixture
	for _, v := range array(baseline["sourceDependencies"]) {
		d := object(v)
		if strings.HasPrefix(str(d["path"]), "Temp/") {
			d["sourcePath"] = filepath.Join(project, filepath.FromSlash(str(d["path"])))
		}
	}
	for name, value := range map[string]any{"baseline.json": baseline, "catalog.json": native} {
		data, err := json.Marshal(value)
		if err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(fixture, name), data, 0644); err != nil {
			t.Fatal(err)
		}
	}
	s, err := NewSession(Options{Project: fixture, Manifest: filepath.Join(fixture, "baseline.json"), NativeCatalog: filepath.Join(fixture, "catalog.json")})
	if err != nil {
		t.Fatal(err)
	}
	output := filepath.Join(fixture, "Temp/Output")
	before, err := s.Prepare(output)
	if err != nil {
		t.Fatal(err)
	}
	logical := str(baseline["scenePath"])
	original, err := os.ReadFile(filepath.Join(project, filepath.FromSlash(logical)))
	if err != nil {
		t.Fatal(err)
	}
	changed := strings.Replace(string(original), "m_Name: Main Camera", "m_Name: Go Iteration Camera", 1)
	changed = strings.Replace(changed, "m_LocalPosition: {x: 0, y: 1, z: -10}", "m_LocalPosition: {x: 0, y: 4, z: -10}", 1)
	changed = strings.Replace(changed, "SceneRoots:\n  m_ObjectHideFlags: 0\n  m_Roots:\n", "SceneRoots:\n  m_ObjectHideFlags: 0\n  m_Roots:\n  - {fileID: 900000011}\n", 1)
	changed += `--- !u!1 &900000010
GameObject:
  m_Component:
  - component: {fileID: 900000011}
  m_Name: Go Added GameObject
  m_IsActive: 1
--- !u!4 &900000011
Transform:
  m_GameObject: {fileID: 900000010}
  m_LocalPosition: {x: 3, y: 2, z: 1}
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalScale: {x: 1, y: 1, z: 1}
  m_Father: {fileID: 0}
  m_Children: []
`
	writeFixtureFile(t, filepath.Join(fixture, filepath.FromSlash(logical)), changed)
	after, err := s.Prepare(output)
	if err != nil {
		t.Fatal(err)
	}
	if after.Objects != before.Objects+2 || after.Assets != before.Assets || after.Cache.YAMLParses != 1 {
		t.Fatalf("full scene iteration mismatch: before %+v after %+v", before, after)
	}
	manifest, err := ManifestJSON(after.Manifest)
	if err != nil {
		t.Fatal(err)
	}
	names := map[string]bool{}
	cameraMoved := false
	for _, v := range array(manifest["objects"]) {
		o := object(v)
		var data map[string]any
		jsonDecodeString(str(o["dataJson"]), &data)
		if number(o["classId"]) == 1 {
			names[str(data["m_Name"])] = true
		}
		if str(o["fileId"]) == "330585546" {
			cameraMoved = number(object(data["m_LocalPosition"])["y"]) == 4
		}
	}
	if !names["Go Added GameObject"] || !names["Go Iteration Camera"] || !cameraMoved {
		t.Fatal("actual Scene YAML changes did not reach manifest")
	}
	unchanged, err := os.ReadFile(filepath.Join(project, filepath.FromSlash(logical)))
	if err != nil || string(unchanged) != string(original) {
		t.Fatal("original authored scene changed")
	}
	t.Logf("actual SampleScene rename + Transform + added GameObject: %.3fs, %+v", after.ElapsedSeconds, after.Cache)
}
