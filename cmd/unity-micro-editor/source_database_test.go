package main

import (
	"encoding/hex"
	"testing"
)

func TestSourceHashesMatchNativeUnityGolden(t *testing.T) {
	// Captured from a closed Unity 6000.6.3f1 oracle SourceAssetDB hash
	// table for these exact deterministic fixture bytes.
	for _, vector := range []struct {
		data []byte
		hash string
	}{
		{fixtureWAV(), "36de4a9c109bdaa391f080c0fd24a265"},
		{fixtureAudioMeta(), "f1df8fafd8fc01412ecb79b3bf5b9377"},
	} {
		hash := nativeSourceHash(vector.data)
		if hex.EncodeToString(hash[:]) != vector.hash {
			t.Fatalf("native Unity digest mismatch: %x, want %s", hash, vector.hash)
		}
	}
}
