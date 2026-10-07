package runtimeartifact

import (
	"bytes"
	"encoding/binary"
	"os"
	"testing"
)

func TestExtractPlayerShaderNativeFixture(t *testing.T) {
	path := os.Getenv("UNITY_PLAY_COMPILED_SHADER_FIXTURE")
	if path == "" {
		t.Skip("set UNITY_PLAY_COMPILED_SHADER_FIXTURE to a genuine Linux compiled Shader member")
	}
	input, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	original := bytes.Clone(input)
	output, proof, err := ExtractPlayerShader(input, "2cbfcd0349c944ee788f6e2919b9d66c", "Dungeon/Screen")
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(input, original) || proof.ArchiveFileID != "5597" || proof.SourceFileID != "4800000" || proof.CompiledBytes == 0 || len(output) >= len(input) {
		t.Fatalf("native Shader identity/payload subset failed: %+v", proof)
	}
	if proof.PayloadSHA256 != "3fcc950eceb2762a3ec8c6d3ccb1f53e8180a8aa076dfa4c51a8e0999d14ab74" || proof.TypeTreeSHA256 != "041abf49270e21e444c574233a40c65719797895d36e215f177eb361011d6dd0" {
		t.Fatal("native Shader differs from the independent binary2text fixture")
	}
	// Re-reading the independent output proves that it contains only the selected
	// Shader and that removing every original external table kept it self-contained.
	again, reread, err := ExtractPlayerShader(output, proof.GUID, proof.Name)
	if err != nil || reread.ArchiveFileID != "4800000" || reread.PayloadSHA256 != proof.PayloadSHA256 || !bytes.Equal(again, output) {
		t.Fatalf("Shader-only projection is not stable: %+v, %v", reread, err)
	}
	for _, invalid := range []struct{ guid, name string }{{"11111111111111111111111111111111", proof.Name}, {proof.GUID, "Dungeon/Wrong"}} {
		if _, _, err := ExtractPlayerShader(input, invalid.guid, invalid.name); err == nil {
			t.Fatal("compiled Shader source substitution was accepted")
		}
	}
	corrupt := bytes.Clone(output)
	binary.BigEndian.PutUint64(corrupt[32:40], uint64(len(corrupt)+1))
	if _, _, err := ExtractPlayerShader(corrupt, proof.GUID, proof.Name); err == nil {
		t.Fatal("out-of-bounds compiled Shader was accepted")
	}
}

func TestExtractPlayerShaderRejectsMalformedMetadata(t *testing.T) {
	for _, input := range [][]byte{nil, make([]byte, 64)} {
		if _, _, err := ExtractPlayerShader(input, "2cbfcd0349c944ee788f6e2919b9d66c", "Dungeon/Screen"); err == nil {
			t.Fatal("invalid compiled Shader was accepted")
		}
	}
}

func TestPlayerShaderReferencesCannotBeHiddenByFixedStructSize(t *testing.T) {
	// Fixed-size wrappers still need traversal. A dependency hidden in such a
	// wrapper would become dangling after the original tables are removed.
	strings := []byte("Shader\x00Base\x00Wrapper\x00wrapper\x00PPtr<Texture>\x00texture\x00")
	var nodes [3 * 32]byte
	for index, value := range []struct {
		level           byte
		typ, name, size uint32
	}{{0, 0, 7, 12}, {1, 12, 20, 12}, {2, 28, 42, 12}} {
		node := nodes[index*32 : (index+1)*32]
		node[2] = value.level
		binary.LittleEndian.PutUint32(node[4:8], value.typ)
		binary.LittleEndian.PutUint32(node[8:12], value.name)
		binary.LittleEndian.PutUint32(node[12:16], value.size)
	}
	tree := make([]byte, 16+len(nodes)+len(strings))
	binary.LittleEndian.PutUint32(tree[8:12], 3)
	binary.LittleEndian.PutUint32(tree[12:16], uint32(len(strings)))
	copy(tree[16:], nodes[:])
	copy(tree[16+len(nodes):], strings)
	payload := make([]byte, 12)
	binary.LittleEndian.PutUint64(payload[4:], 12345)
	if _, err := readPlayerShader(payload, tree); err == nil || !stringsContains(err.Error(), "live dependency") {
		t.Fatalf("nested PPtr was not rejected: %v", err)
	}
}

func stringsContains(value, part string) bool { return bytes.Contains([]byte(value), []byte(part)) }
