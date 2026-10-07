package runtimeartifact

import (
	"bytes"
	"encoding/binary"
	"os"
	"path/filepath"
	"testing"
)

func nativeFixtures(t *testing.T) string {
	t.Helper()
	root := os.Getenv("UNITY_WORKER_NATIVE_FIXTURES")
	if root == "" {
		t.Skip("set UNITY_WORKER_NATIVE_FIXTURES to a successfully imported fixture artifacts directory")
	}
	return root
}

func TestAdaptNativePlayerVerifiedFiles(t *testing.T) {
	root := nativeFixtures(t)
	for _, test := range []struct {
		profile, name, expected string
		objects, removed        int
	}{
		{"fbx", "0-0-dec78b13583b5d38d519af0a8c636e82.bin", "18532c338a8adac73985b37c6ef0913ecf8bf6ff22378cb1bea049889b752a06", 7, 2},
		{"wav", "1-0-cf98ce63e0e5085e20444dc09b0590fc.bin", "a929841748d5617347b074475d6d3177d2e216fcffaa8a001dd8f331eb3c6ca4", 1, 1},
	} {
		t.Run(test.profile, func(t *testing.T) {
			input, err := os.ReadFile(filepath.Join(root, test.name))
			if err != nil {
				t.Fatal(err)
			}
			original := bytes.Clone(input)
			output, metadata, err := Adapt(input, test.profile)
			if err != nil {
				t.Fatal(err)
			}
			if !bytes.Equal(input, original) || metadata.OutputSHA256 != test.expected || len(metadata.Objects) != test.objects || len(metadata.Removed) != test.removed {
				t.Fatalf("projection differs from the independently verified Player file: %+v", metadata)
			}
			dataOffset := binary.BigEndian.Uint64(input[32:40])
			if !bytes.Equal(input[dataOffset:], output[dataOffset:]) {
				t.Fatal("runtime object data changed during metadata projection")
			}
			if test.profile == "fbx" && (len(metadata.ExternalReferences) != 1 || metadata.ExternalReferences[0].GUID != "0000000000000000f000000000000000") {
				t.Fatal("built-in external dependency identity/order changed")
			}
			if test.profile == "fbx" && (metadata.ModelGraphUnsupportedReason != "" || len(metadata.ModelGraph) != 6 || metadata.ModelTopologySHA256 != "c87a23c520dd51731c1a5c7fdb26fd647944937e469bcdcaf6a8644b923d83e9") {
				t.Fatalf("native model hierarchy/reference graph differs from binary2text + public Player readback: %+v", metadata.ModelGraph)
			}
			if test.profile == "wav" && (len(metadata.Resources) != 1 || metadata.Resources[0].VirtualPath != "VirtualArtifacts/Primary/71705265666978617564696f70726f62.resource" || metadata.Resources[0].Size != 16512) {
				t.Fatal("native audio resource identity or size changed")
			}
		})
	}
}

func TestModelGraphPreservesAuthoredOrderAndRejectsUnsupportedLayout(t *testing.T) {
	root := nativeFixtures(t)
	input, err := os.ReadFile(filepath.Join(root, "0-0-dec78b13583b5d38d519af0a8c636e82.bin"))
	if err != nil {
		t.Fatal(err)
	}
	_, metadata, err := Adapt(input, "fbx")
	if err != nil {
		t.Fatal(err)
	}
	graph := metadata.ModelGraph
	child := graph[len(graph)-1]
	if child.FileID != "7771091623220844444" || len(child.References[0].Targets) != 3 {
		t.Fatalf("model child's authored component list missing: %+v", child)
	}
	before := HashModelGraph(graph)
	child.References[0].Targets[0], child.References[0].Targets[1] = child.References[0].Targets[1], child.References[0].Targets[0]
	if HashModelGraph(graph) == before {
		t.Fatal("changing authored component order did not invalidate topology fingerprint")
	}
	posed := bytes.Clone(input)
	entryOffset := nativeObjectStart(t, posed, -8679921383154817045)
	binary.LittleEndian.PutUint32(posed[entryOffset+68:entryOffset+72], 0x3f800000)
	_, posedMetadata, err := Adapt(posed, "fbx")
	if err != nil {
		t.Fatal(err)
	}
	if posedMetadata.ModelTopologySHA256 == metadata.ModelTopologySHA256 {
		t.Fatal("FBX Transform pose edit did not invalidate cached model compatibility")
	}
	// Change only a type-tree version stamp while retaining valid tree bounds.
	// Projection can still preserve runtime bytes, but it must not claim the new
	// layout is proven equivalent to the catalog model reference graph.
	unverified := bytes.Clone(input)
	binary.LittleEndian.PutUint16(unverified[122:124], 1)
	_, changed, err := Adapt(unverified, "fbx")
	if err != nil {
		t.Fatal(err)
	}
	if changed.ModelTopologySHA256 != "" || changed.ModelGraphUnsupportedReason == "" {
		t.Fatal("unproven graph layout was accepted for native replacement")
	}
}

