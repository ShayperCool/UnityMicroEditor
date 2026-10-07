package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"syscall"
	"testing"

	"unitymicroeditor/project/authoring"
)

func sourceCatalogFreezeFixture(t *testing.T) (project, directory, input string, content []byte) {
	t.Helper()
	project = t.TempDir()
	directory = filepath.Join(project, "Library/UnityMicroEditor/preparations/catalog-test")
	input = filepath.Join(t.TempDir(), "external-catalog.json")
	content = []byte("{\n  \"project\":\"unchanged-project\", \"backend\":\"original-native-backend\",\n  \"assets\":[{\"fileId\":-5824371076177060002,\"positive\":9223372036854775807,\"path\":\"Assets/<raw>&asset.prefab\"}]\n}\n")
	if err := os.WriteFile(input, content, 0600); err != nil {
		t.Fatal(err)
	}
	return
}

func TestFreezeAgentSourceCatalogPreservesIdentityAndReplays(t *testing.T) {
	project, directory, input, content := sourceCatalogFreezeFixture(t)
	ctx := context.Background()
	snapshot, err := freezeAgentSourceCatalog(ctx, project, directory, input)
	if err != nil || snapshot != filepath.Join(directory, "native-input-catalog.json") {
		t.Fatalf("external catalog was not frozen: %q %v", snapshot, err)
	}
	actual, err := os.ReadFile(snapshot)
	if err != nil || !bytes.Equal(actual, content) {
		t.Fatalf("catalog bytes/long numeric identities were altered: %s %v", actual, err)
	}
	var document map[string]json.RawMessage
	if err := json.Unmarshal(actual, &document); err != nil {
		t.Fatal(err)
	}
	if string(document["backend"]) != `"original-native-backend"` || string(document["project"]) != `"unchanged-project"` ||
		!bytes.Contains(document["assets"], []byte("-5824371076177060002")) || !bytes.Contains(document["assets"], []byte("9223372036854775807")) {
		t.Fatal("catalog provenance or subasset identity was relabeled")
	}
	var receipt agentSourceCatalogReceipt
	receiptPath := filepath.Join(directory, "native-input-receipt.json")
	if err := agentReadJSON(receiptPath, 1<<20, &receipt); err != nil {
		t.Fatal(err)
	}
	if receipt.SchemaVersion != 1 || receipt.Project != project || receipt.SourcePath != input || receipt.SnapshotPath != snapshot ||
		receipt.SourceSHA256 != bytesSHA(content) || receipt.SnapshotSHA256 != bytesSHA(actual) {
		t.Fatalf("snapshot lacks exact source/hash provenance: %+v", receipt)
	}
	before, err := os.Stat(snapshot)
	if err != nil {
		t.Fatal(err)
	}
	if replay, err := freezeAgentSourceCatalog(ctx, project, directory, input); err != nil || replay != snapshot {
		t.Fatalf("stable replay failed: %q %v", replay, err)
	}
	after, err := os.Stat(snapshot)
	if err != nil || !os.SameFile(before, after) || !before.ModTime().Equal(after.ModTime()) {
		t.Fatal("stable replay rewrote its immutable snapshot", err)
	}
	if original, err := os.ReadFile(input); err != nil || !bytes.Equal(original, content) {
		t.Fatal("external authored input was modified", err)
	}
	if owned, err := freezeAgentSourceCatalog(ctx, project, filepath.Join(project, "Library/UnityMicroEditor/unused"), snapshot); err != nil || owned != snapshot {
		t.Fatalf("owned catalog did not pass through: %q %v", owned, err)
	}
	if _, err := os.Stat(filepath.Join(project, "Library/UnityMicroEditor/unused")); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("owned pass-through created an unnecessary snapshot")
	}
}

func TestFreezeAgentSourceCatalogRejectsChangedRetryAndSnapshot(t *testing.T) {
	for _, damage := range []string{"source", "source-path", "snapshot", "receipt"} {
		t.Run(damage, func(t *testing.T) {
			project, directory, input, content := sourceCatalogFreezeFixture(t)
			snapshot, err := freezeAgentSourceCatalog(context.Background(), project, directory, input)
			if err != nil {
				t.Fatal(err)
			}
			path, replacement := input, []byte(`{"assets":[],"changed":true}`)
			switch damage {
			case "source-path":
				input = filepath.Join(t.TempDir(), "same-bytes-new-input.json")
				path, replacement = input, content
			case "snapshot":
				path = snapshot
			case "receipt":
				path, replacement = filepath.Join(directory, "native-input-receipt.json"), []byte("malformed receipt")
			}
			if err := os.WriteFile(path, replacement, 0600); err != nil {
				t.Fatal(err)
			}
			if _, err := freezeAgentSourceCatalog(context.Background(), project, directory, input); err == nil {
				t.Fatal("changed frozen source/snapshot identity was silently accepted")
			} else if damage != "receipt" {
				var coded *authoring.Error
				if !errors.As(err, &coded) || coded.Code != "request_id_conflict" {
					t.Fatalf("changed retry was not reported as a conflict: %v", err)
				}
			}
			if damage == "source" || damage == "source-path" {
				if actual, err := os.ReadFile(snapshot); err != nil || !bytes.Equal(actual, content) {
					t.Fatal("failed retry replaced the accepted snapshot", err)
				}
			}
		})
	}
}

