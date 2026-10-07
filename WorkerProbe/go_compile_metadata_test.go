package main

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

func TestGoMetadataGUIDIndexStreamsLargeModelSettings(t *testing.T) {
	guid := "A1000000000000000000000000000001"
	// The colliding text starts at a reader-fragment boundary inside one
	// importer line. Neither this nor an indented real PPtr is an asset GUID.
	longLine := "  curve: " + strings.Repeat("x", goMetadataGUIDBufferSize-len("  curve: ")) +
		"guid: b1000000000000000000000000000001" + strings.Repeat("y", 2*1024*1024)
	data := []byte("fileFormatVersion: 2\r\nguid: " + guid + "\r\nModelImporter:\r\n" + longLine +
		"\r\n  lastHumanDescriptionAvatarSource: {fileID: 9000000, guid: c1000000000000000000000000000001, type: 3}\r\n")
	filename := writeGoGraphFile(t, t.TempDir(), "Assets/Model.fbx.meta", string(data))
	before, _ := os.Stat(filename)
	identity, err := newGoMetadataGUIDIndex().read(filename)
	if err != nil {
		t.Fatal(err)
	}
	hash := sha256.Sum256(data)
	if identity.GUID != strings.ToLower(guid) || identity.SHA256 != hex.EncodeToString(hash[:]) || identity.Bytes != int64(len(data)) {
		t.Fatalf("incorrect complete-file identity: %+v", identity)
	}
	after, _ := os.Stat(filename)
	unchanged, _ := os.ReadFile(filename)
	if !bytes.Equal(data, unchanged) || !before.ModTime().Equal(after.ModTime()) {
		t.Fatal("GUID indexing changed authored importer metadata")
	}
}

func TestGoMetadataGUIDIndexRejectsUnsafeIdentitiesAndFiles(t *testing.T) {
	for _, kind := range []string{"duplicate-late", "malformed", "missing", "zero", "long-guid", "oversize", "symlink", "parent-symlink", "directory"} {
		t.Run(kind, func(t *testing.T) {
			project := t.TempDir()
			filename := writeGoGraphFile(t, project, "Assets/Model.fbx.meta", "guid: a1000000000000000000000000000001\nModelImporter:\n")
			switch kind {
			case "duplicate-late":
				writeGoGraphFile(t, project, "Assets/Model.fbx.meta", "guid: a1000000000000000000000000000001\n  settings: "+strings.Repeat("x", 2*1024*1024)+"\nguid: a1000000000000000000000000000002\n")
			case "malformed":
				writeGoGraphFile(t, project, "Assets/Model.fbx.meta", "guid: invalid\n")
			case "missing":
				writeGoGraphFile(t, project, "Assets/Model.fbx.meta", "ModelImporter:\n  reference: {guid: a1000000000000000000000000000001}\n")
			case "zero":
				writeGoGraphFile(t, project, "Assets/Model.fbx.meta", "guid: 00000000000000000000000000000000\n")
			case "long-guid":
				writeGoGraphFile(t, project, "Assets/Model.fbx.meta", "guid: "+strings.Repeat("a", goMetadataGUIDBufferSize)+"\n")
			case "oversize":
				file, err := os.OpenFile(filename, os.O_WRONLY, 0600)
				if err != nil {
					t.Fatal(err)
				}
				err = file.Truncate(goMetadataGUIDMaximum + 1)
				file.Close()
				if err != nil {
					t.Fatal(err)
				}
			case "symlink":
				filename = filepath.Join(project, "Assets/Alias.fbx.meta")
				if err := os.Symlink(filepath.Join(project, "Assets/Model.fbx.meta"), filename); err != nil {
					t.Fatal(err)
				}
			case "parent-symlink":
				if err := os.Symlink(filepath.Join(project, "Assets"), filepath.Join(project, "Alias")); err != nil {
					t.Fatal(err)
				}
				filename = filepath.Join(project, "Alias/Model.fbx.meta")
			case "directory":
				filename = filepath.Join(project, "Assets/Folder.meta")
				if err := os.Mkdir(filename, 0700); err != nil {
					t.Fatal(err)
				}
			}
			if identity, err := newGoMetadataGUIDIndex().read(filename); err == nil || identity != (goMetadataGUIDIdentity{}) {
				t.Fatalf("unsafe %s accepted: %+v %v", kind, identity, err)
			}
		})
	}
}

type goMetadataBrokenReader struct{}

func (goMetadataBrokenReader) Read([]byte) (int, error) { return 0, io.ErrUnexpectedEOF }

