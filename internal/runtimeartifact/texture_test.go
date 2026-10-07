package runtimeartifact

import (
	"bytes"
	"encoding/binary"
	"testing"
)

func textureResourceTree(t *testing.T) []byte {
	t.Helper()
	type node struct {
		name  string
		level byte
		flags byte
		size  int32
		meta  uint32
	}
	values := []node{{"Texture2D", 0, 0, -1, 0}, {"m_Width", 1, 0, 4, 0}, {"m_Height", 1, 0, 4, 0}, {"m_TextureFormat", 1, 0, 4, 0}, {"m_MipCount", 1, 0, 4, 0}, {"m_CompleteImageSize", 1, 0, 4, 0},
		{"image data", 1, 0, -1, 0}, {"Array", 2, 1, -1, 0x4000}, {"size", 3, 0, 4, 0}, {"data", 3, 0, 1, 0},
		{"m_StreamData", 1, 0, -1, 0}, {"offset", 2, 0, 8, 0}, {"size", 2, 0, 4, 0}, {"path", 2, 0, -1, 0x4000}}
	var nodes, chars bytes.Buffer
	for _, value := range values {
		data := make([]byte, 32)
		data[2], data[3] = value.level, value.flags
		binary.LittleEndian.PutUint32(data[8:12], uint32(chars.Len()))
		binary.LittleEndian.PutUint32(data[12:16], uint32(value.size))
		binary.LittleEndian.PutUint32(data[20:24], value.meta)
		nodes.Write(data)
		chars.WriteString(value.name + "\x00")
	}
	header := make([]byte, 16)
	binary.LittleEndian.PutUint32(header[8:12], uint32(len(values)))
	binary.LittleEndian.PutUint32(header[12:16], uint32(chars.Len()))
	return append(append(header, nodes.Bytes()...), chars.Bytes()...)
}

func TestNativeTextureTreeFindsStreamAndPreservesLayout(t *testing.T) {
	path := "VirtualArtifacts/Primary/22222222222222222222222222222222.resource"
	var object bytes.Buffer
	for _, value := range []uint32{32, 16, 10, 6, 512, 4, 0x12345678} {
		binary.Write(&object, binary.LittleEndian, value)
	}
	binary.Write(&object, binary.LittleEndian, uint64(16))
	binary.Write(&object, binary.LittleEndian, uint32(256))
	binary.Write(&object, binary.LittleEndian, uint32(len(path)))
	object.WriteString(path)
	for object.Len()%4 != 0 {
		object.WriteByte(0)
	}
	resources, meta, err := readTextureResources(object.Bytes(), textureResourceTree(t))
	if err != nil {
		t.Fatal(err)
	}
	if len(resources) != 1 || resources[0].VirtualPath != path || resources[0].Offset != 16 || resources[0].Size != 256 || meta.Width != 32 || meta.Height != 16 || meta.Format != 10 || meta.MipCount != 6 || meta.CompleteImageSize != 512 {
		t.Fatalf("texture resource/layout changed: %+v %+v", resources, meta)
	}
	for _, variant := range []string{"truncated", "extra tail", "invalid dimensions", "invalid resource alias"} {
		bad := bytes.Clone(object.Bytes())
		switch variant {
		case "truncated":
			bad = bad[:len(bad)-1]
		case "extra tail":
			bad = append(bad, 1)
		case "invalid dimensions":
			binary.LittleEndian.PutUint32(bad[:4], 0)
		case "invalid resource alias":
			bad[44] = '!'
		}
		if _, _, err := readTextureResources(bad, textureResourceTree(t)); err == nil {
			t.Fatalf("accepted malformed texture payload: %s", variant)
		}
	}
}

func TestTextureProjectionRejectsUnverifiedProfileAndHeader(t *testing.T) {
	for _, profile := range []string{"", "fbx", "uss", "png", "jpg", "jpeg"} {
		if _, _, err := AdaptTexture(make([]byte, 64), profile); err == nil {
			t.Fatal("unverified native Texture2D content was accepted")
		}
	}
}
