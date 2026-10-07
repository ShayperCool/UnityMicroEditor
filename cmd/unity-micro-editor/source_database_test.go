package main

import (
	"bytes"
	"encoding/hex"
	"os"
	"path/filepath"
	"testing"
)

func TestSourceHashesMatchNativeUnityGolden(t *testing.T) {
	// Captured from a closed Unity 6000.6.3f1 oracle SourceAssetDB hash
	// table for these exact deterministic fixture bytes.
	for _, vector := range []struct {
		data []byte
		hash string
	}{
		{fixtureWAV(), "36de4a9c109bdaa391f080c0fd24a265"},
		{fixtureAudioMeta(), "f1df8fafd8fc01412ecb79b3bf5b9377"},
	} {
		hash := nativeSourceHash(vector.data)
		if hex.EncodeToString(hash[:]) != vector.hash {
			t.Fatalf("native Unity digest mismatch: %x, want %s", hash, vector.hash)
		}
	}
}

func TestSourceRegistryResolvesAssetsAndKeepsNativeHashes(t *testing.T) {
	source := fixtureSource(t)
	fixture, err := CreateFixture(source, filepath.Join(source, "Temp/registry"), false)
	if err != nil {
		t.Fatal(err)
	}
	report, err := WriteSourceRegistry(fixture)
	if err != nil {
		t.Fatal(err)
	}
	if report.GUIDCount != 4 || report.TableCounts["PropertyIDToType"] != 55 {
		t.Fatalf("unexpected registry %+v", report)
	}
	tables, err := ReadLMDBTables(report.Database)
	if err != nil {
		t.Fatal(err)
	}
	var mapping []byte
	for _, record := range tables["PathToGuid"] {
		if string(record.Key) == "assets/probe/probe.wav\x00" {
			mapping = record.Value
		}
	}
	if len(mapping) != 48 || hex.EncodeToString(mapping[:16]) != "1707255666968716574696f60727f626" {
		t.Fatalf("native path/GUID lookup mismatch: %x", mapping)
	}
	if hex.EncodeToString(mapping[16:32]) != "f1df8fafd8fc01412ecb79b3bf5b9377" ||
		hex.EncodeToString(mapping[32:48]) != "36de4a9c109bdaa391f080c0fd24a265" {
		t.Fatal("native meta/source hashes are not restored")
	}
	before, err := os.ReadFile(report.Database)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := WriteSourceRegistry(fixture); err == nil {
		t.Fatal("populated source registry was overwritten")
	}
	after, _ := os.ReadFile(report.Database)
	if !bytes.Equal(before, after) {
		t.Fatal("rejected replacement changed database")
	}
}

func TestSourceRegistryRejectsEditedInputs(t *testing.T) {
	source := fixtureSource(t)
	fixture, err := CreateFixture(source, filepath.Join(source, "Temp/registry-edited"), false)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(fixture.Project, audioAsset), []byte("edited"), 0644); err != nil {
		t.Fatal(err)
	}
	if _, err := WriteSourceRegistry(fixture); err == nil {
		t.Fatal("changed source bytes accepted with stale fixture manifest")
	}
	if _, err := os.Stat(filepath.Join(fixture.Project, "Library/SourceAssetDB")); !os.IsNotExist(err) {
		t.Fatal("failed validation wrote database")
	}
}

func TestSourceRegistryRejectsSymlinkInput(t *testing.T) {
	source := fixtureSource(t)
	fixture, err := CreateFixture(source, filepath.Join(source, "Temp/registry-linked"), false)
	if err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(fixture.Project, audioAsset)
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	external := filepath.Join(t.TempDir(), "external.wav")
	if err := os.WriteFile(external, data, 0600); err != nil {
		t.Fatal(err)
	}
	if err := os.Remove(path); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(external, path); err != nil {
		t.Fatal(err)
	}
	if _, err := WriteSourceRegistry(fixture); err == nil {
		t.Fatal("external input symlink accepted")
	}
}
