package goproject

import (
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"io"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

const largeClipGUID = "22222222222222222222222222222222"
const lateNativeGUID = "33333333333333333333333333333333"

func streamingTestHash(t *testing.T, path string) string {
	t.Helper()
	f, err := os.Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer f.Close()
	h := sha256.New()
	if _, err := io.Copy(h, f); err != nil {
		t.Fatal(err)
	}
	return hex.EncodeToString(h.Sum(nil))
}

func cookedLargeClipFixture(t *testing.T, suffix string) (*Session, string, string) {
	t.Helper()
	s, scene, _ := fixtureSession(t)
	logical := "Assets/Large.anim"
	path := filepath.Join(s.project, logical)
	f, err := os.Create(path)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := io.WriteString(f, "--- !u!74 &7400000\nAnimationClip:\n  m_Name: Large\n  _typelessdata: "); err != nil {
		t.Fatal(err)
	}
	chunk := strings.Repeat("0", nativeReferenceLineBytes)
	for size := 0; size <= maxTextBytes; size += len(chunk) {
		if _, err := io.WriteString(f, chunk); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := io.WriteString(f, "\n"+suffix); err != nil {
		t.Fatal(err)
	}
	if err := f.Close(); err != nil {
		t.Fatal(err)
	}
	writeFixtureFile(t, path+".meta", "fileFormatVersion: 2\nguid: "+largeClipGUID+"\n")
	writeFixtureFile(t, scene, fixtureScene+"--- !u!114 &90\nMonoBehaviour:\n  clip: {fileID: 7400000, guid: "+largeClipGUID+", type: 2}\n")
	packPath := filepath.Join(s.project, "Library/UnityMicroEditor/native-test/registry.bundle")
	writeFixtureFile(t, packPath, "independently verified immutable native AnimationClip registry content")
	stamp, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	packStamp, err := os.Stat(packPath)
	if err != nil {
		t.Fatal(err)
	}
	sourceSHA, metaSHA := streamingTestHash(t, path), streamingTestHash(t, path+".meta")
	s.catalog.Success, s.catalog.CookedRegistryVerified = true, true
	s.catalog.Backend, s.catalog.SceneLoader = "unity_editor_public_api", "raw"
	s.catalog.Assets = []map[string]any{{"guid": largeClipGUID, "fileId": "7400000", "classId": 74, "kind": "imported", "path": logical, "typeName": "UnityEngine.AnimationClip"}}
	s.catalog.SourceDependencies = []dependency{{Path: logical, SHA256: sourceSHA, Bytes: stamp.Size()}, {Path: logical + ".meta", SHA256: metaSHA, Bytes: 84}}
	s.critical[logical], s.critical[logical+".meta"] = sourceSHA, metaSHA
	pack := nativeDiscoveryPack{BundlePath: packPath, SHA256: streamingTestHash(t, packPath), Bytes: packStamp.Size(), RegistryAsset: "assets/native-registry.asset"}
	s.catalog.Packs = []nativeDiscoveryPack{pack}
	s.baseline["importedAssetPacks"] = []any{map[string]any{"bundlePath": pack.BundlePath, "sha256": pack.SHA256, "bytes": pack.Bytes, "registryAsset": pack.RegistryAsset}}
	s.indexDirty = true
	if err := s.indexGUIDs(); err != nil {
		t.Fatal(err)
	}
	return s, path, packPath
}

func TestCookedLargeNativeDiscoveryStreamsCurrentSourceAndKeepsLateDependencyClosure(t *testing.T) {
	s, path, _ := cookedLargeClipFixture(t, "  reference: {fileID: 4300000, guid: "+lateNativeGUID+", type: 3}\n")
	writeFixtureFile(t, filepath.Join(s.project, "Assets/Late.fbx"), "native model beyond old clip byte limit")
	writeFixtureFile(t, filepath.Join(s.project, "Assets/Late.fbx.meta"), "guid: "+lateNativeGUID+"\n")
	s.indexDirty = true
	if err := s.indexGUIDs(); err != nil {
		t.Fatal(err)
	}
	refs, err := s.nativeReferenceGUIDs(path)
	if err != nil || !reflect.DeepEqual(refs, []string{lateNativeGUID}) {
		t.Fatalf("late current cooked PPtr was lost: %v %v", refs, err)
	}
	if s.files[path].digest != s.critical["Assets/Large.anim"] || len(s.files[path].content) != 0 || len(s.documents) != 0 {
		t.Fatal("streaming discovery lost exact whole-source hash or retained large text/YAML allocations")
	}
	for pass := 0; pass < 2; pass++ {
		pending, err := s.PendingNativeImportSources()
		if err != nil || !reflect.DeepEqual(pending, []string{"Assets/Late.fbx"}) {
			t.Fatalf("closure beyond old byte limit differs on pass%d: %v %v", pass, pending, err)
		}
	}
}

func TestCookedLargeNativeDiscoveryRejectsChangedCurrentSourceAndMetadata(t *testing.T) {
	for _, suffix := range []string{"", ".meta"} {
		t.Run(suffix, func(t *testing.T) {
			s, path, _ := cookedLargeClipFixture(t, "")
			f, err := os.OpenFile(path+suffix, os.O_WRONLY, 0)
			if err != nil {
				t.Fatal(err)
			}
			if _, err = f.WriteAt([]byte("X"), 20); err != nil {
				t.Fatal(err)
			}
			if err = f.Close(); err != nil {
				t.Fatal(err)
			}
			s.observed = map[string]identity{}
			var needs *NeedsImportError
			if _, err := s.nativeReferenceGUIDs(path); !errors.As(err, &needs) {
				t.Fatalf("changed cooked source or metadata accepted: %v", err)
			}
			if len(s.files[path].content) != 0 || len(s.documents) != 0 {
				t.Fatal("rejected large source was parsed/cached")
			}
		})
	}
}

func TestCookedLargeNativeDiscoveryRequiresExactCurrentCookedProof(t *testing.T) {
	cases := map[string]func(*Session, string, string){
		"uncooked":               func(s *Session, _, _ string) { s.catalog.CookedRegistryVerified = false },
		"source hash omitted":    func(s *Session, _, _ string) { s.catalog.SourceDependencies = s.catalog.SourceDependencies[1:] },
		"metadata hash omitted":  func(s *Session, _, _ string) { s.catalog.SourceDependencies = s.catalog.SourceDependencies[:1] },
		"source size mismatches": func(s *Session, _, _ string) { s.catalog.SourceDependencies[0].Bytes++ },
		"native identity absent": func(s *Session, _, _ string) { s.catalog.Assets = nil },
		"wrong native type":      func(s *Session, _, _ string) { s.catalog.Assets[0]["typeName"] = "UnityEngine.Mesh" },
		"unbound packs":          func(s *Session, _, _ string) { delete(s.baseline, "importedAssetPacks") },
		"cooked pack changed":    func(s *Session, _, pack string) { writeFixtureFile(t, pack, "changed native cooked artifact") },
		"cooked pack same-size tamper": func(s *Session, _, pack string) {
			f, err := os.OpenFile(pack, os.O_WRONLY, 0)
			if err != nil {
				t.Fatal(err)
			}
			if _, err = f.WriteAt([]byte("X"), 0); err != nil {
				t.Fatal(err)
			}
			f.Close()
		},
		"baseline masks catalog revision": func(s *Session, _, _ string) { s.catalog.SourceDependencies[0].SHA256 = strings.Repeat("0", 64) },
	}
	for name, change := range cases {
		t.Run(name, func(t *testing.T) {
			s, path, pack := cookedLargeClipFixture(t, "")
			change(s, path, pack)
			if _, err := s.nativeReferenceGUIDs(path); err == nil {
				t.Fatal("oversized source accepted without exact current cooked proof")
			}
			if len(s.files[path].content) != 0 || len(s.documents) != 0 {
				t.Fatal("invalid large proof caused full text/YAML allocation")
			}
		})
	}
}

func TestCookedLargeNativeDiscoveryNeverFallsBackToFullYAML(t *testing.T) {
	s, path, _ := cookedLargeClipFixture(t, "  pointer:\n    fileID: 4300000\n    guid: "+lateNativeGUID+"\n    type: 3\n")
	if _, err := s.nativeReferenceGUIDs(path); err == nil || !strings.Contains(err.Error(), "full YAML fallback is unsupported") {
		t.Fatalf("noncanonical oversized source triggered an unbounded parser: %v", err)
	}
	if len(s.files[path].content) != 0 || len(s.documents) != 0 {
		t.Fatal("fallback allocated or cached oversized YAML")
	}
}

func TestCookedLargeNativeDiscoveryRetainsAbsoluteStreamingBound(t *testing.T) {
	s, path, _ := cookedLargeClipFixture(t, "")
	if err := os.Truncate(path, maxCookedNativeDiscoveryBytes+1); err != nil {
		t.Fatal(err)
	}
	if _, err := s.nativeReferenceGUIDs(path); err == nil {
		t.Fatal("source larger than supported cooked streaming bound accepted")
	}
}
