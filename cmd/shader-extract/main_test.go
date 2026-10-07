package main

import (
	"bytes"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"

	goproject "unitymicroeditor/internal/project"
)

func TestGuardedShaderExtractionActualArchiveAndImmutableOutput(t *testing.T) {
	input, bundle := os.Getenv("UNITY_PLAY_COMPILED_SHADER_FIXTURE"), os.Getenv("UNITY_PLAY_COMPILED_SHADER_BUNDLE")
	if input == "" || bundle == "" {
		t.Skip("set compiled Shader member and bundle fixture paths")
	}
	data, err := os.ReadFile(input)
	if err != nil {
		t.Fatal(err)
	}
	archive, err := os.ReadFile(bundle)
	if err != nil {
		t.Fatal(err)
	}
	project := t.TempDir()
	source := "Assets/Screen.shader"
	shader := filepath.Join(project, source)
	if err := os.MkdirAll(filepath.Dir(shader), 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(shader, []byte("Shader \"Dungeon/Screen\" {\n#include \"Optional.hlsl\"\n}\n"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(shader+".meta", []byte("fileFormatVersion: 2\nguid: 2cbfcd0349c944ee788f6e2919b9d66c\nShaderImporter:\n"), 0600); err != nil {
		t.Fatal(err)
	}
	origin := filepath.Join(project, "origin.json")
	originValue := []any{map[string]any{"source": bundle, "sha256": fileDigest(archive), "nodes": []any{map[string]any{"name": "CAB-c7e0997e8a48cbbb3c94d6b2c3d64de1.sharedAssets", "size": len(data), "path": input, "sha256": fileDigest(data)}}}}
	encoded, _ := json.Marshal(originValue)
	if err := os.WriteFile(origin, encoded, 0600); err != nil {
		t.Fatal(err)
	}
	output := filepath.Join(project, "Temp/Shader/runtime.bin")
	args := []string{"--input", input, "--input-sha256", fileDigest(data), "--guid", "2cbfcd0349c944ee788f6e2919b9d66c", "--shader-name", "Dungeon/Screen", "--output", output, "--project", project, "--source", source, "--origin", origin, "--environment-sha256", strings.Repeat("a", 64)}
	var stdout bytes.Buffer
	if err := run(args, &stdout); err != nil {
		t.Fatal(err)
	}
	replacementBytes, err := os.ReadFile(output + ".replacement.json")
	if err != nil {
		t.Fatal(err)
	}
	var replacement goproject.NativeReplacement
	if err := json.Unmarshal(replacementBytes, &replacement); err != nil {
		t.Fatal(err)
	}
	if replacement.CompiledShaderSnapshot == nil || replacement.CompiledShaderSnapshot.CompiledSourceRevisionVerified || replacement.EnvironmentSHA256 != strings.Repeat("a", 64) || replacement.Content.Objects[0].FileID != "4800000" {
		t.Fatal("snapshot identity or provenance claim was lost")
	}
	if err := goproject.ValidateShaderSourceRequirements(project, source, replacement.CompiledShaderSnapshot.SourceRequirements, replacement.CompiledShaderSnapshot.MissingIncludes); err != nil {
		t.Fatal(err)
	}
	before, err := os.ReadFile(output)
	if err != nil {
		t.Fatal(err)
	}
	if err := run(args, &stdout); err == nil {
		t.Fatal("replaced an immutable Shader revision")
	}
	after, err := os.ReadFile(output)
	if err != nil || !bytes.Equal(before, after) {
		t.Fatal("rejected extraction changed the existing runtime file")
	}
	if err := os.WriteFile(filepath.Join(project, "Assets/Optional.hlsl"), []byte("changed"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := goproject.ValidateShaderSourceRequirements(project, source, replacement.CompiledShaderSnapshot.SourceRequirements, replacement.CompiledShaderSnapshot.MissingIncludes); err == nil {
		t.Fatal("guard accepted a later optional include")
	}
}

func TestShaderExtractionRejectsIncompleteProfile(t *testing.T) {
	for _, args := range [][]string{nil, {"--input", "relative"}, {"--input", "/input", "--output", "/outside/runtime.bin", "--input-sha256", strings.Repeat("a", 64)}, {"--input", "/input", "--output", "/Temp/runtime.bin", "--input-sha256", strings.Repeat("a", 64), "--project", "/project"}} {
		if err := run(args, &bytes.Buffer{}); err == nil {
			t.Fatal("incomplete Shader extraction profile accepted")
		}
	}
}
