package main

import (
	"bytes"
	"context"
	"encoding/binary"
	"encoding/hex"
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

func TestWarmNativeBC7ExactProjectArtifact(t *testing.T) {
	project := os.Getenv("UNITY_WORKER_UI_PROJECT")
	if project == "" {
		t.Skip("set UNITY_WORKER_UI_PROJECT for actual stopped-cache BC7 evidence")
	}
	logical := "Assets/Dungeon/ShipGen/Art/Sky/Starfield.png"
	configuration, err := warmTextureConfiguration(project, logical)
	if err != nil || len(configuration) == 0 {
		t.Fatalf("BC7 configuration source proof missing: %v", err)
	}
	output := filepath.Join(t.TempDir(), "runtime.bin")
	proof, err := extractWarmNativeUI(context.Background(), project, logical, output)
	if err != nil {
		t.Fatal(err)
	}
	if proof.Metadata.Texture == nil || proof.Metadata.Texture.Format != 25 {
		t.Fatalf("expected native BC7 Starfield Texture2D, got %+v", proof.Metadata.Texture)
	}
	store, err := OpenUDSArtifactStore(filepath.Join(project, "Library/DataStore"))
	if err != nil {
		t.Fatal(err)
	}
	hash, _ := hex.DecodeString(proof.Primary.RawHashKey)
	original, _, err := store.Read(hash)
	if err != nil {
		t.Fatal(err)
	}
	runtime, err := os.ReadFile(output)
	if err != nil {
		t.Fatal(err)
	}
	dataOffset := binary.BigEndian.Uint64(original[32:40])
	if !bytes.Equal(original[dataOffset:], runtime[dataOffset:]) {
		t.Fatal("BC7 native object payload changed in runtime projection")
	}
	dependencyRecords, err := ReadLMDBTable(filepath.Join(project, "Library/ArtifactDB"), "ImportResultIDToArtifactDependencies")
	if err != nil {
		t.Fatal(err)
	}
	resultID, _ := hex.DecodeString(proof.ImportResult)
	verifiedDependency := false
	for _, record := range dependencyRecords {
		if !bytes.Equal(record.Key, resultID) {
			continue
		}
		if err := verifyWarmBC7Dependency(project, record.Value); err != nil {
			t.Fatal(err)
		}
		verifiedDependency = true
	}
	if !verifiedDependency {
		t.Fatal("actual BC7 importer dependency proof missing")
	}
	t.Logf("actual native BC7 texture=%+v, runtime=%s, compressor configuration=%v; no native worker started", *proof.Metadata.Texture, proof.Metadata.OutputSHA256, configuration)
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
