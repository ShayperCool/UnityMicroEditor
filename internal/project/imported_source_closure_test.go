package goproject

import (
	"crypto/sha256"
	"encoding/hex"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func recordImportedSourceProof(t *testing.T, s *Session, paths ...string) {
	t.Helper()
	for _, path := range paths {
		for _, logical := range []string{path, path + ".meta"} {
			content, err := os.ReadFile(filepath.Join(s.project, logical))
			if err != nil {
				t.Fatal(err)
			}
			digest := sha256.Sum256(content)
			expected := hex.EncodeToString(digest[:])
			updated := false
			for index := range s.catalog.SourceDependencies {
				if s.catalog.SourceDependencies[index].Path == logical {
					s.catalog.SourceDependencies[index] = dependency{Path: logical, SHA256: expected, Bytes: int64(len(content))}
					updated = true
				}
			}
			if !updated {
				s.catalog.SourceDependencies = append(s.catalog.SourceDependencies, dependency{Path: logical, SHA256: expected, Bytes: int64(len(content))})
			}
		}
	}
}

func importedCatalogIndex(s *Session) map[string]map[string]any {
	result := map[string]map[string]any{}
	for _, asset := range s.catalog.Assets {
		result[assetKey(str(asset["guid"]), str(asset["fileId"]))] = asset
	}
	return result
}

func TestImportedScriptableSourceTraversesInternalNativeDependenciesAndPrefabBindings(t *testing.T) {
	s, output := bindingFixture(t)
	// The native pack links these objects internally, so the scene only names A.
	// B's exact prefab component binding must still be published before callbacks.
	writeFixtureFile(t, filepath.Join(s.project, "Assets/OwnerA.asset"), "--- !u!114 &11400000\nMonoBehaviour:\n  m_Name: Profile\n  modifier: {fileID: 11400000, guid: "+bindingOwnerB+", type: 2}\n")
	s.catalog.DeferredBindings = s.catalog.DeferredBindings[1:2]
	recordImportedSourceProof(t, s, "Assets/OwnerA.asset")
	prepared, err := s.Prepare(output)
	if err != nil {
		t.Fatal(err)
	}
	manifest, _ := ManifestJSON(prepared.Manifest)
	assets := map[string]map[string]any{}
	for _, value := range array(manifest["assets"]) {
		a := object(value)
		assets[assetKey(str(a["guid"]), str(a["fileId"]))] = a
	}
	bindings := array(manifest["importedAssetBindings"])
	if assets[assetKey(bindingOwnerB, "11400000")] == nil || len(bindings) != 1 || object(bindings[0])["ownerGuid"] != bindingOwnerB ||
		!preparedAssetIdentity(assets, bindingPrefabB, bindingComponentID) {
		t.Fatalf("native profile->modifier->prefab component closure was not published: %+v", bindings)
	}
}

func TestImportedScriptableSourcePreservesSignedLocalSubassetCycles(t *testing.T) {
	s, _ := bindingFixture(t)
	writeFixtureFile(t, filepath.Join(s.project, "Assets/OwnerA.asset"), "--- !u!114 &11400000\nMonoBehaviour:\n  nested: {fileID: -7405474662461247964}\n--- !u!114 &-7405474662461247964\nMonoBehaviour:\n  parent: {fileID: 11400000}\n")
	recordImportedSourceProof(t, s, "Assets/OwnerA.asset")
	if err := s.indexGUIDs(); err != nil {
		t.Fatal(err)
	}
	asset := s.assets[assetKey(bindingOwnerA, "11400000")]
	subasset := clone(asset).(map[string]any)
	subasset["fileId"] = "-7405474662461247964"
	s.catalog.Assets = append(s.catalog.Assets, subasset)
	refs, err := s.importedSourceReferences(asset, importedCatalogIndex(s))
	if err != nil || len(refs) != 1 || fileID(refs[0]) != "-7405474662461247964" || str(refs[0]["guid"]) != bindingOwnerA {
		t.Fatalf("signed local reference was rounded or lost: %+v %v", refs, err)
	}
	refs, err = s.importedSourceReferences(subasset, importedCatalogIndex(s))
	if err != nil || len(refs) != 1 || fileID(refs[0]) != "11400000" {
		t.Fatalf("local subasset cycle was not represented exactly: %+v %v", refs, err)
	}
}

func TestImportedScriptableSourceRejectsCatalogSourceAndMetadataDrift(t *testing.T) {
	for _, name := range []string{"source", "metadata", "missing source proof", "missing metadata proof", "baseline masks catalog hash", "missing object", "wrong class"} {
		t.Run(name, func(t *testing.T) {
			s, _ := bindingFixture(t)
			if err := s.indexGUIDs(); err != nil {
				t.Fatal(err)
			}
			asset := s.assets[assetKey(bindingOwnerA, "11400000")]
			switch name {
			case "source":
				writeFixtureFile(t, filepath.Join(s.project, "Assets/OwnerA.asset"), "--- !u!114 &11400000\nMonoBehaviour:\n  changed: 1\n")
			case "metadata":
				writeFixtureFile(t, filepath.Join(s.project, "Assets/OwnerA.asset.meta"), "guid: "+bindingOwnerA+"\nchanged: 1\n")
			case "missing source proof":
				s.catalog.SourceDependencies = s.catalog.SourceDependencies[1:]
			case "missing metadata proof":
				s.catalog.SourceDependencies = append(s.catalog.SourceDependencies[:1], s.catalog.SourceDependencies[2:]...)
			case "baseline masks catalog hash":
				s.critical["Assets/OwnerA.asset"] = s.catalog.SourceDependencies[0].SHA256
				s.catalog.SourceDependencies[0].SHA256 = strings.Repeat("0", 64)
			case "missing object":
				asset["fileId"] = "123"
			case "wrong class":
				writeFixtureFile(t, filepath.Join(s.project, "Assets/OwnerA.asset"), "--- !u!1 &11400000\nGameObject:\n  m_Name: Wrong\n")
				recordImportedSourceProof(t, s, "Assets/OwnerA.asset")
			}
			if _, err := s.importedSourceReferences(asset, importedCatalogIndex(s)); err == nil {
				t.Fatal("unproven imported source closure was accepted")
			}
		})
	}
}

func TestImportedScriptableSourceKeepsOpaqueNativeDebugReferencesInNativePack(t *testing.T) {
	s, _ := bindingFixture(t)
	writeFixtureFile(t, filepath.Join(s.project, "Assets/OwnerA.asset"), "--- !u!114 &11400000\nMonoBehaviour:\n  modifier: {fileID: 11400000, guid: "+bindingOwnerB+", type: 2}\n  obsoleteDebugShader: {fileID: 4800000, guid: 8f96cd657dc40064aa21efcc7e50a2e7, type: 3}\n")
	recordImportedSourceProof(t, s, "Assets/OwnerA.asset")
	if err := s.indexGUIDs(); err != nil {
		t.Fatal(err)
	}
	refs, err := s.importedSourceReferences(s.assets[assetKey(bindingOwnerA, "11400000")], importedCatalogIndex(s))
	if err != nil || len(refs) != 1 || str(refs[0]["guid"]) != bindingOwnerB {
		t.Fatalf("native opaque pointers were reinterpreted or scripted dependency lost: %+v %v", refs, err)
	}
}

func TestImportedScriptableSourceRejectsChangedIndirectNativeOwner(t *testing.T) {
	s, output := bindingFixture(t)
	writeFixtureFile(t, filepath.Join(s.project, "Assets/OwnerA.asset"), "--- !u!114 &11400000\nMonoBehaviour:\n  modifier: {fileID: 11400000, guid: "+bindingOwnerB+", type: 2}\n")
	recordImportedSourceProof(t, s, "Assets/OwnerA.asset")
	s.catalog.DeferredBindings = s.catalog.DeferredBindings[1:2]
	good, err := s.Prepare(output)
	if err != nil {
		t.Fatal(err)
	}
	before, _ := os.ReadFile(good.Manifest)
	writeFixtureFile(t, filepath.Join(s.project, "Assets/OwnerB.asset"), "--- !u!114 &11400000\nMonoBehaviour:\n  m_Name: ChangedModifier\n")
	content, _ := os.ReadFile(filepath.Join(s.project, "Assets/OwnerB.asset"))
	digest := sha256.Sum256(content)
	s.critical["Assets/OwnerB.asset"] = hex.EncodeToString(digest[:])
	if _, err := s.Prepare(output); err == nil {
		t.Fatal("indirect imported owner used a changed source hidden by baseline critical hashes")
	}
	after, _ := os.ReadFile(good.Manifest)
	if string(before) != string(after) {
		t.Fatal("invalid indirect dependency replaced the prior valid publication")
	}
}
