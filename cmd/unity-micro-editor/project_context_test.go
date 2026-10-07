package main

import (
	"bytes"
	"context"
	"os"
	"path/filepath"
	"testing"
)

func projectContextSource(t *testing.T) string {
	t.Helper()
	project := fixtureSource(t)
	if err := os.MkdirAll(filepath.Join(project, "ProjectSettings"), 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(project, "ProjectSettings/ProjectVersion.txt"), []byte("m_EditorVersion: "+fixtureUnityVersion+"\r\n"), 0600); err != nil {
		t.Fatal(err)
	}
	return project
}

func TestProjectContextOwnedLibrarySurvivesEditorTempCleanup(t *testing.T) {
	project := projectContextSource(t)
	prepared, err := CreateProjectImportContext(context.Background(), project, filepath.Join(project, "Library/UnityMicroEditor/imports/model"), []string{modelSource})
	if err != nil {
		t.Fatal(err)
	}
	if err := os.RemoveAll(filepath.Join(project, "Temp")); err != nil {
		t.Fatal(err)
	}
	if err := VerifyProjectImportSources(context.Background(), prepared); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(filepath.Join(prepared.Project, modelSource)); err != nil {
		t.Fatal("persistent native input disappeared", err)
	}
	for _, path := range []string{"Library/ScriptAssemblies/new", "Library/UnityMicroEditorOther/new", "Library/UnityMicroEditor"} {
		if _, err := CreateProjectImportContext(context.Background(), project, filepath.Join(project, path), []string{modelSource}); err == nil {
			t.Fatalf("foreign cache accepted: %s", path)
		}
	}
}

func TestProjectContextCopiesSourceIdentityWithoutSeedingLibrary(t *testing.T) {
	project := projectContextSource(t)
	if err := os.MkdirAll(filepath.Join(project, "Library"), 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(project, "Library/SourceAssetDB"), []byte("must not seed worker"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(project, "Assets/Models.meta"), []byte("guid: c1d15452ab70b6a669e9b8e3c0b91251\nfolderAsset: yes\n"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(project, "ProjectSettings/ProjectSettings.asset"), []byte("PlayerSettings:\n  m_ColorSpace: 1\n"), 0600); err != nil {
		t.Fatal(err)
	}
	output := filepath.Join(project, "Temp/project-import/new")
	prepared, err := CreateProjectImportContext(context.Background(), project, output, []string{modelSource})
	if err != nil {
		t.Fatal(err)
	}
	if len(prepared.Assets) != 1 || prepared.Assets[0].GUID != "b1d15452ab70b6a669e9b8e3c0b91251" {
		t.Fatalf("source identities changed: %+v", prepared)
	}
	for _, logical := range []string{modelSource, modelSource + ".meta", "Assets/Models.meta", "ProjectSettings/ProjectVersion.txt", "ProjectSettings/ProjectSettings.asset"} {
		source, err := os.ReadFile(filepath.Join(project, logical))
		if err != nil {
			t.Fatal(err)
		}
		copied, err := os.ReadFile(filepath.Join(prepared.Project, logical))
		if err != nil || !bytes.Equal(source, copied) {
			t.Fatalf("source bytes changed for %s", logical)
		}
	}
	entries, err := os.ReadDir(filepath.Join(prepared.Project, "Library"))
	if err != nil || len(entries) != 0 {
		t.Fatal("project context copied Editor import state")
	}
	registry, err := WriteProjectSourceRegistry(ProjectRegistryInput{Project: prepared.Project, Assets: []ProjectRegistryAsset{{Path: modelSource}}})
	if err != nil || registry.GUIDPaths["c1d15452ab70b6a669e9b8e3c0b91251"] != "Assets/Models" {
		t.Fatalf("folder GUID was not available to own source registry: %+v %v", registry, err)
	}
	if err := VerifyProjectImportSources(context.Background(), prepared); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(project, modelSource), []byte("changed during import"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := VerifyProjectImportSources(context.Background(), prepared); err == nil {
		t.Fatal("changed source accepted with stale imported content")
	}
	before, _ := os.ReadFile(filepath.Join(output, "project-context.json"))
	if _, err := CreateProjectImportContext(context.Background(), project, output, []string{modelSource}); err == nil {
		t.Fatal("existing owned context was replaced")
	}
	after, _ := os.ReadFile(filepath.Join(output, "project-context.json"))
	if !bytes.Equal(before, after) {
		t.Fatal("rejected replacement changed context manifest")
	}
}

func TestProjectContextPackageSourceIsMaterializedWithLogicalPath(t *testing.T) {
	project := projectContextSource(t)
	physical := "Library/PackageCache/com.test.models@abc/Runtime/Texture.png"
	registryTestAsset(t, project, physical, "b2d15452ab70b6a669e9b8e3c0b91251", "TextureImporter")
	logical := "Packages/com.test.models/Runtime/Texture.png"
	prepared, err := CreateProjectImportContext(context.Background(), project, filepath.Join(project, "Temp/package-context"), []string{logical})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(filepath.Join(prepared.Project, logical)); err != nil {
		t.Fatal("resolved package source was not made visible to worker")
	}
	if _, err := os.Stat(filepath.Join(prepared.Project, "Library/PackageCache")); !os.IsNotExist(err) {
		t.Fatal("package sources were copied into an Editor import cache")
	}
}

func TestProjectContextRejectsEscapesBadVersionsAndCancelledCopies(t *testing.T) {
	for _, scenario := range []string{"outputEscape", "sourceEscape", "badVersion", "duplicate", "cancelled"} {
		t.Run(scenario, func(t *testing.T) {
			project := projectContextSource(t)
			output := filepath.Join(project, "Temp/rejected")
			assets := []string{modelSource}
			ctx, cancel := context.WithCancel(context.Background())
			defer cancel()
			switch scenario {
			case "outputEscape":
				output = filepath.Join(t.TempDir(), "rejected")
			case "sourceEscape":
				filename := filepath.Join(project, modelSource)
				outside := filepath.Join(t.TempDir(), "outside.fbx")
				os.WriteFile(outside, []byte("unrelated source"), 0600)
				os.Remove(filename)
				os.Symlink(outside, filename)
			case "badVersion":
				os.WriteFile(filepath.Join(project, "ProjectSettings/ProjectVersion.txt"), []byte("m_EditorVersion: 6000.5.0f1\n"), 0600)
			case "duplicate":
				assets = append(assets, modelSource)
			case "cancelled":
				cancel()
			}
			if _, err := CreateProjectImportContext(ctx, project, output, assets); err == nil {
				t.Fatal("invalid project context input accepted")
			}
			if _, err := os.Stat(output); !os.IsNotExist(err) {
				t.Fatal("rejected input wrote an import context")
			}
		})
	}
}
