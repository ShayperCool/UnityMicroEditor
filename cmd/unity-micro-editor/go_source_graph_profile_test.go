package main

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"unitymicroeditor/internal/project/packages"
)

func TestGoSourceProfileRejectsNativeBodyDriftAndWrongPublicBackend(t *testing.T) {
	project := t.TempDir()
	writeGoGraphFile(t, project, "Assets/Keep.txt", "fixture")
	writeGoGraphFile(t, project, "ProjectSettings/ProjectVersion.txt", "m_EditorVersion: "+fixtureUnityVersion+"\n")
	writeGoGraphFile(t, project, "Packages/manifest.json", `{"dependencies":{"com.test.native":"1.0.0"}}`)
	writeGoGraphFile(t, project, "Packages/com.test.native/package.json", `{"name":"com.test.native","version":"1.0.0"}`)
	source := writeGoGraphFile(t, project, "Packages/com.test.native/Native.cs", "public class Native { public static int Read() => 1; }")
	meta := writeGoGraphFile(t, project, "Packages/com.test.native/Native.cs.meta", "guid: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n")
	editor := writeGoGraphFile(t, project, "Temp/sdk/Unity", "SDK fixture, never executed")
	player := writeGoGraphFile(t, project, "Temp/player/Host.x86_64", "native fixture, never executed")
	managed := goSourceManagedDirectory(player)
	dll := writeGoGraphFile(t, project, "Temp/player/Host_Data/Managed/Native.dll", "native DLL fixture")
	writeGoGraphFile(t, project, "Temp/player/Host_Data/ScriptingAssemblies.json", `{"names":["Native.dll"],"types":[16]}`)
	bcl := writeGoGraphFile(t, project, "Temp/sdk/Data/NetStandard/ref/2.1.0/netstandard.dll", "BCL fixture")
	engine := writeGoGraphFile(t, project, "Temp/sdk/Data/PlaybackEngines/LinuxStandaloneSupport/Variations/mono/Managed/UnityEngine.dll", "engine fixture")
	config := goAssemblyConfig{Name: "Native", Sources: []string{source}, Defines: []string{"ENABLE_MONO", "UNITY_STANDALONE_LINUX"}}
	graph := goCompilerGraphExport{SchemaVersion: 1, Backend: "unity_editor", Development: true, UnityVersion: fixtureUnityVersion, Target: "StandaloneLinux64", ScriptingBackend: "Mono2x", Project: project, Unity: editor,
		HostAssemblies: []goCompilerHostAssembly{{Name: "Native", Path: dll, Sources: config.Sources, Defines: config.Defines, Configuration: &config, CompiledReferences: []string{bcl, engine}}}}
	graphPath := filepath.Join(project, "Temp/graph.json")
	if err := atomicJSON(graphPath, graph); err != nil {
		t.Fatal(err)
	}
	logical := "Packages/com.test.native/Native.cs"
	compiled := config
	compiled.Sources = []string{logical}
	sourceHash, _ := goFileHash(source)
	metaHash, _ := goFileHash(meta)
	dllHash, _ := goFileHash(dll)
	bclHash, _ := goFileHash(bcl)
	engineHash, _ := goFileHash(engine)
	proofPath := filepath.Join(filepath.Dir(managed), "UnityMicroEditorSourceProfile.json")
	proof := map[string]any{"schemaVersion": 1, "backend": "unity_editor_build_sources", "unityVersion": fixtureUnityVersion, "target": "StandaloneLinux64", "scriptingBackend": "Mono2x", "development": true,
		"assemblies": map[string]any{"Native": map[string]any{"name": "Native", "dllSha256": dllHash, "configuration": compiled, "sources": map[string]string{logical: sourceHash, logical + ".meta": metaHash}, "references": map[string]string{bcl: bclHash, engine: engineHash}}}}
	if err := atomicJSON(proofPath, proof); err != nil {
		t.Fatal(err)
	}
	snapshot, err := packages.Resolve(context.Background(), project, "", packages.Options{Offline: true})
	if err != nil {
		t.Fatal(err)
	}
	snapshot.Editor = editor
	output := filepath.Join(project, "Temp/profiles/valid.json")
	if _, err := captureGoSourceEngineProfile(context.Background(), project, editor, player, graphPath, output, snapshot); err != nil {
		t.Fatal(err)
	}
	if _, err := loadGoSourceEngineProfile(context.Background(), output, editor, player); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(source, []byte("public class Native { public static int Read() => 2; }"), 0600); err != nil {
		t.Fatal(err)
	}
	snapshot, err = packages.Resolve(context.Background(), project, "", packages.Options{Offline: true})
	if err != nil {
		t.Fatal(err)
	}
	snapshot.Editor = editor
	failed := filepath.Join(project, "Temp/profiles/drift.json")
	if _, err := captureGoSourceEngineProfile(context.Background(), project, editor, player, graphPath, failed, snapshot); err == nil || !strings.Contains(err.Error(), "source-body proof differs") {
		t.Fatalf("same type/name changed native method body accepted: %v", err)
	}
	if _, err := os.Stat(failed); !os.IsNotExist(err) {
		t.Fatal("rejected native body published profile")
	}
	graph.Backend = "go_authored_source"
	if err := atomicJSON(graphPath, graph); err != nil {
		t.Fatal(err)
	}
	if _, err := captureGoSourceEngineProfile(context.Background(), project, editor, player, graphPath, failed, snapshot); err == nil || !strings.Contains(err.Error(), "explicit public Development") {
		t.Fatalf("own graph relabeled as public profile: %v", err)
	}
}
