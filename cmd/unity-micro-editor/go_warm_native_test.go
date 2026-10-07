package main

import (
	"bytes"
	"context"
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
