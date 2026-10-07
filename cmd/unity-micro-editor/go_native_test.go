package main

import (
	"bytes"
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"

	goproject "unitymicroeditor/internal/project"
)

const goNativeTestGUID = "71705265666978617564696f70726f62"

func writeGoNativeManifest(t *testing.T, root string, files []goproject.NativeContentFile) string {
	t.Helper()
	data, err := json.Marshal(struct {
		Files []goproject.NativeContentFile `json:"nativeContentFiles"`
	}{files})
	if err != nil {
		t.Fatal(err)
	}
	filename := filepath.Join(root, "scene.json")
	if err := os.WriteFile(filename, data, 0600); err != nil {
		t.Fatal(err)
	}
	return filename
}

func writeGoNativeResource(t *testing.T, root, name string, contents []byte) goproject.NativeContentResource {
	t.Helper()
	filename := filepath.Join(root, name)
	if err := os.WriteFile(filename, contents, 0600); err != nil {
		t.Fatal(err)
	}
	return goproject.NativeContentResource{Path: filename,
		VirtualPath: "VirtualArtifacts/Primary/" + goNativeTestGUID + ".resource", SHA256: bytesSHA(contents)}
}

func requireGoNativeFileBytes(t *testing.T, filename string, expected []byte) {
	t.Helper()
	contents, err := os.ReadFile(filename)
	if err != nil || !bytes.Equal(contents, expected) {
		t.Fatalf("resource %s differs: %v, got %x want %x", filename, err, contents, expected)
	}
}

func TestGoNativeResourceMountUsesExactRuntimeAliasAndReplacesOwnFile(t *testing.T) {
	root := t.TempDir()
	run := filepath.Join(root, "run")
	first := []byte{0, 1, 0, 2, 0xff, 0x7f}
	resource := writeGoNativeResource(t, root, "native.resource", first)
	file := goproject.NativeContentFile{GUID: goNativeTestGUID, Resources: []goproject.NativeContentResource{resource}}
	manifest := writeGoNativeManifest(t, root, []goproject.NativeContentFile{file})
	if err := stageNativeResources(manifest, run); err != nil {
		t.Fatal(err)
	}
	destination := filepath.Join(run, filepath.FromSlash(resource.VirtualPath))
	requireGoNativeFileBytes(t, destination, first)
	requireGoNativeFileBytes(t, resource.Path, first)
	info, err := os.Stat(destination)
	if err != nil || !info.Mode().IsRegular() || info.Mode().Perm()&0077 != 0 {
		t.Fatalf("mount is not a private regular file: %v %v", info, err)
	}
	second := []byte{0x52, 0x49, 0x46, 0x46, 0, 0, 0, 0}
	resource = writeGoNativeResource(t, root, "native-next.resource", second)
	file.Resources = []goproject.NativeContentResource{resource}
	manifest = writeGoNativeManifest(t, root, []goproject.NativeContentFile{file})
	if err := stageNativeResources(manifest, run); err != nil {
		t.Fatal(err)
	}
	requireGoNativeFileBytes(t, destination, second)
	requireGoNativeFileBytes(t, filepath.Join(root, "native.resource"), first)
	entries, err := os.ReadDir(filepath.Dir(destination))
	if err != nil || len(entries) != 1 || entries[0].Name() != goNativeTestGUID+".resource" {
		t.Fatalf("mount left temporary files or used another alias: %+v %v", entries, err)
	}
}

