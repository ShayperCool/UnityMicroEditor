package goproject

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

const removedModelRenderer = "-5824371076177060002"
const currentModelRoot = "919132149155446097"
const currentModelTransform = "-8679921383154817045"
const staleVariantInstance = "4076523666981031211"
const staleVariantGUID = "33333333333333333333333333333333"

func publicModelStaleOverrideFixture(t *testing.T, target string) (*Session, string, string) {
	t.Helper()
	s, output, _, _, _ := nativeFixture(t, ".fbx")
	docs := cloneDocuments(s.models[nativeFixtureGUID])
	docs[0].FileID, docs[1].FileID = currentModelRoot, currentModelTransform
	docs[0].Data["m_Component"] = []any{map[string]any{"component": map[string]any{"fileID": currentModelTransform}}}
	docs[1].Data["m_GameObject"] = map[string]any{"fileID": currentModelRoot}
	s.models[nativeFixtureGUID] = docs
	s.catalog.ModelPrefabs[0].Objects = docs
	s.catalog.ModelPrefabs[0].RootFileID = currentModelRoot
	s.catalog.Success, s.catalog.CookedRegistryVerified = true, true
	s.catalog.Backend, s.catalog.SceneLoader = "unity_editor_public_api", "raw"
	s.catalog.SourceDependencies = []dependency{{Path: "Assets/Native.fbx", SHA256: s.critical["Assets/Native.fbx"]}, {Path: "Assets/Native.fbx.meta", SHA256: s.critical["Assets/Native.fbx.meta"]}}
	for _, doc := range docs {
		s.catalog.Assets = append(s.catalog.Assets, map[string]any{"guid": nativeFixtureGUID, "path": "Assets/Native.fbx", "fileId": doc.FileID, "typeName": doc.TypeName, "classId": doc.ClassID})
	}
	variant := "Assets/Variant.prefab"
	writeFixtureFile(t, filepath.Join(s.project, variant), fmt.Sprintf(`--- !u!1001 &%s
PrefabInstance:
  m_SourcePrefab: {fileID: 100100000, guid: %s, type: 3}
  m_Modification:
    m_TransformParent: {fileID: 0}
    m_Modifications:
    - target: {fileID: %s, guid: %s, type: 3}
      propertyPath: m_Materials.Array.data[0]
      value:
      objectReference: {fileID: 0}
    - target: {fileID: %s, guid: %s, type: 3}
      propertyPath: m_LocalPosition.x
      value: 3.5
      objectReference: {fileID: 0}
`, staleVariantInstance, nativeFixtureGUID, target, nativeFixtureGUID, currentModelTransform, nativeFixtureGUID))
	writeFixtureFile(t, filepath.Join(s.project, variant)+".meta", "guid: "+staleVariantGUID+"\n")
	writeFixtureFile(t, filepath.Join(s.project, "Assets/Scenes/Sample.unity"), fmt.Sprintf(`--- !u!1001 &9223372036854775806
PrefabInstance:
  m_SourcePrefab: {fileID: 100100000, guid: %s, type: 3}
  m_Modification:
    m_TransformParent: {fileID: 0}
    m_Modifications: []
--- !u!1660057539 &9223372036854775807
SceneRoots:
  m_Roots:
  - {fileID: 9223372036854775806}
`, staleVariantGUID))
	s.indexDirty = true
	return s, output, variant
}

func TestBaselinePublicModelIgnoresProvenRemovedOverrideWithExactProvenance(t *testing.T) {
	s, output, variant := publicModelStaleOverrideFixture(t, removedModelRenderer)
	before := map[string]string{}
	for _, path := range []string{"Assets/Native.fbx", "Assets/Native.fbx.meta", variant, variant + ".meta", "Assets/Scenes/Sample.unity", "Assets/Scenes/Sample.unity.meta"} {
		data, err := os.ReadFile(filepath.Join(s.project, path))
		if err != nil {
			t.Fatal(err)
		}
		before[path] = string(data)
	}
	for pass := 0; pass < 2; pass++ {
		prepared, err := s.Prepare(output)
		if err != nil {
			t.Fatalf("complete public inventory should permit only the removed target: %v", err)
		}
		manifest, err := ManifestJSON(prepared.Manifest)
		if err != nil {
			t.Fatal(err)
		}
		diagnostics := array(manifest["ignoredPrefabOverrideDiagnostics"])
		if len(diagnostics) != 1 {
			t.Fatalf("diagnostic must be stable across repeat preparation: %+v", diagnostics)
		}
		entry := object(diagnostics[0])
		if entry["path"] != variant || entry["instanceFileId"] != staleVariantInstance || entry["targetGuid"] != nativeFixtureGUID || entry["targetFileId"] != removedModelRenderer || entry["propertyPath"] != "m_Materials.Array.data[0]" || entry["reason"] != "target_absent_from_verified_native_model" {
			t.Fatalf("lost exact saved override provenance: %+v", entry)
		}
		if entry["proofSourcePath"] != "Assets/Native.fbx" || entry["proofSourceSha256"] != s.critical["Assets/Native.fbx"] || entry["proofMetaSha256"] != s.critical["Assets/Native.fbx.meta"] || !validNativeHex(str(entry["proofInventorySha256"]), 64) {
			t.Fatalf("ignored override lacks hash-bound current public proof: %+v", entry)
		}
	}
	graph, err := s.AuthoredGraph("Assets/Scenes/Sample.unity")
	if err != nil {
		t.Fatal(err)
	}
	if len(graph.IgnoredPrefabOverrideDiagnostics) != 1 || len(graph.Objects) != 3 {
		t.Fatalf("read graph lost the full model or its diagnostic: %+v", graph)
	}
	positionPreserved, identityPreserved := false, false
	for _, doc := range graph.Objects {
		if doc.ClassID == 4 {
			positionPreserved = str(object(doc.Data["m_LocalPosition"])["x"]) == "3.5"
			for _, identity := range doc.Identities {
				identityPreserved = identityPreserved || identity.GUID == nativeFixtureGUID && identity.FileID == currentModelTransform
			}
		}
	}
	if !positionPreserved || !identityPreserved {
		t.Fatalf("live override or signed native source identity changed: %+v", graph.Objects)
	}
	for path, expected := range before {
		data, _ := os.ReadFile(filepath.Join(s.project, path))
		if string(data) != expected {
			t.Fatalf("inspection/preparation modified authored source %s", path)
		}
	}
}

