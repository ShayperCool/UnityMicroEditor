package goproject

import (
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"strconv"
	"strings"
	"testing"
)

func TestAssetReferenceWorklistDeduplicatesProcessedExactIdentities(t *testing.T) {
	work := newAssetReferenceWorklist()
	for _, id := range []string{"-7405474662461247964", "8107781485188562762"} {
		for i := 0; i < 40000; i++ {
			if err := work.addIdentity(bindingPrefabA, id); err != nil {
				t.Fatal(err)
			}
		}
	}
	if len(work.items) != 2 || len(work.seen) != 2 {
		t.Fatal("duplicates consumed queue bound")
	}
	last, ok := work.pop()
	if !ok || last.fileID != "8107781485188562762" {
		t.Fatal("signed exact identity or LIFO order lost", last)
	}
	if err := work.addIdentity(bindingPrefabA, last.fileID); err != nil {
		t.Fatal(err)
	}
	if len(work.items) != 1 {
		t.Fatal("processed cyclic identity was queued again")
	}
	if _, ok := work.pop(); !ok {
		t.Fatal("other subasset identity was merged")
	}
	if _, ok := work.pop(); ok {
		t.Fatal("worklist did not drain")
	}
	for _, slot := range work.items[:cap(work.items)] {
		if slot != (assetReference{}) {
			t.Fatal("completed queue entry retains source data")
		}
	}
	for _, ref := range []assetReference{{bindingPrefabA, ""}, {bindingPrefabA, "0"}, {"", "1"}, {"0000000000000000f000000000000000", "69"}} {
		if err := work.addIdentity(ref.guid, ref.fileID); err != nil {
			t.Fatal(err)
		}
	}
	if len(work.seen) != 2 {
		t.Fatal("empty/null/builtin reference consumed identity bound")
	}
}

func TestAssetReferenceWorklistRejectsTooManyUniqueReferencesAfterDrain(t *testing.T) {
	work := newAssetReferenceWorklist()
	for i := 0; i < assetReferenceClosureMaximum; i++ {
		if err := work.addIdentity(bindingPrefabA, strconv.Itoa(i+1)); err != nil {
			t.Fatal(err)
		}
		if _, ok := work.pop(); !ok {
			t.Fatal("queue unexpectedly empty")
		}
	}
	if err := work.addIdentity(bindingPrefabA, "1"); err != nil {
		t.Fatal("existing identity rejected at bound", err)
	}
	if err := work.addIdentity(bindingPrefabA, strconv.Itoa(assetReferenceClosureMaximum+1)); err == nil || !strings.Contains(err.Error(), "unique-reference bound") {
		t.Fatal("processed identities bypassed total unique bound", err)
	}
	if len(work.seen) != assetReferenceClosureMaximum || len(work.items) != 0 {
		t.Fatal("rejected reference changed accepted worklist")
	}
}

func TestImportedBindingWideDuplicateSceneAndCachedAssetClosure(t *testing.T) {
	s, _ := bindingFixture(t)
	if err := s.indexGUIDs(); err != nil {
		t.Fatal(err)
	}
	s.dependencies = map[string]dependency{}
	seed, err := s.prepareAssets(nil)
	if err != nil {
		t.Fatal(err)
	}
	for _, value := range seed.Assets {
		asset := object(value)
		s.assets[assetKey(str(asset["guid"]), str(asset["fileId"]))] = asset
	}
	references := make([]any, 40000)
	for i := range references {
		references[i] = map[string]any{"guid": bindingPrefabB, "fileID": bindingComponentID}
	}
	scene := []*expandedObject{{Document: Document{ClassID: 114, FileID: "11", Data: map[string]any{"manyReferences": references}}}}
	for pass := 0; pass < 2; pass++ {
		prepared, err := s.prepareAssets(scene)
		if err != nil {
			t.Fatal(err)
		}
		if !reflect.DeepEqual(prepared, seed) {
			t.Fatal("duplicate PPtrs altered proven asset/binding identities")
		}
	}
}

func TestImportedBindingPropertyBoundStillRejectsDistinctBindingsSharingTarget(t *testing.T) {
	s, _ := bindingFixture(t)
	if err := s.indexGUIDs(); err != nil {
		t.Fatal(err)
	}
	bindings := make([]map[string]any, 32769)
	for i := range bindings {
		entry := clone(s.catalog.DeferredBindings[0]).(map[string]any)
		entry["propertyPath"] = fmt.Sprintf("many[%d]", i)
		bindings[i] = entry
	}
	s.catalog.DeferredBindings = bindings
	closure := s.importedBindingClosure()
	work := newAssetReferenceWorklist()
	if err := closure.queue(s.assets[assetKey(bindingOwnerA, "11400000")], work); err == nil {
		t.Fatal("deduplication removed distinct binding-property bound")
	}
	if len(work.seen) != 1 {
		t.Fatal("same-target bindings were not deduplicated")
	}
}

func TestAssetReferenceActualPreparedManifestReadOnly(t *testing.T) {
	project, baseline, catalogPath := os.Getenv("UNITY_MICRO_EDITOR_ASSET_PROJECT"), os.Getenv("UNITY_MICRO_EDITOR_ASSET_BASELINE"), os.Getenv("UNITY_MICRO_EDITOR_ASSET_CATALOG")
	if project == "" || baseline == "" {
		t.Skip("explicit owned actual project/baseline not supplied")
	}
	s, err := NewSession(Options{Project: project, Manifest: baseline, NativeCatalog: catalogPath})
	if err != nil {
		t.Fatal(err)
	}
	output := os.Getenv("UNITY_MICRO_EDITOR_ASSET_OUTPUT")
	if output == "" {
		output = filepath.Join(project, "Library/UnityMicroEditor/asset-reference-check", filepath.Base(t.TempDir()))
	}
	first, err := s.Prepare(output)
	if err != nil {
		t.Fatal(err)
	}
	manifest, err := ManifestJSON(first.Manifest)
	if err != nil {
		t.Fatal(err)
	}
	second, err := s.Prepare(output)
	if err != nil {
		t.Fatal(err)
	}
	secondManifest, err := ManifestJSON(second.Manifest)
	if err != nil {
		t.Fatal(err)
	}
	// Each successful publication intentionally owns a fresh random revision.
	delete(manifest, "revision")
	delete(secondManifest, "revision")
	if first.Objects != second.Objects || first.Assets != second.Assets || !reflect.DeepEqual(manifest, secondManifest) {
		t.Fatal("unchanged actual preparation changed semantic asset/binding closure")
	}
	t.Logf("actual repeated preparation objects=%d assets=%d bindings=%d revision=%s manifest=%s", first.Objects, first.Assets, len(array(manifest["importedAssetBindings"])), first.Revision, first.Manifest)
}