func TestGoNativeResourceHashMismatchKeepsPreviousMount(t *testing.T) {
	root := t.TempDir()
	run := filepath.Join(root, "run")
	original := []byte("validated immutable streaming audio bytes")
	resource := writeGoNativeResource(t, root, "native.resource", original)
	manifest := writeGoNativeManifest(t, root, []goproject.NativeContentFile{{GUID: goNativeTestGUID,
		Resources: []goproject.NativeContentResource{resource}}})
	if err := stageNativeResources(manifest, run); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(resource.Path, []byte("artifact corrupted after validation"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := stageNativeResources(manifest, run); err == nil {
		t.Fatal("mounted resource whose bytes no longer match the published SHA256")
	}
	requireGoNativeFileBytes(t, filepath.Join(run, filepath.FromSlash(resource.VirtualPath)), original)
}

func TestGoNativeResourceRejectsAliasOutsideGUID(t *testing.T) {
	for _, alias := range []string{"other.resource", "VirtualArtifacts/Primary/other.resource", "../escaped.resource",
		"VirtualArtifacts/Primary/" + goNativeTestGUID + ".resource/../escaped.resource"} {
		t.Run(alias, func(t *testing.T) {
			root := t.TempDir()
			resource := writeGoNativeResource(t, root, "native.resource", []byte("native resource"))
			resource.VirtualPath = alias
			manifest := writeGoNativeManifest(t, root, []goproject.NativeContentFile{{GUID: goNativeTestGUID,
				Resources: []goproject.NativeContentResource{resource}}})
			run := filepath.Join(root, "run")
			if err := stageNativeResources(manifest, run); err == nil {
				t.Fatalf("accepted alias %q outside the exact source GUID", alias)
			}
			if _, err := os.Stat(run); !os.IsNotExist(err) {
				t.Fatalf("invalid alias created a runtime mount: %v", err)
			}
		})
	}
}

func TestGoNativeResourceRejectsGUIDTraversal(t *testing.T) {
	root := t.TempDir()
	run := filepath.Join(root, "run")
	resource := writeGoNativeResource(t, root, "native.resource", []byte("native resource"))
	guid := "../../../escaped"
	resource.VirtualPath = "VirtualArtifacts/Primary/" + guid + ".resource"
	manifest := writeGoNativeManifest(t, root, []goproject.NativeContentFile{{GUID: guid,
		Resources: []goproject.NativeContentResource{resource}}})
	if err := stageNativeResources(manifest, run); err == nil {
		t.Fatal("accepted a path traversal GUID which passes naive alias equality")
	}
	if _, err := os.Stat(filepath.Join(root, "escaped.resource")); !os.IsNotExist(err) {
		t.Fatalf("traversal GUID wrote outside the Player runtime directory: %v", err)
	}
}

func TestGoNativeResourceRejectsSymlinkMountParents(t *testing.T) {
	for _, parent := range []string{"run", "VirtualArtifacts", "Primary"} {
		t.Run(parent, func(t *testing.T) {
			root := t.TempDir()
			run := filepath.Join(root, "run")
			outside := filepath.Join(root, "outside")
			if err := os.Mkdir(outside, 0700); err != nil {
				t.Fatal(err)
			}
			link := run
			if parent == "VirtualArtifacts" {
				link = filepath.Join(run, parent)
			} else if parent == "Primary" {
				link = filepath.Join(run, "VirtualArtifacts", parent)
			}
			if err := os.MkdirAll(filepath.Dir(link), 0700); err != nil {
				t.Fatal(err)
			}
			if err := os.Symlink(outside, link); err != nil {
				t.Fatal(err)
			}
			resource := writeGoNativeResource(t, root, "native.resource", []byte("native resource"))
			manifest := writeGoNativeManifest(t, root, []goproject.NativeContentFile{{GUID: goNativeTestGUID,
				Resources: []goproject.NativeContentResource{resource}}})
			if err := stageNativeResources(manifest, run); err == nil {
				t.Fatal("mount followed an existing symlink parent outside the owned run directory")
			}
			entries, err := os.ReadDir(outside)
			if err != nil || len(entries) != 0 {
				t.Fatalf("mount wrote through symlink into another directory: %+v %v", entries, err)
			}
		})
	}
}

func TestGoNativeResourceRejectsConflictingSharedAliasBeforeMounting(t *testing.T) {
	root := t.TempDir()
	run := filepath.Join(root, "run")
	first := writeGoNativeResource(t, root, "first.resource", []byte("first native descriptor bytes"))
	second := writeGoNativeResource(t, root, "second.resource", []byte("conflicting native descriptor bytes"))
	manifest := writeGoNativeManifest(t, root, []goproject.NativeContentFile{
		{GUID: goNativeTestGUID, Resources: []goproject.NativeContentResource{first}},
		{GUID: goNativeTestGUID, Resources: []goproject.NativeContentResource{second}},
	})
	if err := stageNativeResources(manifest, run); err == nil {
		t.Fatal("silently replaced one native descriptor's resource with another descriptor's bytes")
	}
	if _, err := os.Stat(filepath.Join(run, filepath.FromSlash(first.VirtualPath))); !os.IsNotExist(err) {
		t.Fatalf("conflicting resource descriptors partially changed live mount: %v", err)
	}
}

// This exercises cache replay through the real Session validator. Its runtime
// files are immutable byte fixtures; no native importer or Player is invoked.
func goNativeReplayFixture(t *testing.T) (launchOptions, *goproject.Session, []goproject.NativeReplacement) {
	t.Helper()
	project := t.TempDir()
	output := filepath.Join(project, "Temp", "native-replay")
	if err := os.MkdirAll(filepath.Join(project, "Assets"), 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.MkdirAll(filepath.Join(output, "native"), 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.MkdirAll(filepath.Join(project, "ProjectSettings"), 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(project, "ProjectSettings/ProjectSettings.asset"), []byte(workerSettingsFixture), 0600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(project, "ProjectSettings/ProjectVersion.txt"), []byte("m_EditorVersion: "+fixtureUnityVersion+"\n"), 0600); err != nil {
		t.Fatal(err)
	}
	environment, err := nativeEnvironmentFingerprint(project, "")
	if err != nil {
		t.Fatal(err)
	}
	scene := `--- !u!1 &10
GameObject:
  m_Component:
  - component: {fileID: 11}
  m_Name: Cache proof
  m_IsActive: 1
--- !u!4 &11
Transform:
  m_GameObject: {fileID: 10}
  m_LocalPosition: {x: 0, y: 0, z: 0}
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalScale: {x: 1, y: 1, z: 1}
  m_Father: {fileID: 0}
  m_Children: []
`
	if err := os.WriteFile(filepath.Join(project, "Assets/Scene.unity"), []byte(scene), 0600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(project, "Assets/Scene.unity.meta"), []byte("guid: 99999999999999999999999999999999\n"), 0600); err != nil {
		t.Fatal(err)
	}
	var assets, dependencies []any
	var replacements []goproject.NativeReplacement
	for index, guid := range []string{"11111111111111111111111111111111", "22222222222222222222222222222222", "33333333333333333333333333333333"} {
		logical := "Assets/Audio" + string(rune('A'+index)) + ".wav"
		source := []byte("source fixture " + guid)
		meta := []byte("guid: " + guid + "\n")
		if err := os.WriteFile(filepath.Join(project, logical), source, 0600); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(project, logical+".meta"), meta, 0600); err != nil {
			t.Fatal(err)
		}
		runtime := filepath.Join(output, "native", guid+".runtime.bin")
		contents := []byte("adapted fixture " + guid)
		if err := os.WriteFile(runtime, contents, 0600); err != nil {
			t.Fatal(err)
		}
		assets = append(assets, map[string]any{"kind": "imported", "guid": guid, "fileId": "8300000", "typeName": "UnityEngine.AudioClip", "path": logical, "classId": 83})
		dependencies = append(dependencies, map[string]any{"path": logical, "sha256": bytesSHA(source), "bytes": len(source)}, map[string]any{"path": logical + ".meta", "sha256": bytesSHA(meta), "bytes": len(meta)})
		replacements = append(replacements, goproject.NativeReplacement{GUID: guid, SourcePath: logical, SourceSHA256: bytesSHA(source), MetaSHA256: bytesSHA(meta), EnvironmentSHA256: environment, Content: goproject.NativeContentFile{GUID: guid, Path: runtime, SHA256: bytesSHA(contents), Objects: []goproject.NativeContentObject{{FileID: "8300000", TypeName: "UnityEngine.AudioClip"}}}})
	}
	baseline := filepath.Join(output, "baseline.json")
	if err := atomicJSON(baseline, map[string]any{"schemaVersion": 1, "supported": true, "unityVersion": "6000.6.3f1", "projectRoot": project, "scenePath": "Assets/Scene.unity", "objects": []any{}, "assets": assets, "sourceDependencies": dependencies}); err != nil {
		t.Fatal(err)
	}
	session, err := goproject.NewSession(goproject.Options{Project: project, Manifest: baseline})
	if err != nil {
		t.Fatal(err)
	}
	return launchOptions{Project: project, Output: output}, session, replacements
}

func TestGoNativeIndexReplayLoadsDependenciesBeforeGUIDSortedDependent(t *testing.T) {
	opts, session, entries := goNativeReplayFixture(t)
	// Persistent index uses lexical GUID order, which places A before its B/C
	// dependencies. A also lists a native built-in that needs no cached source.
	entries[0].Content.Dependencies = []goproject.NativeContentDependency{{GUID: entries[1].GUID}, {GUID: entries[2].GUID}, {GUID: "0000000000000000f000000000000000", Global: true}}
	entries[1].Content.Dependencies = []goproject.NativeContentDependency{{GUID: entries[2].GUID}}
	index := filepath.Join(opts.Output, "native/index.json")
	if err := atomicJSON(index, entries); err != nil {
		t.Fatal(err)
	}
	original, _ := os.ReadFile(index)
	importer, err := newNativeProjectImporter(opts, session)
	if err != nil {
		t.Fatal(err)
	}
	if len(importer.entries) != 3 {
		t.Fatalf("dependency replay lost entries: %d", len(importer.entries))
	}
	prepared, err := session.Prepare(opts.Output)
	if err != nil {
		t.Fatal(err)
	}
	var published struct {
		Files []goproject.NativeContentFile `json:"nativeContentFiles"`
	}
	if err := readJSONFile(prepared.Manifest, &published); err != nil {
		t.Fatal(err)
	}
	if len(published.Files) != 3 {
		t.Fatalf("validated native overlay wasn't published: %+v", published.Files)
	}
	for i, file := range published.Files {
		if file.GUID != entries[i].GUID || file.SHA256 != entries[i].Content.SHA256 {
			t.Fatalf("replay changed native object identity/hash: %+v", file)
		}
	}
	if !reflect.DeepEqual(published.Files[0].Dependencies, entries[0].Content.Dependencies) {
		t.Fatal("replay reordered the native external dependency table")
	}
	unchanged, _ := os.ReadFile(index)
	if !bytes.Equal(original, unchanged) {
		t.Fatal("cache replay mutated the persisted index")
	}
}

func TestGoNativeIndexReplayRejectsDependencyCyclesAndMissingSourceRevision(t *testing.T) {
	for _, kind := range []string{"cycle", "self cycle", "missing", "stale dependency"} {
		t.Run(kind, func(t *testing.T) {
			opts, session, entries := goNativeReplayFixture(t)
			want := ""
			switch kind {
			case "cycle":
				entries = entries[:2]
				entries[0].Content.Dependencies = []goproject.NativeContentDependency{{GUID: entries[1].GUID}}
				entries[1].Content.Dependencies = []goproject.NativeContentDependency{{GUID: entries[0].GUID}}
				want = "dependency cycle"
			case "self cycle":
				entries = entries[:1]
				entries[0].Content.Dependencies = []goproject.NativeContentDependency{{GUID: entries[0].GUID}}
				want = "dependency cycle"
			case "missing":
				entries = entries[:1]
				entries[0].Content.Dependencies = []goproject.NativeContentDependency{{GUID: "77777777777777777777777777777777"}}
				want = "no current source revision"
			case "stale dependency":
				entries = entries[:2]
				entries[0].Content.Dependencies = []goproject.NativeContentDependency{{GUID: entries[1].GUID}}
				if err := os.WriteFile(filepath.Join(opts.Project, entries[1].SourcePath), []byte("source changed since its cached import"), 0600); err != nil {
					t.Fatal(err)
				}
				want = "no current source revision"
			}
			if err := atomicJSON(filepath.Join(opts.Output, "native/index.json"), entries); err != nil {
				t.Fatal(err)
			}
			importer, err := newNativeProjectImporter(opts, session)
			if err == nil || !strings.Contains(err.Error(), want) || importer != nil {
				t.Fatalf("invalid %s graph was accepted or misdiagnosed: importer=%v err=%v", kind, importer, err)
			}
		})
	}
}

func TestGoNativeIndexReplayInvalidatesLegacyAndChangedEnvironment(t *testing.T) {
	for _, kind := range []string{"legacy", "color space", "shader precision", "graphics API", "different engine path"} {
		t.Run(kind, func(t *testing.T) {
			opts, session, entries := goNativeReplayFixture(t)
			source := workerSettingsFixture
			switch kind {
			case "legacy":
				for index := range entries {
					entries[index].EnvironmentSHA256 = ""
				}
			case "color space":
				source = strings.Replace(source, "m_ActiveColorSpace: 1", "m_ActiveColorSpace: 0", 1)
			case "shader precision":
				source = strings.Replace(source, "shaderPrecisionModel: 0", "shaderPrecisionModel: 1", 1)
			case "graphics API":
				source = strings.Replace(source, "m_BuildTargetGraphicsAPIs: []", "m_BuildTargetGraphicsAPIs:\n  - m_BuildTarget: LinuxStandaloneSupport\n    m_APIs: 15000000i\n    m_Automatic: 0", 1)
			case "different engine path":
				opts.Editor = "/a/different/pinned/Unity"
			}
			if err := os.WriteFile(filepath.Join(opts.Project, "ProjectSettings/ProjectSettings.asset"), []byte(source), 0600); err != nil {
				t.Fatal(err)
			}
			index := filepath.Join(opts.Output, "native/index.json")
			if err := atomicJSON(index, entries); err != nil {
				t.Fatal(err)
			}
			original, _ := os.ReadFile(index)
			importer, err := newNativeProjectImporter(opts, session)
			if err != nil || len(importer.entries) != 0 {
				t.Fatalf("replayed stale %s native content: %+v %v", kind, importer, err)
			}
			unchanged, _ := os.ReadFile(index)
			if !bytes.Equal(original, unchanged) {
				t.Fatal("invalidation must keep previously published immutable cache files")
			}
		})
	}
}

func TestGoNativePrepareRejectsEnvironmentMutationDuringLiveSession(t *testing.T) {
	opts, session, entries := goNativeReplayFixture(t)
	if err := atomicJSON(filepath.Join(opts.Output, "native/index.json"), entries); err != nil {
		t.Fatal(err)
	}
	importer, err := newNativeProjectImporter(opts, session)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := importer.Prepare(context.Background()); err != nil {
		t.Fatal(err)
	}
	changed := strings.Replace(workerSettingsFixture, "m_ActiveColorSpace: 1", "m_ActiveColorSpace: 0", 1)
	if err := os.WriteFile(filepath.Join(opts.Project, "ProjectSettings/ProjectSettings.asset"), []byte(changed), 0600); err != nil {
		t.Fatal(err)
	}
	if _, err := importer.Prepare(context.Background()); err == nil || !strings.Contains(err.Error(), "environment changed") {
		t.Fatalf("published previous imported data after environment mutation: %v", err)
	}
}
