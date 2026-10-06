package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"

	goproject "unitymicroeditor/project"
)

func TestBuiltinShaderSnapshotRejectsNativePayloadSubstitution(t *testing.T) {
	path := os.Getenv("UNITY_PLAY_BUILTIN_SHADER_SNAPSHOT")
	if path == "" {
		t.Skip("set a real built-in Shader snapshot; SDK/native files are only read")
	}
	var good goproject.RetainedBuiltinShader
	if err := readJSONFile(path, &good); err != nil {
		t.Fatal(err)
	}
	if err := verifyBuiltinShaderSnapshot(good); err != nil {
		t.Fatal(err)
	}
	for _, corrupt := range []func(*goproject.RetainedBuiltinShader){
		func(v *goproject.RetainedBuiltinShader) { v.ArchiveFileID = "1" },
		func(v *goproject.RetainedBuiltinShader) { v.PayloadSHA256 = strings.Repeat("0", 64) },
		func(v *goproject.RetainedBuiltinShader) { v.MemberName = "other-member" },
		func(v *goproject.RetainedBuiltinShader) { v.BuiltinSourceSHA256 = strings.Repeat("0", 64) },
	} {
		value := good
		corrupt(&value)
		if err := verifyBuiltinShaderSnapshot(value); err == nil {
			t.Fatal("substituted built-in Shader provenance accepted")
		}
	}
	bytes, err := os.ReadFile(good.Content.Path)
	if err != nil {
		t.Fatal(err)
	}
	bytes[len(bytes)-1] ^= 1
	changed := filepath.Join(t.TempDir(), "changed.bin")
	if err := os.WriteFile(changed, bytes, 0600); err != nil {
		t.Fatal(err)
	}
	value := good
	value.Content.Path = changed
	value.Content.SHA256 = bytesSHA(bytes)
	if err := verifyBuiltinShaderSnapshot(value); err == nil {
		t.Fatal("changed compiled GPU payload with an updated descriptor hash was accepted")
	}
}
