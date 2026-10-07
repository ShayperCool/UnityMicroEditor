package runtimeartifact

import (
	"bytes"
	"encoding/json"
	"os"
	"testing"
)

func TestBuiltinLightingSDKIdentity(t *testing.T) {
	path := os.Getenv("UNITY_BUILTIN_LIGHTING_RESOURCE")
	if path == "" {
		t.Skip("requires explicitly supplied pinned native resource")
	}
	file, err := os.Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	info, err := file.Stat()
	if err != nil {
		t.Fatal(err)
	}
	proof, err := InspectBuiltinLighting(file, info.Size())
	data, _ := json.Marshal(proof)
	t.Log(string(data))
	if err != nil {
		t.Fatal(err)
	}
	if proof.FileID != 20201 || proof.ClassID != 1120 || proof.LightmapCount != 0 || proof.LightProbeCount != 0 {
		t.Fatal("native lighting identity/profile was not retained")
	}
}

func TestBuiltinLightingRejectsMalformedAndUnverifiedNativeFiles(t *testing.T) {
	for _, data := range [][]byte{nil, make([]byte, 64), []byte("same named object does not prove native lighting content")} {
		if _, err := InspectBuiltinLighting(bytes.NewReader(data), int64(len(data))); err == nil {
			t.Fatal("malformed builtin lighting source was accepted")
		}
	}
}
