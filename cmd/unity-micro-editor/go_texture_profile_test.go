package main

import (
	"encoding/binary"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

const nativeTextureProfileFixture = `guid: 22222222222222222222222222222222
TextureImporter:
  textureType: 0
  textureShape: 1
  platformSettings:
  - buildTarget: DefaultTexturePlatform
    textureCompression: 0
    textureFormat: -1
  - buildTarget: Standalone
    overridden: 0
    textureCompression: 2
    textureFormat: -1
`

func TestNativeTextureProfilePreservesEffectivePlatformSettingsAndCompressorGuard(t *testing.T) {
	project := t.TempDir()
	meta := filepath.Join(project, "Assets/Texture.png.meta")
	if err := os.MkdirAll(filepath.Dir(meta), 0700); err != nil {
		t.Fatal(err)
	}
	writeMeta := func(data string) {
		if err := os.WriteFile(meta, []byte(data), 0600); err != nil {
			t.Fatal(err)
		}
	}
	writeMeta(nativeTextureProfileFixture)
	if warm, err := nativePNGNeedsWarm(project, "Assets/Texture.png"); err != nil || warm {
		t.Fatalf("inactive compressed override suppressed proven cold native route: %v %v", warm, err)
	}
	writeMeta(strings.Replace(nativeTextureProfileFixture, "overridden: 0", "overridden: 1", 1))
	if warm, err := nativePNGNeedsWarm(project, "Assets/Texture.png"); err != nil || !warm {
		t.Fatalf("active BC7 override reached unproven cold route: %v %v", warm, err)
	}
	settings := filepath.Join(project, "ProjectSettings/EditorSettings.asset")
	if err := os.MkdirAll(filepath.Dir(settings), 0700); err != nil {
		t.Fatal(err)
	}
	data := "--- !u!159 &1\nEditorSettings:\n  m_Bc7TextureCompressor: 0\n"
	if err := os.WriteFile(settings, []byte(data), 0600); err != nil {
		t.Fatal(err)
	}
	configuration, err := warmTextureConfiguration(project, "Assets/Texture.png")
	if err != nil || configuration["ProjectSettings/EditorSettings.asset"] != bytesSHA([]byte(data)) {
		t.Fatalf("compressor configuration source guard missing: %v %v", configuration, err)
	}
	if err := validateNativeConfiguration(project, "Assets/Texture.png", configuration); err != nil {
		t.Fatal(err)
	}
	if err := validateNativeConfiguration(project, "Assets/Texture.png", nil); err == nil {
		t.Fatal("legacy compressed texture cache without compressor guard accepted")
	}
	if err := os.WriteFile(settings, []byte(strings.Replace(data, ": 0", ": 1", 1)), 0600); err != nil {
		t.Fatal(err)
	}
	if err := validateNativeConfiguration(project, "Assets/Texture.png", configuration); err == nil {
		t.Fatal("changed BC7 compressor reused prior compressed artifact guard")
	}
}

func TestWarmBC7HistoricalDependencyPreventsRestampingOldCompressedArtifact(t *testing.T) {
	project := t.TempDir()
	settings := filepath.Join(project, "ProjectSettings/EditorSettings.asset")
	if err := os.MkdirAll(filepath.Dir(settings), 0700); err != nil {
		t.Fatal(err)
	}
	writeSettings := func(value string) {
		if err := os.WriteFile(settings, []byte("--- !u!159 &1\nEditorSettings:\n  m_Bc7TextureCompressor: "+value+"\n"), 0600); err != nil {
			t.Fatal(err)
		}
	}
	writeSettings("0")
	data := make([]byte, 224)
	copy(data[176:], "graphics/bc7-compressor\x00")
	binary.LittleEndian.PutUint32(data[128:132], 176-128)
	data[136], data[137] = 1, 2
	if err := verifyWarmBC7Dependency(project, data); err != nil {
		t.Fatalf("pinned Default->ISPC native dependency rejected: %v", err)
	}
	writeSettings("1")
	if err := verifyWarmBC7Dependency(project, data); err == nil {
		t.Fatal("changed compressor reused historical BC7 output with a fresh configuration stamp")
	}
	writeSettings("0")
	binary.LittleEndian.PutUint32(data[128:132], 0)
	if _, err := recordedBC7Dependency(data); err == nil {
		t.Fatal("unlinked BC7 dependency string accepted as a native hash proof")
	}
	binary.LittleEndian.PutUint32(data[128:132], 176-128)
	binary.LittleEndian.PutUint32(data[152:156], 176-152)
	data[160], data[161] = 1, 2
	if _, err := recordedBC7Dependency(data); err == nil {
		t.Fatal("ambiguous native BC7 compressor dependencies accepted")
	}
}