func TestGoMetadataGUIDIndexChecksCompleteInputAndCanReuseBuffer(t *testing.T) {
	index := newGoMetadataGUIDIndex()
	if _, err := index.scan(io.MultiReader(strings.NewReader("guid: a1000000000000000000000000000001\n"), goMetadataBrokenReader{})); err == nil {
		t.Fatal("read failure after a valid GUID was accepted")
	}
	for _, guid := range []string{"a1000000000000000000000000000001", "b1000000000000000000000000000002"} {
		identity, err := index.scan(strings.NewReader("guid: " + guid))
		if err != nil || identity.GUID != guid {
			t.Fatalf("reused scanner retained earlier GUID: %+v %v", identity, err)
		}
	}
	oversize := io.MultiReader(strings.NewReader("guid: a1000000000000000000000000000001\nModelImporter:\n  value: "), io.LimitReader(&goMetadataZeroReader{}, goMetadataGUIDMaximum))
	if _, err := index.scan(oversize); err == nil {
		t.Fatal("streaming source exceeded its hard byte bound")
	}
}

type goMetadataZeroReader struct{}

func (*goMetadataZeroReader) Read(buffer []byte) (int, error) {
	clear(buffer)
	return len(buffer), nil
}

func TestGoSourceGraphAdditionWithLargeMetadataChecksProjectWideGUIDCollisions(t *testing.T) {
	for _, collision := range []bool{false, true} {
		t.Run(map[bool]string{false: "unrelated-model", true: "colliding-model"}[collision], func(t *testing.T) {
			compiler := sourceExtensionFixture(t)
			source := writeGoGraphFile(t, compiler.project, "Assets/Game/Added.cs", "class Added {}")
			guid := "10000000000000000000000000000002"
			writeGoGraphFile(t, compiler.project, "Assets/Game/Added.cs.meta", "guid: "+guid+"\n")
			assetGUID := "30000000000000000000000000000001"
			if collision {
				assetGUID = guid
			}
			metadata := "fileFormatVersion: 2\r\nguid: " + assetGUID + "\r\nModelImporter:\r\n  extraUserProperties: " + strings.Repeat("x", 2*1024*1024) + "\r\n  nestedReference: {guid: " + guid + "}\r\n"
			writeGoGraphFile(t, compiler.project, "Assets/Model.fbx.meta", metadata)
			acceptedBytes, _ := json.Marshal(compiler.manifest)
			var accepted goCompileManifest
			if err := json.Unmarshal(acceptedBytes, &accepted); err != nil {
				t.Fatal(err)
			}
			candidate, err := compiler.sourceGraphCandidate([]string{source})
			if collision {
				if err == nil || candidate != nil || !strings.Contains(err.Error(), "collides with asset metadata") {
					t.Fatalf("missed large model GUID collision: %v", err)
				}
			} else if err != nil || candidate == nil || len(candidate.manifest.Scripts) != 2 {
				t.Fatalf("valid large model prevented new MonoScript discovery: %v", err)
			}
			if !reflect.DeepEqual(accepted, compiler.manifest) {
				t.Fatal("candidate scan changed the accepted compiler graph")
			}
		})
	}
}

func TestGoMetadataGUIDActualModelReadOnly(t *testing.T) {
	filename := os.Getenv("UNITY_GO_METADATA_PARITY_FILE")
	if filename == "" {
		t.Skip("set UNITY_GO_METADATA_PARITY_FILE for actual large Unity model metadata")
	}
	identity, err := newGoMetadataGUIDIndex().read(filename)
	if err != nil {
		t.Fatal(err)
	}
	if identity.Bytes <= 1024*1024 {
		t.Fatal("actual fixture does not exercise former 1 MiB metadata limit")
	}
	acceptedSHA, err := goFileHash(filename)
	if err != nil || acceptedSHA != identity.SHA256 {
		t.Fatal("actual metadata complete-file SHA differs")
	}
	t.Logf("read-only metadata GUID=%s bytes=%d sha256=%s", identity.GUID, identity.Bytes, identity.SHA256)
}

func TestGoMetadataGUIDActualProjectIndexReadOnly(t *testing.T) {
	project := os.Getenv("UNITY_GO_METADATA_PARITY_PROJECT")
	if project == "" {
		t.Skip("set UNITY_GO_METADATA_PARITY_PROJECT for actual project-wide GUID indexing")
	}
	index := newGoMetadataGUIDIndex()
	count, large := 0, 0
	var bytesRead int64
	err := filepath.WalkDir(filepath.Join(project, "Assets"), func(filename string, entry fs.DirEntry, err error) error {
		if err != nil || entry.IsDir() || !strings.HasSuffix(filename, ".meta") {
			return err
		}
		identity, err := index.read(filename)
		if err != nil {
			return err
		}
		count++
		bytesRead += identity.Bytes
		if identity.Bytes > 1024*1024 {
			large++
		}
		return nil
	})
	if err != nil {
		t.Fatal(err)
	}
	if count == 0 || large == 0 {
		t.Fatal("actual project does not exercise large asset metadata")
	}
	t.Logf("read-only project-wide metadata index files=%d largeFiles=%d bytes=%d", count, large, bytesRead)
}