func TestFreezeAgentSourceCatalogCompletesMatchingInterruptedSnapshot(t *testing.T) {
	project, directory, input, content := sourceCatalogFreezeFixture(t)
	snapshot := filepath.Join(directory, "native-input-catalog.json")
	if err := agentImmutableFile(snapshot, content); err != nil {
		t.Fatal(err)
	}
	if actual, err := freezeAgentSourceCatalog(context.Background(), project, directory, input); err != nil || actual != snapshot {
		t.Fatalf("matching interrupted snapshot could not finish its receipt: %q %v", actual, err)
	}
	if _, err := os.Stat(filepath.Join(directory, "native-input-receipt.json")); err != nil {
		t.Fatal(err)
	}
}

func TestFreezeAgentSourceCatalogRejectsMalformedOrUnsafeInputs(t *testing.T) {
	for _, damage := range []string{"malformed", "trailing", "null", "array", "invalid-utf8", "oversize", "directory", "symlink", "parent-symlink", "fifo", "relative"} {
		t.Run(damage, func(t *testing.T) {
			project, directory, input, _ := sourceCatalogFreezeFixture(t)
			var content []byte
			switch damage {
			case "malformed":
				content = []byte(`{"unfinished":`)
			case "trailing":
				content = []byte(`{} {}`)
			case "null":
				content = []byte(`null`)
			case "array":
				content = []byte(`[]`)
			case "invalid-utf8":
				content = []byte{'{', '"', 'x', '"', ':', '"', 0xff, '"', '}'}
			case "oversize":
				if err := os.Truncate(input, agentSourceCatalogMaximum+1); err != nil {
					t.Fatal(err)
				}
			case "directory", "symlink", "fifo":
				if err := os.Remove(input); err != nil {
					t.Fatal(err)
				}
				var err error
				if damage == "directory" {
					err = os.Mkdir(input, 0700)
				} else if damage == "symlink" {
					err = os.Symlink(filepath.Join(project, "linked-target.json"), input)
				} else {
					err = syscall.Mkfifo(input, 0600)
				}
				if err != nil {
					t.Fatal(err)
				}
			case "parent-symlink":
				linked := filepath.Join(t.TempDir(), "linked-parent")
				if err := os.Symlink(filepath.Dir(input), linked); err != nil {
					t.Fatal(err)
				}
				input = filepath.Join(linked, filepath.Base(input))
			case "relative":
				input = "catalog.json"
			}
			if content != nil {
				if err := os.WriteFile(input, content, 0600); err != nil {
					t.Fatal(err)
				}
			}
			if _, err := freezeAgentSourceCatalog(context.Background(), project, directory, input); err == nil {
				t.Fatal("malformed or unsafe catalog input accepted")
			}
			if _, err := os.Stat(filepath.Join(directory, "native-input-catalog.json")); !errors.Is(err, os.ErrNotExist) {
				t.Fatal("unsafe input published a snapshot")
			}
		})
	}
}

func TestFreezeAgentSourceCatalogRejectsForeignOrLinkedOutputs(t *testing.T) {
	for _, damage := range []string{"foreign", "linked-parent", "linked-snapshot", "linked-receipt", "canceled"} {
		t.Run(damage, func(t *testing.T) {
			project, directory, input, _ := sourceCatalogFreezeFixture(t)
			ctx := context.Background()
			if damage == "foreign" {
				directory = t.TempDir()
			} else if damage == "canceled" {
				var cancel context.CancelFunc
				ctx, cancel = context.WithCancel(ctx)
				cancel()
			} else if damage == "linked-parent" {
				if err := os.MkdirAll(filepath.Dir(directory), 0700); err != nil {
					t.Fatal(err)
				}
				if err := os.Symlink(t.TempDir(), directory); err != nil {
					t.Fatal(err)
				}
			} else {
				if err := os.MkdirAll(directory, 0700); err != nil {
					t.Fatal(err)
				}
				name := "native-input-catalog.json"
				if damage == "linked-receipt" {
					name = "native-input-receipt.json"
				}
				if err := os.Symlink(input, filepath.Join(directory, name)); err != nil {
					t.Fatal(err)
				}
			}
			if _, err := freezeAgentSourceCatalog(ctx, project, directory, input); err == nil {
				t.Fatal("unsafe snapshot output was accepted")
			}
		})
	}
}
