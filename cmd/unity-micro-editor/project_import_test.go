package main

import (
	"context"
	"encoding/binary"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestProjectImportExtractionLinksNativeHashesAndIgnoresMetadataPaths(t *testing.T) {
	project := projectContextSource(t)
	output := filepath.Join(project, "Temp/extraction-context")
	prepared, err := CreateProjectImportContext(context.Background(), project, output, []string{modelSource})
	if err != nil {
		t.Fatal(err)
	}
	store := filepath.Join(prepared.Project, "Library/DataStore")
	if err := os.Mkdir(store, 0700); err != nil {
		t.Fatal(err)
	}
	content := []byte("arbitrary native artifact fixture\x00")
	hash := nativeSourceHash(content)
	storage := make([]byte, 32)
	binary.LittleEndian.PutUint32(storage[:4], 3)
	binary.LittleEndian.PutUint64(storage[24:32], uint64(len(content)))
	database, err := buildProjectLMDB(map[string][]LMDBRecord{
		"StorageInfoTable": {{Key: hash[:], Value: storage}}, "ContentDataTable": {{Key: hash[:], Value: content}},
	})
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(store, "uds.db"), database, 0600); err != nil {
		t.Fatal(err)
	}
	report := &probeReport{WorkerExit: "exit 0", Imports: []importEvidence{{
		GUID: prepared.Assets[0].GUID, ResultID: strings.Repeat("a", 32), Metadata: ArtifactMetaInfo{
			Files: []ArtifactFile{{Name: "../../escape", Hash: hash}},
		},
	}}}
	extracted, err := extractProjectImportContent(context.Background(), output, prepared, report)
	if err != nil {
		t.Fatal(err)
	}
	file := extracted.Assets[0].Files[0]
	if !extracted.SourceUnchanged || file.Name != "../../escape" || !withinDirectory(file.Path, filepath.Join(output, "artifacts")) ||
		!file.Proof.ContentKeyVerified || file.SHA256 != bytesSHA(content) || file.Bytes != uint64(len(content)) {
		t.Fatalf("native linkage or generated output path changed: %+v", extracted)
	}
	if _, err := os.Stat(filepath.Join(project, "escape")); !os.IsNotExist(err) {
		t.Fatal("untrusted native filename became a filesystem destination")
	}
}

func TestProjectImportExtractionRejectsIncompleteShutdown(t *testing.T) {
	prepared := ProjectImportContext{Assets: []ProjectImportSource{{Path: modelSource}}}
	for _, report := range []*probeReport{
		{WorkerExit: "signal: killed", Imports: []importEvidence{{GUID: "guid"}}},
		{WorkerExit: "exit 0", Imports: nil},
	} {
		if _, err := extractProjectImportContent(context.Background(), t.TempDir(), prepared, report); err == nil {
			t.Fatal("incomplete worker job or forced shutdown accepted")
		}
	}
}
