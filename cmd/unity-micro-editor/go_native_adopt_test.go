package main

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"

	goproject "unitymicroeditor/internal/project"
)

func TestNativeAdoptionRequiresImportedSourceMetadataAndGUIDRevision(t *testing.T) {
	opts, _, replacements := goNativeReplayFixture(t)
	importer := &nativeProjectImporter{opts: opts, environmentSHA256: replacements[0].EnvironmentSHA256}
	prepared := ProjectImportContext{SourceHashes: map[string]string{}}
	for _, replacement := range replacements {
		physical, err := resolveProjectImportSource(opts.Project, replacement.SourcePath)
		if err != nil {
			t.Fatal(err)
		}
		prepared.Assets = append(prepared.Assets, ProjectImportSource{Path: replacement.SourcePath, GUID: replacement.GUID})
		prepared.SourceHashes[physical], prepared.SourceHashes[physical+".meta"] = replacement.SourceSHA256, replacement.MetaSHA256
	}
	if inputs, err := adoptionInputs(importer, prepared); err != nil || len(inputs) != len(replacements) {
		t.Fatalf("exact imported inputs rejected: %v", err)
	}
	physical := filepath.Join(opts.Project, replacements[0].SourcePath)
	for _, filename := range []string{physical, physical + ".meta"} {
		original := prepared.SourceHashes[filename]
		prepared.SourceHashes[filename] = strings.Repeat("0", 64)
		if _, err := adoptionInputs(importer, prepared); err == nil {
			t.Fatalf("changed imported source hash accepted: %s", filename)
		}
		prepared.SourceHashes[filename] = original
	}
	prepared.Assets[0].GUID = strings.Repeat("f", 32)
	if _, err := adoptionInputs(importer, prepared); err == nil {
		t.Fatal("changed source GUID accepted")
	}
	prepared.Assets[0].GUID = replacements[0].GUID
	prepared.Assets = append(prepared.Assets, prepared.Assets[0])
	if _, err := adoptionInputs(importer, prepared); err == nil {
		t.Fatal("duplicate imported source accepted")
	}
}

func TestNativeAdoptionRejectsWrongWorkerEnvironmentBeforePublishing(t *testing.T) {
	opts, session, replacements := goNativeReplayFixture(t)
	importer := &nativeProjectImporter{opts: opts, session: session, entries: map[string]goproject.NativeReplacement{}, environmentSHA256: replacements[0].EnvironmentSHA256}
	environment, err := readWorkerEnvironment(opts.Project)
	if err != nil {
		t.Fatal(err)
	}
	for _, name := range []string{"precision", "textureOverrides", "workerFailure", "editorStarted"} {
		t.Run(name, func(t *testing.T) {
			copy := *environment
			report := &probeReport{Environment: &copy, WorkerExit: "exit 0", Success: true}
			switch name {
			case "precision":
				copy.ShaderPrecisionModel ^= 1
			case "textureOverrides":
				copy.TextureMaxSizeOverride = 1024
			case "workerFailure":
				report.WorkerExit = "exit 1"
			case "editorStarted":
				report.MainEditorStarted = true
			}
			err := importer.adoptColdReport(context.Background(), report, opts.Output, "result.json", nil, nil)
			if err == nil || !strings.Contains(err.Error(), "current standalone worker environment") || len(importer.entries) != 0 {
				t.Fatalf("invalid environment reached native publication: %v", err)
			}
		})
	}
}

func TestNativeAdoptionRejectsLegacyReportEnvironmentAndSymlinkArtifacts(t *testing.T) {
	project := t.TempDir()
	root := filepath.Join(project, "Temp", "import")
	if err := os.MkdirAll(root, 0700); err != nil {
		t.Fatal(err)
	}
	reportPath := filepath.Join(root, "result.json")
	for _, omitted := range []string{"textureMaxSizeOverride", "textureCompressionOverride", "defaultTextureCompressionFormat"} {
		environment := map[string]uint32{"textureMaxSizeOverride": 0, "textureCompressionOverride": 0, "defaultTextureCompressionFormat": 0}
		delete(environment, omitted)
		data, _ := json.Marshal(map[string]any{"environment": environment})
		if err := os.WriteFile(reportPath, data, 0600); err != nil {
			t.Fatal(err)
		}
		if _, _, _, err := readColdAdoptionReport(context.Background(), project, reportPath); err == nil || !strings.Contains(err.Error(), "complete v2") {
			t.Fatalf("legacy worker report with omitted %s accepted: %v", omitted, err)
		}
	}
	alias := filepath.Join(root, "alias.json")
	if err := os.Symlink(reportPath, alias); err != nil {
		t.Fatal(err)
	}
	if err := validateAdoptionFile(alias, root, maxArtifactBytes); err == nil {
		t.Fatal("symlink artifact accepted")
	}
	if err := validateAdoptionFile(reportPath, filepath.Join(root, "artifacts"), maxArtifactBytes); err == nil {
		t.Fatal("artifact outside report ownership accepted")
	}
}

// Replays actual stopped-worker UDS content; no native process is started.
func TestNativeAdoptionActualStoppedBatchProvesEveryArtifact(t *testing.T) {
	project, reportPath := os.Getenv("UNITY_WORKER_UI_PROJECT"), os.Getenv("UNITY_WORKER_BATCH_IMPORT_REPORT")
	if project == "" || reportPath == "" {
		t.Skip("set UNITY_WORKER_UI_PROJECT and UNITY_WORKER_BATCH_IMPORT_REPORT for actual batch artifact evidence")
	}
	report, prepared, content, err := readColdAdoptionReport(context.Background(), project, reportPath)
	if err != nil {
		t.Fatal(err)
	}
	if len(content.Assets) != 176 || len(prepared.Assets) != 176 || report.MainEditorStarted || report.WorkerExit != "exit 0" {
		t.Fatalf("unexpected actual batch report: assets=%d workerExit=%s", len(content.Assets), report.WorkerExit)
	}
	t.Logf("verified %d actual native artifacts; original worker %.6fs, peak RSS %dKiB; no worker launched by test", len(content.Assets), report.ElapsedSeconds, report.WorkerPeakRSSKiB)
}
