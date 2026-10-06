package goproject

import (
	"bufio"
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"syscall"
	"testing"
)

func scanNativeText(text string) ([]string, bool, error) {
	return scanNativeReferenceGUIDs(bufio.NewReaderSize(strings.NewReader(text), nativeReferenceLineBytes))
}

func TestNativeReferenceScannerDistinguishesPPtrsFromGUIDText(t *testing.T) {
	a, b := "22222222222222222222222222222222", "33333333333333333333333333333333"
	text := "--- !u!114 &11400000\nMonoBehaviour:\n" +
		"  local: {fileID: 15}\n" +
		"  null: {fileID: 0, guid: " + a + ", type: 3}\n" +
		"  identifier: " + a + "\n" +
		"  guidOnly: {guid: " + a + ", type: 3}\n" +
		"  double: \"{fileID: 1, guid: " + a + ", type: 3}\"\n" +
		"  single: '{fileID: 1, guid: " + a + ", type: 3}'\n" +
		"  plain: label {fileID:1,guid:" + a + ",type:3}\n" +
		"  quotedEscapes: 'don''t treat {fileID: 1, guid: " + a + "} as data'\n" +
		"  literal: |\n    {fileID: 1, guid: " + a + ", type: 3}\n" +
		"  folded: >-\n    {fileID: 1, guid: " + a + ", type: 3}\n" +
		"  real: {fileID: -9223372036854775808, guid: " + b + ", type: 2}\n" +
		"  duplicate: {fileID: 4300000, guid: " + b + ", type: 3} # comment\n" +
		"  list: [{nested: {fileID: 43, guid: " + b + ", type: 3}}, {guid: " + a + "}]\n" +
		"  ids:\n  - 123\n  - 456\n"
	guids, fallback, err := scanNativeText(text)
	if err != nil || fallback || !reflect.DeepEqual(guids, []string{b}) {
		t.Fatalf("canonical PPtr selection = %v fallback=%t err=%v", guids, fallback, err)
	}
	// Compare with the existing loader traversal, including nested flow maps.
	docs, err := ParseUnityYAML([]byte(text))
	if err != nil {
		t.Fatal(err)
	}
	expected := map[string]bool{}
	for _, doc := range docs {
		collectNativeGUIDs(doc.Data, expected)
	}
	if !reflect.DeepEqual(guids, sortedNativeGUIDs(expected)) {
		t.Fatal("scanner differs from normal YAML traversal")
	}
}

func TestNativeReferenceScannerFallbackKeepsBlockPointersAndSchemaErrors(t *testing.T) {
	s, _, _ := fixtureSession(t)
	path := filepath.Join(s.project, "Assets/Custom.asset")
	guid := "22222222222222222222222222222222"
	cases := map[string]string{
		"block pointer": "  pointer:\n    fileID: 4300000\n    guid: " + guid + "\n    type: 3\n",
		"wrapped flow":  "  pointer: {fileID: 4300000,\n    guid: " + guid + ", type: 3}\n",
		"quoted field":  "  pointer: {\"fileID\": 4300000, guid: " + guid + ", type: 3}\n",
	}
	for name, body := range cases {
		t.Run(name, func(t *testing.T) {
			writeFixtureFile(t, path, "--- !u!114 &11400000\nMonoBehaviour:\n"+body)
			s.observed = map[string]identity{}
			refs, err := s.nativeReferenceGUIDs(path)
			if err != nil || !reflect.DeepEqual(refs, []string{guid}) {
				t.Fatalf("custom PPtr lost: %v %v", refs, err)
			}
			if len(s.documents) != 0 {
				t.Fatal("fallback seeded full YAML document cache")
			}
		})
	}
	writeFixtureFile(t, path, "--- !u!114 &11400000\nMonoBehaviour:\n  pointer:\n    fileID: 4\n    fileID: 5\n    guid: "+guid+"\n")
	s.observed = map[string]identity{}
	if _, err := s.nativeReferenceGUIDs(path); err == nil {
		t.Fatal("fallback accepted duplicate pointer keys")
	}
	writeFixtureFile(t, path, "--- !u!114 &11400000\nMonoBehaviour:\n  pointer:\n    fileID: 9223372036854775808\n    guid: "+guid+"\n")
	s.observed = map[string]identity{}
	if _, err := s.nativeReferenceGUIDs(path); err == nil {
		t.Fatal("fallback rounded oversized signed fileID")
	}
}

