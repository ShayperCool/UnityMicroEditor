package main

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

func TestSceneRouteRequiresExactAuthoredPathAndGUID(t *testing.T) {
	guid := "a4c2d5e87cc894d488ffbfd8c52ed16a"
	path := "Assets/Dungeon/ShipGen/Scenes/ShipGenDemo.unity"
	gotGUID, gotPath, err := parseContentScene(guid + "=" + path)
	if err != nil || gotGUID != guid || gotPath != path {
		t.Fatalf("route mismatch %s %s %v", gotGUID, gotPath, err)
	}
	for _, invalid := range []string{path, "GUID=" + path, guid + "=/tmp/scene.unity", guid + "=Assets/../scene.unity", guid + "=Assets/scene.prefab"} {
		if _, _, err := parseContentScene(invalid); err == nil {
			t.Fatalf("invalid authored route accepted %s", invalid)
		}
	}
}

func TestContentRouteWatchReusesSourceCacheAndKeepsGlobalPrefabsAtRoot(t *testing.T) {
	root := t.TempDir()
	guid := "a4c2d5e87cc894d488ffbfd8c52ed16a"
	scene := "Assets/Ship.unity"
	yaml := `%YAML 1.1
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
	write := func(path, data string) {
		t.Helper()
		if err := os.MkdirAll(filepath.Dir(path), 0700); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, []byte(data), 0600); err != nil {
			t.Fatal(err)
		}
	}
	write(filepath.Join(root, scene), yaml)
	write(filepath.Join(root, scene)+".meta", "guid: "+guid+"\n")
	write(filepath.Join(root, "ProjectSettings/ProjectSettings.asset"), workerSettingsFixture)
	write(filepath.Join(root, "ProjectSettings/ProjectVersion.txt"), "m_EditorVersion: "+fixtureUnityVersion+"\n")
	manifest := filepath.Join(root, "baseline.json")
	if err := atomicJSON(manifest, map[string]any{"schemaVersion": 1, "supported": true, "unityVersion": fixtureUnityVersion,
		"projectRoot": root, "scenePath": scene, "sceneName": "Ship", "objects": []any{}, "assets": []any{},
		"assemblies": []string{}, "sourceDependencies": []any{}}); err != nil {
		t.Fatal(err)
	}
	baseline := baselineBuild{Project: root, Manifest: manifest, Player: filepath.Join(root, "Player/Host.x86_64")}
	opts := launchOptions{Project: root, Output: filepath.Join(root, "Temp/cache"), Editor: "pinned-sdk",
		ContentScenes: stringListFlag{guid + "=" + scene},
		// A route must not resolve this root-only runtime-addressed prefab.
		ContentPrefabs: stringListFlag{"Assets/RootOnly.prefab"}}
	cache := make(map[string]*nativeProjectImporter)
	before, err := prepareGoContentRoutes(context.Background(), opts, baseline, nil, nil, nil, cache, nil)
	if err != nil {
		t.Fatal(err)
	}
	firstImporter := cache[guid]
	write(filepath.Join(root, scene), strings.Replace(yaml, "Original", "Changed", 1))
	after, err := prepareGoContentRoutes(context.Background(), opts, baseline, nil, nil, nil, cache, []string{filepath.Join(root, scene)})
	if err != nil {
		t.Fatal(err)
	}
	if cache[guid] != firstImporter {
		t.Fatal("watch discarded parsed YAML and native importer caches")
	}
	if equal, err := contentRoutesRuntimeEqual(before, after); err != nil || equal {
		t.Fatalf("reused source cache missed the authored change: %v", err)
	}
	warm, err := firstImporter.session.Prepare(firstImporter.opts.Output)
	if err != nil || warm.Cache.YAMLParses != 0 || warm.Cache.FileReads != 0 {
		t.Fatalf("content source cache did not remain warm: %+v %v", warm.Cache, err)
	}
	oldBytes, _ := os.ReadFile(before)
	write(filepath.Join(root, "ProjectSettings/ProjectSettings.asset"), strings.Replace(workerSettingsFixture, "m_ActiveColorSpace: 1", "m_ActiveColorSpace: 0", 1))
	if _, err := prepareGoContentRoutes(context.Background(), opts, baseline, nil, nil, nil, cache, nil); err == nil {
		t.Fatal("cached route bypassed native environment invalidation")
	}
	currentBytes, _ := os.ReadFile(before)
	if !reflect.DeepEqual(oldBytes, currentBytes) {
		t.Fatal("rejected cache update mutated the active Player route snapshot")
	}
}

func routeTestManifest(t *testing.T, root, revision string, extra map[string]any) string {
	t.Helper()
	manifest := map[string]any{"projectRoot": root, "sourceScenePath": "Assets/Ship.unity", "revision": revision,
		"objects":            []any{map[string]any{"fileId": "1", "classId": 1, "dataJson": `{"m_Name":"Ship","m_IsActive":1}`}},
		"sourceDependencies": []any{map[string]any{"path": "Assets/Ship.unity", "sha256": revision, "bytes": len(revision)}}}
	for key, value := range extra {
		manifest[key] = value
	}
	directory := filepath.Join(root, revision)
	if err := os.MkdirAll(directory, 0700); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(directory, "scene.json")
	if err := atomicJSON(path, manifest); err != nil {
		t.Fatal(err)
	}
	return path
}

func routeTestSnapshot(t *testing.T, root string, manifests ...string) string {
	t.Helper()
	routes := rawContentRoutes{SchemaVersion: 1, Project: root, Scenes: []rawSceneRoute{}}
	guids := []string{"a4c2d5e87cc894d488ffbfd8c52ed16a", "b4c2d5e87cc894d488ffbfd8c52ed16a"}
	for index, manifest := range manifests {
		hash, err := goFileHash(manifest)
		if err != nil {
			t.Fatal(err)
		}
		routes.Scenes = append(routes.Scenes, rawSceneRoute{GUID: guids[index], Manifest: manifest, SHA256: hash})
	}
	path, err := publishGoContentRoutes(root, routes)
	if err != nil {
		t.Fatal(err)
	}
	return path
}

func TestContentRouteWatchDetectsShipChangesWithUnchangedBoot(t *testing.T) {
	root := t.TempDir()
	before := routeTestManifest(t, root, "before", nil)
	boot := routeTestManifest(t, root, "boot", nil)
	oldRoutes := routeTestSnapshot(t, root, before)
	for _, fixture := range []struct {
		name  string
		extra map[string]any
		equal bool
	}{
		{"yaml-formatting", nil, true},
		{"added-game-object", map[string]any{"objects": []any{map[string]any{"fileId": "1"}, map[string]any{"fileId": "2"}}}, false},
		{"prefab-component-edit", map[string]any{"objects": []any{map[string]any{"fileId": "1", "dataJson": `{"m_Name":"New ship"}`}}}, false},
		{"native-asset-edit", map[string]any{"nativeContentFiles": []any{map[string]any{"sha256": "changed-model"}}}, false},
		{"material-edit", map[string]any{"assets": []any{map[string]any{"kind": "material", "color": "new-color"}}}, false},
	} {
		t.Run(fixture.name, func(t *testing.T) {
			next := routeTestManifest(t, root, fixture.name, fixture.extra)
			newRoutes := routeTestSnapshot(t, root, next)
			unchangedBoot, err := sceneRuntimeEqual(boot, boot)
			if err != nil || !unchangedBoot {
				t.Fatalf("root scene fixture changed: %v", err)
			}
			equal, err := contentRoutesRuntimeEqual(oldRoutes, newRoutes)
			if err != nil || equal != fixture.equal {
				t.Fatalf("content watch equal=%v want=%v: %v", equal, fixture.equal, err)
			}
		})
	}
}

func TestContentRoutesAreImmutableAndValidateManifestHashes(t *testing.T) {
	root := t.TempDir()
	oldManifest := routeTestManifest(t, root, "old", nil)
	newManifest := routeTestManifest(t, root, "new", nil)
	oldPath := routeTestSnapshot(t, root, oldManifest)
	oldBytes, _ := os.ReadFile(oldPath)
	newPath := routeTestSnapshot(t, root, newManifest)
	if oldPath == newPath {
		t.Fatal("a new route revision overwrote the active Player's snapshot")
	}
	currentOldBytes, _ := os.ReadFile(oldPath)
	if !reflect.DeepEqual(oldBytes, currentOldBytes) {
		t.Fatal("publishing new routes mutated the previous snapshot")
	}
	if repeated := routeTestSnapshot(t, root, oldManifest); repeated != oldPath {
		t.Fatal("identical routes did not reuse their immutable snapshot")
	}
	if err := os.WriteFile(newManifest, []byte(`{"objects":[]}`), 0600); err != nil {
		t.Fatal(err)
	}
	if _, err := contentRoutesRuntimeEqual(oldPath, newPath); err == nil {
		t.Fatal("a changed published manifest was silently accepted")
	}
	var routes rawContentRoutes
	if err := json.Unmarshal(oldBytes, &routes); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(oldPath, []byte(`{}`), 0600); err != nil {
		t.Fatal(err)
	}
	if _, err := publishGoContentRoutes(root, routes); err == nil {
		t.Fatal("tampered immutable snapshot was silently overwritten")
	}
}

func TestContentRouteComparisonUsesGUIDNotDeclarationOrder(t *testing.T) {
	root := t.TempDir()
	a := routeTestManifest(t, root, "a", nil)
	b := routeTestManifest(t, root, "b", nil)
	first := routeTestSnapshot(t, root, a, b)
	var routes rawContentRoutes
	if err := readJSONFile(first, &routes); err != nil {
		t.Fatal(err)
	}
	routes.Scenes[0], routes.Scenes[1] = routes.Scenes[1], routes.Scenes[0]
	reordered, err := publishGoContentRoutes(root, routes)
	if err != nil {
		t.Fatal(err)
	}
	if equal, err := contentRoutesRuntimeEqual(first, reordered); err != nil || !equal {
		t.Fatalf("declaration order changed runtime content: %v", err)
	}
	routes.Scenes[0].GUID = "c4c2d5e87cc894d488ffbfd8c52ed16a"
	remapped, err := publishGoContentRoutes(root, routes)
	if err != nil {
		t.Fatal(err)
	}
	if equal, err := contentRoutesRuntimeEqual(first, remapped); err != nil || equal {
		t.Fatalf("changed runtime scene identity was ignored: %v", err)
	}
}

func TestGameArgumentsKeepValuesAndDoNotConsumeRunnerFlags(t *testing.T) {
	args := []string{"--scene", "Assets/Boot.unity", "--", "-host", "-name", "Captain 1", "-relay", "ws://127.0.0.1:8800", "-autostart", "2"}
	runner, game := splitGameArguments(args)
	if !reflect.DeepEqual(runner, args[:2]) || !reflect.DeepEqual(game, args[3:]) {
		t.Fatalf("game arguments changed: %v %v", runner, game)
	}
	if err := validateGameArguments(game); err != nil {
		t.Fatal(err)
	}
	if err := validateGameArguments([]string{"--unity-cli-frames", "1"}); err == nil {
		t.Fatal("game arguments overrode verification protocol")
	}
}
