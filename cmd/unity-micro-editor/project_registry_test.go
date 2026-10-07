package main

import (
	"bytes"
	"encoding/binary"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func registryTestProject(t *testing.T) string {
	t.Helper()
	project := filepath.Join(t.TempDir(), "Temp", "owned-project")
	for _, folder := range []string{"Library", "Assets", "Packages"} {
		if err := os.MkdirAll(filepath.Join(project, folder), 0700); err != nil {
			t.Fatal(err)
		}
	}
	return project
}

func registryTestAsset(t *testing.T, project, name, guid, importer string) ProjectRegistryAsset {
	t.Helper()
	filename := filepath.Join(project, filepath.FromSlash(name))
	if err := os.MkdirAll(filepath.Dir(filename), 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filename, []byte("content of "+name), 0600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filename+".meta", []byte("fileFormatVersion: 2\nguid: "+guid+"\n"+importer+":\n  userData: \n  assetBundleName: \n  assetBundleVariant: \n"), 0600); err != nil {
		t.Fatal(err)
	}
	return ProjectRegistryAsset{Path: name}
}

func registryRecordsByKey(records []LMDBRecord) map[string][]byte {
	result := make(map[string][]byte)
	for _, record := range records {
		result[string(record.Key)] = record.Value
	}
	return result
}

func TestProjectRegistryKeepsGUIDsFoldersSettingsAndPackageRoots(t *testing.T) {
	project := registryTestProject(t)
	assets := []ProjectRegistryAsset{
		registryTestAsset(t, project, "Assets/Models/floor.fbx", "b1d15452ab70b6a669e9b8e3c0b91251", "ModelImporter"),
		registryTestAsset(t, project, "Assets/Audio/music.wav", audioGUID, "AudioImporter"),
		registryTestAsset(t, project, "Packages/com.test.models/Runtime/Texture.png", "b2d15452ab70b6a669e9b8e3c0b91251", "TextureImporter"),
		registryTestAsset(t, project, "Assets/Scripts/Script.cs", "b3d15452ab70b6a669e9b8e3c0b91251", "MonoImporter"),
	}
	modelsMeta := "fileFormatVersion: 2\nguid: c1d15452ab70b6a669e9b8e3c0b91251\nfolderAsset: yes\nDefaultImporter:\n  userData: \n"
	if err := os.WriteFile(filepath.Join(project, "Assets/Models.meta"), []byte(modelsMeta), 0600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(project, "Packages/com.test.models/package.json"), []byte(`{"name":"com.test.models","version":"2.3.4"}`), 0600); err != nil {
		t.Fatal(err)
	}
	scriptMeta := "fileFormatVersion: 2\nguid: b3d15452ab70b6a669e9b8e3c0b91251\nMonoImporter:\n  executionOrder: -100\n  userData: 'author''s setting'\n  assetBundleName: models\n  assetBundleVariant: \n"
	if err := os.WriteFile(filepath.Join(project, assets[3].Path+".meta"), []byte(scriptMeta), 0600); err != nil {
		t.Fatal(err)
	}
	report, err := WriteProjectSourceRegistry(ProjectRegistryInput{Project: project, Assets: assets})
	if err != nil {
		t.Fatal(err)
	}
	if report.SourceFileCount != 9 || report.GUIDCount != 10 || report.GUIDPaths["c1d15452ab70b6a669e9b8e3c0b91251"] != "Assets/Models" {
		t.Fatalf("unexpected registry sources or original folder identity: %+v", report)
	}
	tables, err := ReadLMDBTables(report.Database)
	if err != nil {
		t.Fatal(err)
	}
	paths := registryRecordsByKey(tables["PathToGuid"])
	model := paths["assets/models/floor.fbx\x00"]
	wire, _ := sourceRegistryGUID("b1d15452ab70b6a669e9b8e3c0b91251")
	if len(model) != 48 || !bytes.Equal(model[:16], wire) {
		t.Fatal("original model GUID was not retained")
	}
	data, _ := os.ReadFile(filepath.Join(project, assets[0].Path))
	hash := nativeSourceHash(data)
	if !bytes.Equal(model[32:48], hash[:]) {
		t.Fatal("worker-visible source bytes were not hashed")
	}
	props := registryRecordsByKey(tables["GuidPropertyIDToProperty"])
	scriptGUID, _ := sourceRegistryGUID("b3d15452ab70b6a669e9b8e3c0b91251")
	order := props[string(append([]byte("MonoImporter.executionOrder"), scriptGUID...))]
	if len(order) != 4 || int32(binary.LittleEndian.Uint32(order)) != -100 ||
		string(props[string(append([]byte("MonoImporter.userData"), scriptGUID...))]) != "author's setting\x00" {
		t.Fatal("typed importer settings changed")
	}
	roots := registryRecordsByKey(tables["RootFolders"])
	packageRoot := roots["Packages/com.test.models\x00"]
	if len(roots) != 2 || len(packageRoot) < 32 || binary.LittleEndian.Uint32(packageRoot[16:20]) != 1 {
		t.Fatal("native package root was not encoded")
	}
	for index, expected := range []string{"Packages", "com.test.models", filepath.Join(project, "Packages/com.test.models")} {
		offset := 20 + 4*index
		position := offset + int(binary.LittleEndian.Uint32(packageRoot[offset:offset+4]))
		if position < 32 || !bytes.HasPrefix(packageRoot[position:], []byte(expected+"\x00")) {
			t.Fatal("relative package root string differs from observed native layout")
		}
	}
	packageGUID, _ := sourceRegistryGUID("b2d15452ab70b6a669e9b8e3c0b91251")
	if _, exists := props[string(append([]byte("packageName"), packageGUID...))]; exists {
		t.Fatal("UPM root was confused with AssetOrigin package metadata")
	}
	before, _ := os.ReadFile(report.Database)
	if _, err := WriteProjectSourceRegistry(ProjectRegistryInput{Project: project, Assets: assets}); err == nil {
		t.Fatal("existing immutable registry was replaced")
	}
	after, _ := os.ReadFile(report.Database)
	if !bytes.Equal(before, after) {
		t.Fatal("rejected replacement changed source database")
	}
	if data, _ := os.ReadFile(filepath.Join(project, "Assets/Models.meta")); string(data) != modelsMeta {
		t.Fatal("source folder metadata was modified")
	}
	if _, err := os.Stat(filepath.Join(project, "Assets/Audio.meta")); !os.IsNotExist(err) {
		t.Fatal("registry wrote a synthetic folder .meta into content")
	}
}

func TestProjectRegistryRejectsGUIDConflictsUnsafePathsAndExternalSources(t *testing.T) {
	for _, scenario := range []string{"duplicateGUID", "caseCollision", "traversal", "external", "symlink", "badGUID", "override"} {
		t.Run(scenario, func(t *testing.T) {
			project := registryTestProject(t)
			first := registryTestAsset(t, project, "Assets/a.fbx", "b1d15452ab70b6a669e9b8e3c0b91251", "ModelImporter")
			second := registryTestAsset(t, project, "Assets/b.fbx", "b2d15452ab70b6a669e9b8e3c0b91251", "ModelImporter")
			assets := []ProjectRegistryAsset{first, second}
			switch scenario {
			case "duplicateGUID":
				data, _ := os.ReadFile(filepath.Join(project, first.Path+".meta"))
				os.WriteFile(filepath.Join(project, second.Path+".meta"), data, 0600)
			case "caseCollision":
				assets[1] = registryTestAsset(t, project, "Assets/A.fbx", "b2d15452ab70b6a669e9b8e3c0b91251", "ModelImporter")
			case "traversal":
				assets[1].Path = "Assets/../../outside.fbx"
			case "external":
				assets[1].Source = filepath.Join(t.TempDir(), "outside.fbx")
			case "symlink":
				filename := filepath.Join(project, second.Path)
				os.Remove(filename)
				os.Symlink(filepath.Join(project, first.Path), filename)
			case "badGUID":
				os.WriteFile(filepath.Join(project, second.Path+".meta"), []byte("guid: 00000000000000000000000000000000\n"), 0600)
			case "override":
				assets[1].GUID = "c2d15452ab70b6a669e9b8e3c0b91251"
			}
			if _, err := WriteProjectSourceRegistry(ProjectRegistryInput{Project: project, Assets: assets}); err == nil {
				t.Fatal("unsafe registry inputs accepted")
			}
			if _, err := os.Stat(filepath.Join(project, "Library/SourceAssetDB")); !os.IsNotExist(err) {
				t.Fatal("rejected input published a source database")
			}
		})
	}
}

func TestProjectRegistryManyAssets(t *testing.T) {
	project := registryTestProject(t)
	var assets []ProjectRegistryAsset
	for index := 0; index < 600; index++ {
		assets = append(assets, registryTestAsset(t, project, fmt.Sprintf("Assets/Many/floor%04d.fbx", index), fmt.Sprintf("%032x", 100+index), "ModelImporter"))
	}
	report, err := WriteProjectSourceRegistry(ProjectRegistryInput{Project: project, Assets: assets})
	if err != nil {
		t.Fatal(err)
	}
	tables, err := ReadLMDBTables(report.Database)
	if err != nil || len(tables["GuidToPath"]) != 602 || len(tables["hash"]) != 1200 {
		t.Fatalf("large source registry lost entries: %+v %v", report, err)
	}
	for _, record := range tables["GuidToChildren"] {
		if len(record.Value) == 600*16 {
			return // Requires native overflow pages rather than one-leaf probe.
		}
	}
	t.Fatal("large folder child list was not persisted")
}

func TestRegistryMetaGUIDAndScalarsRejectAmbiguousValues(t *testing.T) {
	if guid, err := registryMetaGUID([]byte("guid: B1D15452AB70B6A669E9B8E3C0B91251\nImporter:\n  object: {guid: c1d15452ab70b6a669e9b8e3c0b91251}\n")); err != nil || guid != strings.ToLower("B1D15452AB70B6A669E9B8E3C0B91251") {
		t.Fatalf("valid top-level GUID was not selected: %s %v", guid, err)
	}
	if _, err := registryMetaGUID([]byte("guid: b1d15452ab70b6a669e9b8e3c0b91251\nguid: c1d15452ab70b6a669e9b8e3c0b91251\n")); err == nil {
		t.Fatal("ambiguous GUID accepted")
	}
	if _, err := registryMetaProperties([]byte("MonoImporter:\n  executionOrder: nope\n")); err == nil {
		t.Fatal("invalid native int32 property accepted")
	}
	properties, err := registryMetaProperties([]byte("AssetOrigin:\n  productId: 320309\n  packageName: Character Animation System\n  packageVersion: 2.4.1\n  uploadId: 976338\n"))
	if err != nil || string(properties["packageName"]) != "Character Animation System\x00" ||
		len(properties["productId"]) != 4 || binary.LittleEndian.Uint32(properties["productId"]) != 320309 {
		t.Fatalf("original AssetOrigin typed properties changed: %+v %v", properties, err)
	}
}

func TestRegistryStreamingHashMatchesNativeGolden(t *testing.T) {
	filename := filepath.Join(t.TempDir(), "audio.wav")
	data := fixtureWAV()
	if err := os.WriteFile(filename, data, 0600); err != nil {
		t.Fatal(err)
	}
	// Tiny chunks exercise the streaming XXH3 path independently of the
	// one-shot native vectors already captured from the Editor oracle.
	result, err := registryHashFile(filename, 1024*1024, make([]byte, 37))
	if err != nil || result.hash != nativeSourceHash(data) || result.size != int64(len(data)) {
		t.Fatalf("streaming source digest differs from native content: %+v %v", result, err)
	}
	if _, err := registryHashFile(filename, 100, make([]byte, 37)); err == nil {
		t.Fatal("source file exceeds its declared hashing bound")
	}
}
