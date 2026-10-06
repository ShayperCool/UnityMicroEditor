package main

import (
	"bytes"
	"context"
	"unitymicroeditor/worker/runtimeartifact"
	"encoding/binary"
	"encoding/hex"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestWarmNativeUIRequiresExactSourceMetadataAndPinnedImport(t *testing.T) {
	for _, variant := range []string{"stale source", "stale metadata", "wrong GUID", "missing current import"} {
		t.Run(variant, func(t *testing.T) {
			project := t.TempDir()
			logical := "Assets/UI.uss"
			source := []byte(".root {color: red;}")
			meta := []byte("guid: 11111111111111111111111111111111\n")
			if err := os.MkdirAll(filepath.Join(project, "Assets"), 0700); err != nil {
				t.Fatal(err)
			}
			if err := os.MkdirAll(filepath.Join(project, "Library/DataStore"), 0700); err != nil {
				t.Fatal(err)
			}
			if err := os.WriteFile(filepath.Join(project, logical), source, 0600); err != nil {
				t.Fatal(err)
			}
			if err := os.WriteFile(filepath.Join(project, logical+".meta"), meta, 0600); err != nil {
				t.Fatal(err)
			}
			wire, _ := sourceRegistryGUID("11111111111111111111111111111111")
			sourceHash, metaHash := nativeSourceHash(source), nativeSourceHash(meta)
			mapping := append(bytes.Clone(wire), metaHash[:]...)
			mapping = append(mapping, sourceHash[:]...)
			switch variant {
			case "stale source":
				mapping[32] ^= 1
			case "stale metadata":
				mapping[16] ^= 1
			case "wrong GUID":
				mapping[0] ^= 1
			}
			db, err := buildFixtureLMDB(map[string][]LMDBRecord{"PathToGuid": {{Key: []byte(strings.ToLower(logical) + "\x00"), Value: mapping}}})
			if err != nil {
				t.Fatal(err)
			}
			if err := os.WriteFile(filepath.Join(project, "Library/SourceAssetDB"), db, 0600); err != nil {
				t.Fatal(err)
			}
			db, err = buildFixtureLMDB(map[string][]LMDBRecord{"CurrentRevisions": {}})
			if err != nil {
				t.Fatal(err)
			}
			if err := os.WriteFile(filepath.Join(project, "Library/ArtifactDB"), db, 0600); err != nil {
				t.Fatal(err)
			}
			if err := os.WriteFile(filepath.Join(project, "Library/DataStore/uds.db"), db, 0600); err != nil {
				t.Fatal(err)
			}
			output := filepath.Join(project, "Temp/runtime.bin")
			if _, err := extractWarmNativeUI(context.Background(), project, logical, output); err == nil {
				t.Fatal("accepted unproven cached source revision")
			}
			if _, err := os.Stat(output); !os.IsNotExist(err) {
				t.Fatalf("failed validation published runtime data: %v", err)
			}
		})
	}
}

func TestWarmNativeUIExactProjectArtifacts(t *testing.T) {
	project := os.Getenv("UNITY_WORKER_UI_PROJECT")
	if project == "" {
		t.Skip("set UNITY_WORKER_UI_PROJECT to an imported pinned-version project")
	}
	for _, logical := range []string{"Assets/Dungeon/Net/UI/Lobby.uss", "Assets/Dungeon/Net/UI/Lobby.uxml", "Assets/Dungeon/UI/Terminal.uss", "Assets/Dungeon/UI/Fonts/VT323-Regular.ttf", "Assets/Dungeon/UI/Fonts/Silkscreen-Regular.ttf"} {
		t.Run(filepath.Base(logical), func(t *testing.T) {
			output := filepath.Join(t.TempDir(), "runtime.bin")
			proof, err := extractWarmNativeUI(context.Background(), project, logical, output)
			if err != nil {
				t.Fatal(err)
			}
			if !proof.Primary.ContentKeyVerified || proof.ImportResult == "" || proof.SourceNative == "" || proof.MetaNative == "" || proof.Metadata.RuntimeTarget != 24 {
				t.Fatal("source/import/runtime proof is incomplete")
			}
			store, err := OpenUDSArtifactStore(filepath.Join(project, "Library/DataStore"))
			if err != nil {
				t.Fatal(err)
			}
			hash, err := hex.DecodeString(proof.Primary.RawHashKey)
			if err != nil {
				t.Fatal(err)
			}
			original, _, err := store.Read(hash)
			if err != nil {
				t.Fatal(err)
			}
			runtime, err := os.ReadFile(output)
			if err != nil {
				t.Fatal(err)
			}
			offset := binary.BigEndian.Uint64(original[32:40])
			if !bytes.Equal(original[offset:], runtime[offset:]) {
				t.Fatal("native UI/font importer payload changed")
			}
			if bytesSHA(runtime) != proof.Metadata.OutputSHA256 {
				t.Fatal("published runtime descriptor hash differs")
			}
			if _, err := extractWarmNativeUI(context.Background(), project, logical, output); err == nil {
				t.Fatal("immutable output was overwritten")
			}
		})
	}
}

func TestWarmNativeTextureExactProjectArtifact(t *testing.T) {
	project := os.Getenv("UNITY_WORKER_UI_PROJECT")
	if project == "" {
		t.Skip("set UNITY_WORKER_UI_PROJECT to an imported pinned-version project")
	}
	output := filepath.Join(t.TempDir(), "runtime.bin")
	proof, err := extractWarmNativeUI(context.Background(), project, "Assets/Dungeon/ShipGen/Art/Pixel/Decals/Poster_D.png", output)
	if err != nil {
		t.Fatal(err)
	}
	if len(proof.Metadata.Objects) != 1 || proof.Metadata.Objects[0].FileID != "2800000" || proof.Metadata.Objects[0].TypeName != "UnityEngine.Texture2D" {
		t.Fatalf("texture native identity differs: %+v", proof)
	}
	if proof.Metadata.Texture == nil {
		t.Fatal("native texture layout metadata is missing")
	}
	if *proof.Metadata.Texture != (runtimeartifact.TextureMetadata{Width: 128, Height: 128, Format: 3, MipCount: 8, CompleteImageSize: 65535}) {
		t.Fatalf("pinned Unity NPOT resize/layout changed: %+v", proof.Metadata.Texture)
	}
	store, err := OpenUDSArtifactStore(filepath.Join(project, "Library/DataStore"))
	if err != nil {
		t.Fatal(err)
	}
	hash, err := hex.DecodeString(proof.Primary.RawHashKey)
	if err != nil {
		t.Fatal(err)
	}
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
		t.Fatal("Unity's resized/mipped texture payload changed")
	}
	t.Logf("native imported texture layout: %+v", *proof.Metadata.Texture)
	t.Logf("texture source/meta/import/artifact proof: %+v", proof)
}

func TestWarmNativeShaderRequiresGPUCompilation(t *testing.T) {
	project := os.Getenv("UNITY_WORKER_UI_PROJECT")
	if project == "" {
		t.Skip("set UNITY_WORKER_UI_PROJECT to an imported pinned-version project")
	}
	output := filepath.Join(t.TempDir(), "shader.runtime.bin")
	_, err := extractWarmNativeUI(context.Background(), project, "Assets/Dungeon/Shaders/Screen/Screen.shader", output)
	if err == nil || !strings.Contains(err.Error(), "GPU compilation") {
		t.Fatalf("parse-only shader must require GPU compilation: %v", err)
	}
	if _, err := os.Stat(output); !os.IsNotExist(err) {
		t.Fatalf("parse-only shader output must not be published: %v", err)
	}
}
