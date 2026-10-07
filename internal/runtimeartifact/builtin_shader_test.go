package runtimeartifact

import (
	"bytes"
	"os"
	"testing"
)

func TestBuiltinShaderNativeIdentityAndCompiledPayload(t *testing.T) {
	memberPath, sourcePath := os.Getenv("UNITY_PLAY_BUILTIN_SHADER_FIXTURE"), os.Getenv("UNITY_PLAY_BUILTIN_SHADER_SOURCE")
	if memberPath == "" || sourcePath == "" {
		t.Skip("set real native builtin compiled-member and engine-resource fixture paths; no Unity process is used")
	}
	member, err := os.ReadFile(memberPath)
	if err != nil {
		t.Fatal(err)
	}
	original := bytes.Clone(member)
	projected, proof, err := ExtractBuiltinPlayerShader(member, 108)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(original, member) || proof.BuiltinGUID != "0000000000000000f000000000000000" || proof.BuiltinFileID != "108" || proof.ArchiveFileID != "2756170372350126523" || proof.Name != "Skybox/Panoramic" || proof.PayloadSHA256 != "8fe06b73b38fb9ceaebc86e5a9a83661ba7bbe20ca0dc88f663d9c20758b7ee1" {
		t.Fatalf("native Panoramic payload/identity differed from independent binary2text readback: %+v", proof)
	}
	again, reread, err := ExtractBuiltinPlayerShader(projected, 108)
	if err != nil || !bytes.Equal(projected, again) || reread.ArchiveFileID != "108" || reread.PayloadSHA256 != proof.PayloadSHA256 {
		t.Fatalf("built-in projection changed compiled payload: %v %+v", err, reread)
	}
	for _, id := range []int64{0, 101, 4800000} {
		if _, _, err := ExtractBuiltinPlayerShader(member, id); err == nil {
			t.Fatal("an unverified built-in alias was accepted")
		}
	}
	file, err := os.Open(sourcePath)
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	info, err := file.Stat()
	if err != nil {
		t.Fatal(err)
	}
	identity, err := InspectBuiltinShader(file, info.Size(), 108)
	if err != nil || identity.Name != proof.Name || identity.FileID != 108 || identity.PayloadSHA256 != "1873d28b8300bbba76b8e69975db0d2afac29dfed413a129cd758e05ba015d0a" {
		t.Fatalf("native engine108 identity was not proved: %+v %v", identity, err)
	}
}

func TestBuiltinShaderInspectorRejectsInvalidHeaders(t *testing.T) {
	for _, data := range [][]byte{nil, make([]byte, 48), make([]byte, 80)} {
		if _, err := InspectBuiltinShader(bytes.NewReader(data), int64(len(data)), 108); err == nil {
			t.Fatal("invalid native built-in resource accepted")
		}
	}
}
