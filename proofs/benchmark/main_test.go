package main

import (
	"bytes"
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
	"time"
)

func TestBenchmarkEditAddsActualMethodWithoutChangingExistingCode(t *testing.T) {
	original := []byte("class AmbientSound {\n        private AudioService _audio;\n}\n")
	modified, err := makeBenchmarkEdit(original, 123456)
	if err != nil || bytes.Equal(modified, original) || !bytes.Contains(modified, []byte("public static int __UnityPlayBenchmarkRevision() => 123456;")) {
		t.Fatalf("benchmark did not emit real code: %s %v", modified, err)
	}
	if _, err := makeBenchmarkEdit(modified, 654321); err == nil {
		t.Fatal("old benchmark method accepted as original user source")
	}
	filename := filepath.Join(t.TempDir(), "AmbientSound.cs")
	if err := os.WriteFile(filename, modified, 0600); err != nil {
		t.Fatal(err)
	}
	if err := restoreBenchmarkSource(filename, original, modified); err != nil {
		t.Fatal(err)
	}
	data, _ := os.ReadFile(filename)
	if !bytes.Equal(data, original) {
		t.Fatal("original bytes were not restored")
	}
	if err := os.WriteFile(filename, []byte("another user's edit"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := restoreBenchmarkSource(filename, original, modified); err == nil {
		t.Fatal("restore overwrote a concurrent user edit")
	}
}

func TestEventTailWaitsForCompleteRelevantLines(t *testing.T) {
	filename := filepath.Join(t.TempDir(), "events.jsonl")
	if err := os.WriteFile(filename, []byte(`{"event":"code-updated","paths":["/other"]}`+"\n"+`{"event":"code-updated","paths":["/source"]`), 0600); err != nil {
		t.Fatal(err)
	}
	file, err := os.Open(filename)
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	tail := &eventTail{file: file}
	events, err := tail.next()
	if err != nil || len(events) != 1 || len(tail.pending) == 0 {
		t.Fatalf("partial JSONL line was consumed: %+v %v", events, err)
	}
	appendFile, err := os.OpenFile(filename, os.O_APPEND|os.O_WRONLY, 0600)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := appendFile.WriteString(`,"pid":7}` + "\n"); err != nil {
		t.Fatal(err)
	}
	appendFile.Close()
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	event, err := tail.waitCodeUpdate(ctx, "/source")
	if err != nil || event.PID != 7 {
		t.Fatalf("did not read completed relevant event: %+v %v", event, err)
	}
}

func TestDryRunCreatesBackupWithoutEditingGameSource(t *testing.T) {
	project := t.TempDir()
	filename := filepath.Join(project, filepath.FromSlash(benchmarkSource))
	if err := os.MkdirAll(filepath.Dir(filename), 0700); err != nil {
		t.Fatal(err)
	}
	original := []byte("class AmbientSound {\n        private AudioService _audio;\n}\n")
	if err := os.WriteFile(filename, original, 0600); err != nil {
		t.Fatal(err)
	}
	report, err := runBenchmark(context.Background(), benchmarkOptions{Project: project, Output: filepath.Join(project, "Temp/prepared")})
	if err != nil || !report.Success || report.Stage != "prepared" || len(report.Measurements) != 0 {
		t.Fatalf("dry run did not only prepare backup: %+v %v", report, err)
	}
	actual, _ := os.ReadFile(filename)
	backup, _ := os.ReadFile(report.Backup)
	if !bytes.Equal(actual, original) || !bytes.Equal(backup, original) {
		t.Fatal("dry run changed original bytes or backup")
	}
}

func TestRestoreMatchesCompiledSourceProofEvenWhenAnotherFileTriggeredBatch(t *testing.T) {
	root := t.TempDir()
	source := filepath.Join(root, filepath.FromSlash(benchmarkSource))
	expected := fingerprint([]byte("original source"))
	dll := filepath.Join(root, "Dungeon.Runtime.dll")
	if err := os.WriteFile(dll, []byte("restored assembly without benchmark method"), 0600); err != nil {
		t.Fatal(err)
	}
	manifest := filepath.Join(root, "scene.json")
	manifestBytes, _ := json.Marshal(map[string]any{"lifecycleBarrierVersion": 1, "assemblies": []string{dll}, "lifecycleBarrierAssemblies": []string{dll}})
	if err := os.WriteFile(manifest, manifestBytes, 0600); err != nil {
		t.Fatal(err)
	}
	filename := filepath.Join(root, "events.jsonl")
	eventBytes, _ := json.Marshal(watchEvent{Event: "code-updated", Paths: []string{"/unrelated/CLIPlay.cs"},
		Manifest: manifest, PID: 17, SourceHashes: map[string]string{source: expected}})
	if err := os.WriteFile(filename, append(eventBytes, '\n'), 0600); err != nil {
		t.Fatal(err)
	}
	file, err := os.Open(filename)
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	event, err := (&eventTail{file: file}).waitCodeMarker(ctx, source, expected, []byte(benchmarkMethod), false)
	if err != nil || event.PID != 17 {
		t.Fatalf("missed actual restored-source compilation because paths only held another trigger: %+v %v", event, err)
	}
	event.SourceHashes[source] = fingerprint([]byte("different source"))
	if eventSourceHash(event, source) == expected {
		t.Fatal("different compiler snapshot accepted as restoration proof")
	}
}

func TestBenchmarkBodyEditOnlyChangesExistingCoroutineConstant(t *testing.T) {
	original := []byte("class AmbientSound { private IEnumerator OneShots() { yield return new WaitForSeconds(Random.Range(0.3f, 1f)); } }")
	modified, marker, err := makeBenchmarkBodyEdit(original)
	if err != nil || bytes.Equal(original, modified) || bytes.Contains(modified, []byte(benchmarkMethod)) || len(marker) != 5 || marker[0] != 0x22 {
		t.Fatalf("did not emit a private body edit with actual ldc.r4 proof: %s %x %v", modified, marker, err)
	}
	expected := bytes.Replace(original, []byte("0.3f"), []byte("0.30000004f"), 1)
	if !bytes.Equal(expected, modified) {
		t.Fatal("body benchmark changed other authored source")
	}
	if _, _, err := makeBenchmarkBodyEdit(modified); err == nil {
		t.Fatal("body benchmark accepted an already modified original")
	}
}
