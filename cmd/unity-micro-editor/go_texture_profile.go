package main

import (
	"bytes"
	"encoding/binary"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	goproject "unitymicroeditor/internal/project"

	"gopkg.in/yaml.v3"
)

// Cold TextureImporter delivery is proven for uncompressed ordinary textures,
// including native resize/mips. Compression requires additional pinned native
// compressor dependencies; until verified, use only exact existing artifacts.
func nativePNGNeedsWarm(project, logical string) (bool, error) {
	if !strings.EqualFold(filepath.Ext(logical), ".png") {
		return false, nil
	}
	physical, err := resolveProjectImportSource(project, logical+".meta")
	if err != nil {
		return false, err
	}
	info, err := os.Stat(physical)
	if err != nil || !info.Mode().IsRegular() || info.Size() > 1<<20 {
		return false, errors.New("native texture profile requires bounded source metadata")
	}
	data, err := os.ReadFile(physical)
	if err != nil {
		return false, err
	}
	var meta struct {
		Importer *struct {
			TextureType  int `yaml:"textureType"`
			TextureShape int `yaml:"textureShape"`
			Platforms    []struct {
				Target      string `yaml:"buildTarget"`
				Overridden  int    `yaml:"overridden"`
				Compression *int   `yaml:"textureCompression"`
				Format      *int   `yaml:"textureFormat"`
				Crunched    int    `yaml:"crunchedCompression"`
			} `yaml:"platformSettings"`
		} `yaml:"TextureImporter"`
	}
	if err := yaml.Unmarshal(data, &meta); err != nil {
		return false, err
	}
	if meta.Importer == nil || meta.Importer.TextureType != 0 || meta.Importer.TextureShape != 1 || len(meta.Importer.Platforms) > 64 {
		return true, nil
	}
	selected := -1
	seen := map[string]bool{}
	for index, platform := range meta.Importer.Platforms {
		if platform.Target == "" || seen[platform.Target] {
			return false, errors.New("native texture metadata has a repeated or missing platform")
		}
		seen[platform.Target] = true
		if platform.Target == "DefaultTexturePlatform" {
			selected = index
		}
	}
	for index, platform := range meta.Importer.Platforms {
		if platform.Target == "Standalone" && platform.Overridden != 0 {
			selected = index
		}
	}
	if selected < 0 {
		return true, nil
	}
	platform := meta.Importer.Platforms[selected]
	return platform.Compression == nil || *platform.Compression != 0 || platform.Format == nil || *platform.Format != -1 || platform.Crunched != 0, nil
}

func warmTextureConfiguration(project, logical string) (map[string]string, error) {
	warm, err := nativePNGNeedsWarm(project, logical)
	if err != nil || !warm {
		return nil, err
	}
	sha, _, err := readBC7CompressorSettings(project)
	if err != nil {
		return nil, err
	}
	return map[string]string{"ProjectSettings/EditorSettings.asset": sha}, nil
}

func readBC7CompressorSettings(project string) (string, byte, error) {
	physical, err := resolveProjectImportSource(project, "ProjectSettings/EditorSettings.asset")
	if err != nil {
		return "", 0, err
	}
	info, err := os.Stat(physical)
	if err != nil || info.Size() > 1<<20 {
		return "", 0, errors.New("texture compressor settings must be bounded source text")
	}
	data, err := os.ReadFile(physical)
	if err != nil {
		return "", 0, err
	}
	documents, err := goproject.ParseUnityYAML(data)
	if err != nil || len(documents) != 1 || documents[0].ClassID != 159 || documents[0].TypeName != "EditorSettings" {
		return "", 0, errors.New("texture compressor requires one authored EditorSettings document")
	}
	bc7, ok := documents[0].Data["m_Bc7TextureCompressor"].(int64)
	if !ok || bc7 < 0 || bc7 > 2 {
		return "", 0, fmt.Errorf("unsupported pinned BC7 compressor setting: %v", documents[0].Data["m_Bc7TextureCompressor"])
	}
	if bc7 == 0 {
		// Pinned native EditorSettings::GetEffectiveBc7TextureCompressor maps
		// authored Default(0) to ISPC(2); other explicit enum values are kept.
		bc7 = 2
	}
	return bytesSHA(data), byte(bc7), nil
}

func verifyWarmBC7Dependency(project string, dependencies []byte) error {
	_, compressor, err := readBC7CompressorSettings(project)
	if err != nil {
		return err
	}
	actual, err := recordedBC7Dependency(dependencies)
	if err != nil {
		return err
	}
	expected := [16]byte{1, compressor}
	if actual != expected {
		return errors.New("warm native BC7 artifact was imported with different compressor settings")
	}
	return nil
}

// The pinned ArtifactDB dependency blob contains 8-aligned 24-byte custom
// dependency entries: relative string pointer, zero padding, Hash128. Match the
// recorded key through its pointer; an unlinked string occurrence is no proof.
func recordedBC7Dependency(dependencies []byte) ([16]byte, error) {
	var hash [16]byte
	if len(dependencies) < 128 || len(dependencies) > 1<<20 {
		return hash, errors.New("warm BC7 dependency blob exceeds pinned bounds")
	}
	const key = "graphics/bc7-compressor"
	found := false
	for field := 128; field+24 <= len(dependencies); field += 8 {
		if binary.LittleEndian.Uint32(dependencies[field+4:field+8]) != 0 {
			continue
		}
		name, err := blobString(dependencies, field)
		if err != nil || name != key {
			continue
		}
		if found {
			return hash, errors.New("warm BC7 compressor dependency is ambiguous")
		}
		copy(hash[:], dependencies[field+8:field+24])
		found = true
	}
	if !found || bytes.Equal(hash[:], make([]byte, 16)) {
		return hash, errors.New("warm BC7 artifact lacks its recorded compressor dependency")
	}
	return hash, nil
}

func validateNativeConfiguration(project, source string, configuration map[string]string) error {
	expected, err := warmTextureConfiguration(project, source)
	if err != nil {
		return err
	}
	if len(expected) != 0 && configuration["ProjectSettings/EditorSettings.asset"] != expected["ProjectSettings/EditorSettings.asset"] {
		return errors.New("native compressed texture cache lacks the current compressor settings guard")
	}
	for logical, expected := range configuration {
		if logical != "ProjectSettings/EditorSettings.asset" || !validSHA256(expected) {
			return errors.New("unsupported native configuration source guard")
		}
		physical, err := resolveProjectImportSource(project, logical)
		if err != nil {
			return err
		}
		actual, err := goFileHash(physical)
		if err != nil || actual != expected {
			return errors.New("native texture compressor settings changed")
		}
	}
	return nil
}
