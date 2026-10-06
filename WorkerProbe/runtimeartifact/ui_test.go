package runtimeartifact

import (
	"bytes"
	"encoding/binary"
	"os"
	"path/filepath"
	"testing"
)

func TestAdaptUIRetainsRealImporterPayloadAndClassIdentity(t *testing.T) {
	root := os.Getenv("UNITY_WORKER_UI_FIXTURES")
	if root == "" {
		t.Skip("set UNITY_WORKER_UI_FIXTURES to exact native UI artifact fixtures")
	}
	for _, fixture := range []struct {
		profile, file string
		objects       int
	}{{"uss", "b54a76024e0db49348ae426002a8a519.bin", 1}, {"uxml", "87785bfd71a0a425fb905aaf7743f114.bin", 2}} {
		t.Run(fixture.profile, func(t *testing.T) {
			input, err := os.ReadFile(filepath.Join(root, fixture.file))
			if err != nil {
				t.Fatal(err)
			}
			original := bytes.Clone(input)
			output, proof, err := AdaptUI(input, fixture.profile)
			if err != nil {
				t.Fatal(err)
			}
			if len(proof.Objects) != fixture.objects || len(proof.Removed) != 1 || proof.Removed[0].ClassID != 2089858483 || !bytes.Equal(input, original) {
				t.Fatalf("native UI identities changed: %+v", proof)
			}
			offset := binary.BigEndian.Uint64(input[32:40])
			if !bytes.Equal(input[offset:], output[offset:]) {
				t.Fatal("native UI object payload changed")
			}
			if len(proof.ExternalReferences) == 0 || proof.ExternalReferences[0].GUID != "0000000000000000e000000000000000" {
				t.Fatal("builtin script dependency was lost")
			}
			id := int64(7433441132597879392)
			if fixture.profile == "uxml" {
				id = 1068529831357515114
			}
			start := nativeObjectStart(t, input, id)
			bad := bytes.Clone(input)
			binary.LittleEndian.PutUint64(bad[start+64:start+72], 12385)
			if _, _, err := AdaptUI(bad, fixture.profile); err == nil {
				t.Fatal("Editor importer script was accepted as runtime UI")
			}
		})
	}
}

func TestAdaptUIRejectsMalformedHeadersAndUnsupportedProfiles(t *testing.T) {
	for _, profile := range []string{"", "fbx", "uss", "uxml", "ttf", "otf"} {
		if _, _, err := AdaptUI(make([]byte, 64), profile); err == nil {
			t.Fatal("invalid UI file was accepted")
		}
	}
}
