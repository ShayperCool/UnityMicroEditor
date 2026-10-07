package main

import (
	"bytes"
	"encoding/binary"
	"encoding/hex"
	"os"
	"path/filepath"
	"testing"
)

func fixtureSource(t *testing.T) string {
	t.Helper()
	project := t.TempDir()
	source := filepath.Join(project, filepath.FromSlash(modelSource))
	if err := os.MkdirAll(filepath.Dir(source), 0755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(source, []byte("Kaydara FBX Binary  \x00fixture"), 0644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(source+".meta", []byte("fileFormatVersion: 2\nguid: b1d15452ab70b6a669e9b8e3c0b91251\nModelImporter:\n  meshes:\n    globalScale: 1\n"), 0644); err != nil {
		t.Fatal(err)
	}
	return project
}

func TestFixtureCopiesExactFBXWithoutChangingSource(t *testing.T) {
	project := fixtureSource(t)
	output := filepath.Join(project, "Temp/worker/fixture")
	fixture, err := CreateFixture(project, output, false)
	if err != nil {
		t.Fatal(err)
	}
	if fixture.ModelGUID != "b1d15452ab70b6a669e9b8e3c0b91251" || fixture.AudioGUID != audioGUID || fixture.ReferenceIncluded {
		t.Fatalf("unexpected identities: %+v", fixture)
	}
	for source, hash := range fixture.SourceHashes {
		data, err := os.ReadFile(source)
		if err != nil || bytesSHA(data) != hash {
			t.Fatalf("source changed: %s", source)
		}
	}
	for path, hash := range fixture.InputHashes {
		data, err := os.ReadFile(filepath.Join(fixture.Project, path))
		if err != nil || bytesSHA(data) != hash {
			t.Fatalf("fixture identity differs: %s", path)
		}
	}
	for _, directory := range []string{"Library", "Temp", "Logs"} {
		entries, err := os.ReadDir(filepath.Join(fixture.Project, directory))
		if err != nil || len(entries) != 0 {
			t.Fatalf("fixture directory should be empty: %s", directory)
		}
	}
	if _, err = os.Stat(filepath.Join(fixture.Project, "Assets/Editor")); !os.IsNotExist(err) {
		t.Fatal("reference helper unexpectedly included")
	}
	before, _ := os.ReadFile(filepath.Join(output, "fixture.json"))
	if _, err = CreateFixture(project, output, false); err == nil {
		t.Fatal("duplicate fixture was silently overwritten")
	}
	after, _ := os.ReadFile(filepath.Join(output, "fixture.json"))
	if !bytes.Equal(before, after) {
		t.Fatal("duplicate attempt changed existing report")
	}
}

func TestFixtureRejectsOutputEscape(t *testing.T) {
	project := fixtureSource(t)
	outside := t.TempDir()
	if _, err := CreateFixture(project, filepath.Join(outside, "fixture"), false); err == nil {
		t.Fatal("accepted output outside Temp")
	}
	if err := os.Mkdir(filepath.Join(project, "Temp"), 0755); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(outside, filepath.Join(project, "Temp/escape")); err != nil {
		t.Fatal(err)
	}
	if _, err := CreateFixture(project, filepath.Join(project, "Temp/escape/new"), false); err == nil {
		t.Fatal("accepted symlink escaping Temp")
	}
	if _, err := os.Stat(filepath.Join(outside, "new")); !os.IsNotExist(err) {
		t.Fatal("rejected fixture still wrote outside Temp")
	}
}

func TestFixtureWaveHasExactPCMAndNoLossyImport(t *testing.T) {
	guid, err := hex.DecodeString(audioGUID)
	if err != nil || len(audioGUID) != 32 || len(guid) != 16 {
		t.Fatal("AudioImporter GUID must be exactly 128 bits")
	}
	data := fixtureWAV()
	if len(data) != 44+4096*4 || string(data[:4]) != "RIFF" || string(data[8:16]) != "WAVEfmt " || string(data[36:40]) != "data" {
		t.Fatal("invalid PCM RIFF layout")
	}
	if binary.LittleEndian.Uint16(data[20:22]) != 1 || binary.LittleEndian.Uint16(data[22:24]) != 2 || binary.LittleEndian.Uint32(data[24:28]) != 22050 || binary.LittleEndian.Uint16(data[34:36]) != 16 {
		t.Fatal("unexpected WAV format")
	}
	for _, index := range []int{0, 1, 1024, 4095} {
		left := int16(binary.LittleEndian.Uint16(data[44+index*4 : 46+index*4]))
		right := int16(binary.LittleEndian.Uint16(data[46+index*4 : 48+index*4]))
		if int(left) != (index*173+7919)%49152-24576 || int(right) != (index*307+3571)%40960-20480 {
			t.Fatalf("PCM sample differs at frame %d", index)
		}
	}
	meta := fixtureAudioMeta()
	for _, required := range []string{"compressionFormat: 0", "loadType: 0", "forceToMono: 0", "normalize: 0"} {
		if !bytes.Contains(meta, []byte(required)) {
			t.Fatalf("missing exact PCM importer setting %s", required)
		}
	}
}
