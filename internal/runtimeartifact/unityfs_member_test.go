package runtimeartifact

import (
	"bytes"
	"encoding/binary"
	"os"
	"testing"
)

func TestVerifyUnityFSActualShaderMember(t *testing.T) {
	memberPath, bundlePath := os.Getenv("UNITY_PLAY_COMPILED_SHADER_FIXTURE"), os.Getenv("UNITY_PLAY_COMPILED_SHADER_BUNDLE")
	if memberPath == "" || bundlePath == "" {
		t.Skip("set compiled Shader member and bundle fixture paths")
	}
	member, err := os.ReadFile(memberPath)
	if err != nil {
		t.Fatal(err)
	}
	archive, err := os.ReadFile(bundlePath)
	if err != nil {
		t.Fatal(err)
	}
	name := "CAB-c7e0997e8a48cbbb3c94d6b2c3d64de1.sharedAssets"
	if err := VerifyUnityFSMember(archive, name, member); err != nil {
		t.Fatal(err)
	}
	member[0] ^= 1
	if err := VerifyUnityFSMember(archive, name, member); err == nil {
		t.Fatal("accepted substituted member bytes")
	}
	if err := VerifyUnityFSMember(archive, "different", member); err == nil {
		t.Fatal("accepted substituted member name")
	}
}

func TestVerifyUnityFSRawMemberAndBounds(t *testing.T) {
	member := []byte("compiled shader fixture")
	var info bytes.Buffer
	info.Write(make([]byte, 16))
	for _, v := range []any{uint32(1), uint32(len(member)), uint32(len(member)), uint16(0), uint32(1), uint64(0), uint64(len(member)), uint32(4)} {
		if err := binary.Write(&info, binary.BigEndian, v); err != nil {
			t.Fatal(err)
		}
	}
	info.WriteString("shader.member\x00")
	var header bytes.Buffer
	header.WriteString("UnityFS\x00")
	_ = binary.Write(&header, binary.BigEndian, uint32(8))
	header.WriteString("6000.6.3f1\x00revision\x00")
	sizePosition := header.Len()
	_ = binary.Write(&header, binary.BigEndian, uint64(0))
	_ = binary.Write(&header, binary.BigEndian, uint32(info.Len()))
	_ = binary.Write(&header, binary.BigEndian, uint32(info.Len()))
	_ = binary.Write(&header, binary.BigEndian, uint32(0x40))
	for header.Len()%16 != 0 {
		header.WriteByte(0)
	}
	header.Write(info.Bytes())
	header.Write(member)
	archive := header.Bytes()
	binary.BigEndian.PutUint64(archive[sizePosition:sizePosition+8], uint64(len(archive)))
	if err := VerifyUnityFSMember(archive, "shader.member", member); err != nil {
		t.Fatal(err)
	}
	for _, input := range [][]byte{nil, archive[:10], archive[:len(archive)-1]} {
		if err := VerifyUnityFSMember(input, "shader.member", member); err == nil {
			t.Fatal("accepted malformed UnityFS archive")
		}
	}
}

func TestUnityFSLZ4OverlappingMatchAndMalformedOffsets(t *testing.T) {
	// One literal plus an overlapping offset-1 match, followed by a literal.
	decoded, err := unpackUnityFSBlock([]byte{0x10, 'A', 1, 0, 0x10, 'B'}, 6, 3)
	if err != nil || string(decoded) != "AAAAAB" {
		t.Fatalf("LZ4 overlap failed: %q %v", decoded, err)
	}
	for _, input := range [][]byte{{0x10, 'A', 0, 0}, {0x10, 'A', 2, 0}, {0xf0}, {0x10}, {0x00, 1}} {
		if _, err := unpackUnityFSBlock(input, 6, 2); err == nil {
			t.Fatal("invalid LZ4 block accepted")
		}
	}
}