func TestBaselineRemovedOverrideRequiresCompleteCurrentPublicModel(t *testing.T) {
	cases := map[string]func(*Session){
		"failed export":                         func(s *Session) { s.catalog.Success = false },
		"unverified registry":                   func(s *Session) { s.catalog.CookedRegistryVerified = false },
		"native scene":                          func(s *Session) { s.catalog.ContainsScenes = true },
		"foreign backend":                       func(s *Session) { s.catalog.Backend = "unknown" },
		"missing meta proof":                    func(s *Session) { delete(s.critical, "Assets/Native.fbx.meta") },
		"catalog source proof missing":          func(s *Session) { s.catalog.SourceDependencies = nil },
		"baseline masks changed catalog source": func(s *Session) { s.catalog.SourceDependencies[0].SHA256 = strings.Repeat("0", 64) },
		"missing root":                          func(s *Session) { s.catalog.ModelPrefabs[0].RootFileID = "0" },
		"incomplete graph":                      func(s *Session) { s.catalog.ModelPrefabs[0].Objects = s.catalog.ModelPrefabs[0].Objects[:1] },
		"incomplete registry":                   func(s *Session) { s.catalog.Assets = s.catalog.Assets[:1] },
		"existing omitted component": func(s *Session) {
			s.catalog.Assets = append(s.catalog.Assets, map[string]any{"guid": nativeFixtureGUID, "path": "Assets/Native.fbx", "fileId": "7795428923366279233", "typeName": "UnityEngine.MeshRenderer", "classId": 23})
		},
		"excluded target exists": func(s *Session) {
			s.catalog.ExcludedRuntimeAssets = []map[string]any{{"guid": nativeFixtureGUID, "fileId": removedModelRenderer}}
		},
		"known animation target exists": func(s *Session) {
			s.catalog.ModelAnimationClips = []map[string]any{{"guid": nativeFixtureGUID, "fileId": removedModelRenderer}}
		},
	}
	for name, change := range cases {
		t.Run(name, func(t *testing.T) {
			s, output, _ := publicModelStaleOverrideFixture(t, removedModelRenderer)
			change(s)
			if _, err := s.Prepare(output); err == nil || !strings.Contains(err.Error(), "unresolved prefab override") {
				t.Fatalf("unproven stale override was accepted: %v", err)
			}
		})
	}
}

func TestBaselineExistingNativeResourceOverrideRemainsRejected(t *testing.T) {
	s, output, _ := publicModelStaleOverrideFixture(t, "4300000")
	if _, err := s.Prepare(output); err == nil || !strings.Contains(err.Error(), "unresolved prefab override") {
		t.Fatalf("existing native mesh outside hierarchy was silently treated as removed: %v", err)
	}
}

func TestBaselineMalformedNativeTargetCannotBeCalledRemoved(t *testing.T) {
	for _, target := range []string{"0", "9223372036854775808", "01", "abc"} {
		t.Run(target, func(t *testing.T) {
			s, output, _ := publicModelStaleOverrideFixture(t, target)
			if _, err := s.Prepare(output); err == nil {
				t.Fatalf("noncanonical signed 64-bit target accepted: %s", target)
			}
		})
	}
}

func TestBaselineStaleOverrideCannotBypassChangedNativeSourceProof(t *testing.T) {
	for _, path := range []string{"Assets/Native.fbx", "Assets/Native.fbx.meta"} {
		t.Run(path, func(t *testing.T) {
			s, output, _ := publicModelStaleOverrideFixture(t, removedModelRenderer)
			if _, err := s.Prepare(output); err != nil {
				t.Fatal(err)
			}
			bytes, _ := os.ReadFile(filepath.Join(s.project, path))
			writeFixtureFile(t, filepath.Join(s.project, path), string(bytes)+"\nchanged importer/source\n")
			var needs *NeedsImportError
			if _, err := s.Prepare(output); !errors.As(err, &needs) {
				t.Fatalf("stale override bypassed current source hash validation: %v", err)
			}
		})
	}
}