func TestNativeReferenceDiscoveryDoesNotCacheLargeBuffersAndKeepsCurrentHash(t *testing.T) {
	s, _, _ := fixtureSession(t)
	path := filepath.Join(s.project, "Assets/Large.asset")
	guid := "33333333333333333333333333333333"
	content := []byte("--- !u!43 &4300000\nMesh:\n  _typelessdata: " + strings.Repeat("00", 2<<20) +
		"\n  glyphs:\n" + strings.Repeat("  - index: 123\n    value: {x: 1, y: 2}\n", 10000) +
		"  reference: {fileID: 4300000, guid: " + guid + ", type: 3}\n")
	writeFixtureFile(t, path, string(content))
	refs, err := s.nativeReferenceGUIDs(path)
	if err != nil || !reflect.DeepEqual(refs, []string{guid}) {
		t.Fatalf("bounded discovery failed: %v %v", refs, err)
	}
	digest := sha256.Sum256(content)
	if s.files[path].digest != hex.EncodeToString(digest[:]) {
		t.Fatal("discovery did not hash the complete large scalar")
	}
	if len(s.files[path].content) != 0 || len(s.documents) != 0 {
		t.Fatal("discovery cached mesh/glyph content")
	}
	writeFixtureFile(t, path, strings.Replace(string(content), guid, "44444444444444444444444444444444", 1))
	if _, err := s.nativeReferenceGUIDs(path); err == nil {
		t.Fatal("changed source during one scan pass accepted")
	}
	s.observed = map[string]identity{}
	refs, err = s.nativeReferenceGUIDs(path)
	if err != nil || !reflect.DeepEqual(refs, []string{"44444444444444444444444444444444"}) {
		t.Fatalf("source revision not refreshed: %v %v", refs, err)
	}
}

func TestNativeDiscoveryIgnoresMissingNullAndUnrelatedGUIDs(t *testing.T) {
	s, scene, _ := fixtureSession(t)
	native := filepath.Join(s.project, "Assets/Unrelated.fbx")
	guid := "22222222222222222222222222222222"
	writeFixtureFile(t, native, "native model")
	writeFixtureFile(t, native+".meta", "guid: "+guid+"\n")
	writeFixtureFile(t, scene, fixtureScene+"--- !u!114 &90\nMonoBehaviour:\n  missing: {fileID: 4300000, guid: 33333333333333333333333333333333, type: 3}\n  null: {fileID: 0, guid: "+guid+", type: 3}\n  guidOnly: {guid: "+guid+", type: 3}\n  text: \"{fileID: 4300000, guid: "+guid+", type: 3}\"\n")
	refs, err := s.PendingNativeImportSources()
	if err != nil || len(refs) != 0 {
		t.Fatalf("unrelated/missing GUID selected for import: %v %v", refs, err)
	}
	if len(s.documents) != 0 {
		t.Fatal("discovery seeded parsed scene cache")
	}
}

func BenchmarkNativeReferenceScanLargeScalar(b *testing.B) {
	path := filepath.Join(b.TempDir(), "Mesh.asset")
	data := []byte("--- !u!43 &4300000\nMesh:\n  _typelessdata: " + strings.Repeat("00", 4<<20) +
		"\n  glyphs:\n" + strings.Repeat("  - index: 123\n    value: {x: 1, y: 2}\n", 10000) +
		"  reference: {fileID: 43, guid: 22222222222222222222222222222222, type: 3}\n")
	if err := os.WriteFile(path, data, 0600); err != nil {
		b.Fatal(err)
	}
	s := &Session{files: map[string]fileRecord{}, observed: map[string]identity{}}
	b.ReportAllocs()
	b.SetBytes(int64(len(data)))
	b.ResetTimer()
	for index := 0; index < b.N; index++ {
		if refs, err := s.nativeReferenceGUIDs(path); err != nil || len(refs) != 1 {
			b.Fatalf("%v %v", refs, err)
		}
	}
}

func TestNativeReferenceScannerBinaryAndUnterminatedLastLine(t *testing.T) {
	for _, source := range []string{"\x00binary.asset", "--- !u!43 &4300000\nMesh:\n  _typelessdata: 00"} {
		refs, fallback, err := scanNativeReferenceGUIDs(bufio.NewReaderSize(bytes.NewBufferString(source), nativeReferenceLineBytes))
		if err != nil || fallback || len(refs) != 0 {
			t.Fatalf("empty/binary EOF: %v %t %v", refs, fallback, err)
		}
	}
}

func TestNativeDiscoveryKeepsNormalPrepareSchemaValidation(t *testing.T) {
	s, scene, output := fixtureSession(t)
	writeFixtureFile(t, scene, strings.Replace(fixtureScene, "  m_Name: Original", "  m_Name: Original\n  m_Name: Duplicate", 1))
	if _, err := s.PendingNativeImportSources(); err != nil {
		t.Fatalf("discovery should only collect references: %v", err)
	}
	if _, err := s.Prepare(output); err == nil || !strings.Contains(err.Error(), "duplicate YAML key") {
		t.Fatalf("normal preparation lost schema validation: %v", err)
	}
}

func TestNativeReferenceSourceBoundsRejectFIFOAndOversizeBeforeReading(t *testing.T) {
	s, _, _ := fixtureSession(t)
	pipe := filepath.Join(s.project, "source.fifo")
	if err := syscall.Mkfifo(pipe, 0600); err != nil {
		t.Fatal(err)
	}
	if _, err := s.nativeReferenceGUIDs(pipe); err == nil {
		t.Fatal("nonregular source accepted")
	}
	large := filepath.Join(s.project, "oversize.asset")
	writeFixtureFile(t, large, "--- !u!43 &4300000\nMesh:\n")
	if err := os.Truncate(large, maxTextBytes+1); err != nil {
		t.Fatal(err)
	}
	if _, err := s.nativeReferenceGUIDs(large); err == nil {
		t.Fatal("oversize source accepted")
	}
}
