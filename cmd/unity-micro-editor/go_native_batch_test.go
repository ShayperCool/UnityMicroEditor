package main

import (
	"strings"
	"testing"

	goproject "unitymicroeditor/internal/project"
)

func TestNativeBatchDependenciesPublishFreshReplacementBeforeDependent(t *testing.T) {
	first := goproject.NativeReplacement{GUID: "11111111111111111111111111111111", SourceSHA256: "new first"}
	second := goproject.NativeReplacement{GUID: "22222222222222222222222222222222", SourceSHA256: "new second"}
	first.Content.Dependencies = []goproject.NativeContentDependency{{GUID: second.GUID}, {GUID: "0000000000000000f000000000000000", Global: true}}
	ordered, err := orderNativeBatch([]goproject.NativeReplacement{first, second}, map[string]goproject.NativeReplacement{second.GUID: {GUID: second.GUID, SourceSHA256: "old second"}})
	if err != nil || len(ordered) != 2 || ordered[0].GUID != second.GUID || ordered[0].SourceSHA256 != "new second" || ordered[1].GUID != first.GUID {
		t.Fatalf("batch ignored an updated dependency or changed ordering: %+v %v", ordered, err)
	}
	first.Content.Dependencies = []goproject.NativeContentDependency{{GUID: "33333333333333333333333333333333"}}
	if _, err := orderNativeBatch([]goproject.NativeReplacement{first}, nil); err == nil || !strings.Contains(err.Error(), "no imported runtime descriptor") {
		t.Fatalf("published dependency absent from batch/current entries: %v", err)
	}
	first.Content.Dependencies = []goproject.NativeContentDependency{{GUID: second.GUID}}
	second.Content.Dependencies = []goproject.NativeContentDependency{{GUID: first.GUID}}
	if _, err := orderNativeBatch([]goproject.NativeReplacement{first, second}, nil); err == nil || !strings.Contains(err.Error(), "cycle") {
		t.Fatalf("published cyclic native batch: %v", err)
	}
}

func TestNativeLegacyRefreshCandidatesRequireExactSourceGUIDAndMetadata(t *testing.T) {
	opts, session, entries := goNativeReplayFixture(t)
	for index := range entries {
		entries[index].EnvironmentSHA256 = ""
	}
	entries[1].SourceSHA256 = strings.Repeat("0", 64)
	entries[2].MetaSHA256 = strings.Repeat("0", 64)
	if err := atomicJSON(opts.Output+"/native/index.json", entries); err != nil {
		t.Fatal(err)
	}
	importer, err := newNativeProjectImporter(opts, session)
	if err != nil || len(importer.pendingNativePaths) != 1 || importer.pendingNativePaths[0] != entries[0].SourcePath || len(importer.entries) != 0 {
		t.Fatalf("legacy batch refresh accepted stale inputs or replayed legacy environment: %+v %v", importer, err)
	}
	if !coldNativeProfile("Assets/Model.fbx") || !coldNativeProfile("Assets/Poster.png") || !coldNativeProfile("Assets/Audio.wav") || coldNativeProfile("Assets/UI.uxml") || coldNativeProfile("Assets/Uncompiled.shader") || coldNativeProfile("Assets/Unverified.jpg") {
		t.Fatal("batch refresh includes an unverified cold native profile")
	}
}