func nativeObjectStart(t *testing.T, input []byte, id int64) int {
	t.Helper()
	dataOffset := int(binary.BigEndian.Uint64(input[32:40]))
	needle := make([]byte, 8)
	binary.LittleEndian.PutUint64(needle, uint64(id))
	index := bytes.Index(input[48:dataOffset], needle)
	if index < 0 || 48+index+24 > dataOffset {
		t.Fatal("fixture object table local ID missing")
	}
	return dataOffset + int(binary.LittleEndian.Uint64(input[48+index+8:48+index+16]))
}

func TestNativeAudioRejectsStreamingAndUnverifiedCompression(t *testing.T) {
	root := nativeFixtures(t)
	input, err := os.ReadFile(filepath.Join(root, "1-0-cf98ce63e0e5085e20444dc09b0590fc.bin"))
	if err != nil {
		t.Fatal(err)
	}
	start := nativeObjectStart(t, input, 8300000)
	r := reader{data: input[start:], pos: 40}
	if _, err := r.string(); err != nil {
		t.Fatal(err)
	}
	loadOffset := start + r.pos
	for _, loadType := range []uint32{1, 2, 3} {
		changed := bytes.Clone(input)
		binary.LittleEndian.PutUint32(changed[loadOffset:loadOffset+4], loadType)
		if _, _, err := Adapt(changed, "wav"); err == nil {
			t.Fatalf("unsupported native audio load/compression pairing %d was accepted", loadType)
		}
	}
}

func TestAdaptRealProjectPreloadedVorbis(t *testing.T) {
	root := os.Getenv("UNITY_WORKER_PROJECT_AUDIO_FIXTURES")
	if root == "" {
		t.Skip("set UNITY_WORKER_PROJECT_AUDIO_FIXTURES to native door_crank_loop artifacts")
	}
	input, err := os.ReadFile(filepath.Join(root, "03979594c803f4b0eb50ac92c7a2c1fe-0000.bin"))
	if err != nil {
		t.Fatal(err)
	}
	output, meta, err := Adapt(input, "wav")
	if err != nil {
		t.Fatal(err)
	}
	if len(meta.Objects) != 1 || meta.Audio == nil || meta.Audio.LoadType != 1 || meta.Audio.CompressionFormat != 1 || meta.Audio.Channels != 1 || meta.Audio.Frequency != 44100 || meta.Audio.Length != 1 || len(meta.Resources) != 1 || meta.Resources[0].Size != 9184 {
		t.Fatalf("real project Vorbis identity differs from native importer output: %+v", meta)
	}
	dataOffset := binary.BigEndian.Uint64(input[32:40])
	if !bytes.Equal(input[dataOffset:], output[dataOffset:]) {
		t.Fatal("native compressed AudioClip body changed")
	}
	resource, err := os.ReadFile(filepath.Join(root, "03979594c803f4b0eb50ac92c7a2c1fe-0001.bin"))
	if err != nil {
		t.Fatal(err)
	}
	if len(resource) != 9184 {
		t.Fatal("native project resource size mismatch")
	}
}

func TestMalformedNativeMetadataIsRejected(t *testing.T) {
	root := nativeFixtures(t)
	input, err := os.ReadFile(filepath.Join(root, "0-0-dec78b13583b5d38d519af0a8c636e82.bin"))
	if err != nil {
		t.Fatal(err)
	}
	for _, test := range []struct {
		name string
		edit func([]byte)
	}{
		{"wrong-target", func(data []byte) { binary.LittleEndian.PutUint32(data[54:58], 24) }},
		{"wrong-version", func(data []byte) { binary.BigEndian.PutUint32(data[8:12], 26) }},
		{"wrong-file-size", func(data []byte) { binary.BigEndian.PutUint64(data[24:32], 1) }},
		{"external-type-tree", func(data []byte) { data[58] = 0 }},
		{"unknown-native-class", func(data []byte) { binary.LittleEndian.PutUint32(data[63:67], 28) }},
		{"script-backed-type", func(data []byte) { binary.LittleEndian.PutUint16(data[68:70], 0) }},
	} {
		t.Run(test.name, func(t *testing.T) {
			malformed := bytes.Clone(input)
			test.edit(malformed)
			if _, _, err := Adapt(malformed, "fbx"); err == nil {
				t.Fatal("unsupported/corrupted native format was accepted")
			}
		})
	}
}

func TestRejectUnsupportedProfileAndTruncation(t *testing.T) {
	for _, profile := range []string{"", "shader", "fbx", "wav"} {
		if _, _, err := Adapt(make([]byte, 32), profile); err == nil {
			t.Fatal("truncated or unsupported content was accepted")
		}
	}
}
