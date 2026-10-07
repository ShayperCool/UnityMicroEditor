package runtimeartifact

import (
	"bytes"
	"encoding/binary"
	"testing"
)

func TestExtractPlayerShaderRejectsMalformedMetadata(t *testing.T) {
	for _, input := range [][]byte{nil, make([]byte, 64)} {
		if _, _, err := ExtractPlayerShader(input, "2cbfcd0349c944ee788f6e2919b9d66c", "Sample/Screen"); err == nil {
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
