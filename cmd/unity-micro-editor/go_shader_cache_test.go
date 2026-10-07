package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"

	goproject "unitymicroeditor/internal/project"
	"unitymicroeditor/internal/runtimeartifact"
)

func TestCompiledShaderCacheReplayChecksActualProjectionAndArchive(t *testing.T) {
	memberPath, bundlePath := os.Getenv("UNITY_PLAY_COMPILED_SHADER_FIXTURE"), os.Getenv("UNITY_PLAY_COMPILED_SHADER_BUNDLE")
	if memberPath == "" || bundlePath == "" {
		t.Skip("set compiled Shader member and bundle fixture paths")
	}
	member, err := os.ReadFile(memberPath)
	if err != nil {
		t.Fatal(err)
	}
	archive, err := os.ReadFile(bundlePath)
	if err != nil {
		t.Fatal(err)
	}
	output, proof, err := runtimeartifact.ExtractPlayerShader(member, "2cbfcd0349c944ee788f6e2919b9d66c", "Dungeon/Screen")
	if err != nil {
		t.Fatal(err)
	}
	directory := t.TempDir()
	archivePath := filepath.Join(directory, "origin.bundle")
	copyMember := filepath.Join(directory, "origin.sharedAssets")
	runtime := filepath.Join(directory, "runtime.bin")
	for path, data := range map[string][]byte{archivePath: archive, copyMember: member, runtime: output} {
		if err := os.WriteFile(path, data, 0600); err != nil {
			t.Fatal(err)
		}
	}
	entry := goproject.NativeReplacement{GUID: proof.GUID, SourcePath: "Assets/Screen.shader",
		Content:                goproject.NativeContentFile{GUID: proof.GUID, Path: runtime, SHA256: proof.OutputSHA256, Objects: []goproject.NativeContentObject{{FileID: "4800000", TypeName: "UnityEngine.Shader"}}},
		CompiledShaderSnapshot: &goproject.CompiledShaderSnapshot{SchemaVersion: 1, SourcePath: "Assets/Screen.shader", ShaderName: proof.Name, ArchiveFileID: proof.ArchiveFileID, BundlePath: archivePath, BundleSHA256: bytesSHA(archive), MemberPath: copyMember, MemberName: "CAB-c7e0997e8a48cbbb3c94d6b2c3d64de1.sharedAssets", MemberSHA256: bytesSHA(member), PayloadSHA256: proof.PayloadSHA256, TypeTreeSHA256: proof.TypeTreeSHA256}}
	if err := verifyCompiledShaderCache(entry); err != nil {
		t.Fatal(err)
	}
	encoded, err := json.Marshal(entry)
	if err != nil {
		t.Fatal(err)
	}
	for _, kind := range []string{"GUID", "name", "archive ID", "payload", "tree", "projection", "member name", "object ID", "type", "extra object", "provenance claim"} {
		t.Run(kind, func(t *testing.T) {
			var changed goproject.NativeReplacement
			if err := json.Unmarshal(encoded, &changed); err != nil {
				t.Fatal(err)
			}
			switch kind {
			case "GUID":
				changed.GUID = strings.Repeat("1", 32)
			case "name":
				changed.CompiledShaderSnapshot.ShaderName = "Other/Shader"
			case "archive ID":
				changed.CompiledShaderSnapshot.ArchiveFileID = "5598"
			case "payload":
				changed.CompiledShaderSnapshot.PayloadSHA256 = strings.Repeat("1", 64)
			case "tree":
				changed.CompiledShaderSnapshot.TypeTreeSHA256 = strings.Repeat("1", 64)
			case "projection":
				changed.Content.SHA256 = strings.Repeat("1", 64)
			case "member name":
				changed.CompiledShaderSnapshot.MemberName = "another.sharedAssets"
			case "object ID":
				changed.Content.Objects[0].FileID = "4800001"
			case "type":
				changed.Content.Objects[0].TypeName = "UnityEngine.Material"
			case "extra object":
				changed.Content.Objects = append(changed.Content.Objects, goproject.NativeContentObject{FileID: "1", TypeName: "UnityEngine.GameObject"})
			case "provenance claim":
				changed.CompiledShaderSnapshot.CompiledSourceRevisionVerified = true
			}
			if err := verifyCompiledShaderCache(changed); err == nil {
				t.Fatal("compiled Shader cache substitution was accepted")
			}
		})
	}
	archive[len(archive)-1] ^= 1
	if err := os.WriteFile(archivePath, archive, 0600); err != nil {
		t.Fatal(err)
	}
	if err := verifyCompiledShaderCache(entry); err == nil {
		t.Fatal("changed archive cache replay was accepted")
	}
}
