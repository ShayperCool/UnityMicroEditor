package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	goproject "unitymicroeditor/project"
)

func TestNativeMeshCPUCacheUpgradeActualIndex(t *testing.T) {
	index := os.Getenv("UNITY_NATIVE_MESH_CPU_INDEX")
	if index == "" {
		t.Skip("set UNITY_NATIVE_MESH_CPU_INDEX for pure-Go upgrade/verification of real native model cache")
	}
	projectSource := os.Getenv("UNITY_MICRO_EDITOR_TEST_PROJECT")
	if projectSource == "" {
		t.Skip("set UNITY_MICRO_EDITOR_TEST_PROJECT to the owner of the native model cache")
	}
	project, err := filepath.Abs(projectSource)
	if err != nil {
		t.Fatal(err)
	}
	var entries []goproject.NativeReplacement
	if err := readJSONFile(index, &entries); err != nil {
		t.Fatal(err)
	}
	output := filepath.Join(project, "Temp/UnityPlay/mesh-cpu-proof", randomID())
	importer := &nativeProjectImporter{opts: launchOptions{Project: project, Output: output}}
	started := time.Now()
	models, meshes := 0, 0
	var first goproject.NativeReplacement
	for _, entry := range entries {
		if !strings.EqualFold(filepath.Ext(entry.SourcePath), ".fbx") {
			continue
		}
		next, err := importer.retainNativeModelMeshCPU(entry)
		if err != nil {
			t.Fatalf("%s: %v", entry.SourcePath, err)
		}
		if next.GUID != entry.GUID || next.SourceSHA256 != entry.SourceSHA256 || next.MetaSHA256 != entry.MetaSHA256 || next.EnvironmentSHA256 != entry.EnvironmentSHA256 || next.ModelGraphSHA256 != entry.ModelGraphSHA256 {
			t.Fatalf("source/native import/model graph identity changed while retaining CPU data: %s", entry.SourcePath)
		}
		verified, err := importer.retainNativeModelMeshCPU(next)
		if err != nil || verified.Content.Path != next.Content.Path || verified.Content.SHA256 != next.Content.SHA256 {
			t.Fatalf("exact native geometry projection was not replayable: %s %v", entry.SourcePath, err)
		}
		models++
		meshes += len(next.Content.MeshCPURetention)
		if first.GUID == "" && next.MeshCPUProjection != nil {
			first = next
		}
	}
	if models == 0 || meshes == 0 {
		t.Fatal("actual index test found no retained native Mesh geometry")
	}
	// A valid SHA over a modified output must still fail against the original
	// imported bytes; claimed descriptor hashes alone do not prove fidelity.
	if first.Content.Path != "" {
		data, err := os.ReadFile(first.Content.Path)
		if err != nil {
			t.Fatal(err)
		}
		data[len(data)-1] ^= 1
		tampered := filepath.Join(output, "tampered-runtime.bin")
		if err := os.MkdirAll(output, 0700); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(tampered, data, 0600); err != nil {
			t.Fatal(err)
		}
		first.Content.Path, first.Content.SHA256 = tampered, bytesSHA(data)
		if _, err := importer.retainNativeModelMeshCPU(first); err == nil {
			t.Fatal("modified geometry passed retention cache proof after updating descriptor SHA")
		}
	}
	data, _ := json.Marshal(map[string]any{"models": models, "meshes": meshes, "elapsedSeconds": time.Since(started).Seconds(), "mainEditorStarted": false, "workerStarted": false, "output": output})
	if err := os.MkdirAll(output, 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(output, "result.json"), data, 0600); err != nil {
		t.Fatal(err)
	}
	t.Log(string(data))
}

func TestNativeMeshCPUFilesRejectUnownedSymlinkAndImmutableConflicts(t *testing.T) {
	project := t.TempDir()
	directory := filepath.Join(project, "Temp/derived")
	if err := os.MkdirAll(directory, 0700); err != nil {
		t.Fatal(err)
	}
	filename := filepath.Join(directory, "mesh.bin")
	if err := writeImmutableNativeMeshFile(filename, []byte("immutable payload")); err != nil {
		t.Fatal(err)
	}
	if err := writeImmutableNativeMeshFile(filename, []byte("immutable payload")); err != nil {
		t.Fatal(err)
	}
	if err := writeImmutableNativeMeshFile(filename, []byte("changed geometry")); err == nil {
		t.Fatal("conflicting immutable geometry payload was overwritten")
	}
	if _, err := readOwnedNativeMeshFile(project, filename); err != nil {
		t.Fatal(err)
	}
	unowned := filepath.Join(project, "Assets/mesh.bin")
	if _, err := readOwnedNativeMeshFile(project, unowned); err == nil {
		t.Fatal("project asset path was accepted as mutable runtime projection target")
	}
	link := filepath.Join(directory, "linked.bin")
	if err := os.Symlink(filename, link); err != nil {
		t.Fatal(err)
	}
	if _, err := readOwnedNativeMeshFile(project, link); err == nil {
		t.Fatal("symlinked native geometry input was accepted")
	}
	if err := writeImmutableNativeMeshFile(link, []byte("immutable payload")); err == nil {
		t.Fatal("immutable native output symlink was followed")
	}
}
