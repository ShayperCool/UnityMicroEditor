package goproject

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"path/filepath"
	"sort"
	"strings"
)

// PrefabOverrideDiagnostic records a saved override Unity cannot apply because
// its target is absent from a complete current public native model inventory.
// Identifiers remain strings, including signed 64-bit imported object IDs.
type PrefabOverrideDiagnostic struct {
	Path                 string `json:"path"`
	InstanceFileID       string `json:"instanceFileId"`
	TargetGUID           string `json:"targetGuid"`
	TargetFileID         string `json:"targetFileId"`
	PropertyPath         string `json:"propertyPath"`
	Reason               string `json:"reason"`
	ProofBackend         string `json:"proofBackend"`
	ProofSourcePath      string `json:"proofSourcePath"`
	ProofSourceSHA256    string `json:"proofSourceSha256"`
	ProofMetaSHA256      string `json:"proofMetaSha256"`
	ProofInventorySHA256 string `json:"proofInventorySha256"`
}

func (s *Session) provenMissingNativeModelTarget(guid, id string) *PrefabOverrideDiagnostic {
	if !validNativeGUID(guid) || !validNativeID(id) {
		return nil
	}
	if native, known := s.nativeOverlays[guid]; known && len(native.ModelGraph) != 0 {
		for _, item := range native.Content.Objects {
			if item.FileID == id {
				return nil
			}
		}
		return &PrefabOverrideDiagnostic{Reason: "target_absent_from_verified_native_model", ProofBackend: "verified_native_replacement",
			ProofSourcePath: native.SourcePath, ProofSourceSHA256: native.SourceSHA256, ProofMetaSHA256: native.MetaSHA256, ProofInventorySHA256: native.ModelGraphSHA256}
	}
	if !s.catalog.Success || !s.catalog.CookedRegistryVerified || s.catalog.ContainsScenes || s.catalog.SceneLoader != "raw" ||
		(s.catalog.Backend != "unity_editor_public_api" && s.catalog.Backend != "unity_editor") {
		return nil
	}
	for _, model := range s.catalog.ModelPrefabs {
		if model.GUID != guid || model.Path != s.byGUID[guid] || len(model.Objects) == 0 || model.RootFileID == "" || model.RootFileID == "0" ||
			filepath.Ext(model.Path) == ".prefab" || filepath.Ext(model.Path) == ".unity" {
			continue
		}
		source, meta := s.dependencies[model.Path], s.dependencies[model.Path+".meta"]
		if !validNativeHex(source.SHA256, 64) || !validNativeHex(meta.SHA256, 64) || s.critical[model.Path] != source.SHA256 || s.critical[model.Path+".meta"] != meta.SHA256 ||
			!s.nativeCatalogSourceMatches(model.Path, source.SHA256) || !s.nativeCatalogSourceMatches(model.Path+".meta", meta.SHA256) {
			return nil
		}
		identities := map[string]string{}
		for _, asset := range s.catalog.Assets {
			if str(asset["guid"]) != guid {
				continue
			}
			fileID, typeName := str(asset["fileId"]), str(asset["typeName"])
			if fileID == id {
				return nil
			}
			if str(asset["path"]) != model.Path || !validNativeID(fileID) || typeName == "" {
				return nil
			}
			if previous := identities[fileID]; previous != "" && previous != typeName {
				return nil
			}
			identities[fileID] = typeName
		}
		// Excluded components/clips still exist in the source model, even when
		// absent from the runtime graph. Their overrides must not be called stale.
		for _, records := range [][]map[string]any{s.catalog.ExcludedRuntimeAssets, s.catalog.ModelAnimationClips} {
			for _, asset := range records {
				if str(asset["guid"]) == guid && str(asset["fileId"]) == id {
					return nil
				}
			}
		}
		for _, asset := range s.assets {
			if str(asset["guid"]) == guid && str(asset["fileId"]) == id {
				return nil
			}
		}
		root, seen := false, map[string]bool{}
		for _, doc := range model.Objects {
			if doc.FileID == id {
				return nil
			}
			if seen[doc.FileID] || identities[doc.FileID] != doc.TypeName {
				return nil
			}
			seen[doc.FileID] = true
			if doc.FileID == model.RootFileID && doc.ClassID == 1 {
				root = true
			}
		}
		if !root || len(identities) == 0 {
			return nil
		}
		// The exported object graph must cover the complete live GameObject and
		// component inventory. Native resources are represented separately. An
		// incomplete graph cannot establish that an authored target was removed.
		for fileID, typeName := range identities {
			if !seen[fileID] && !nativeModelResourceType(typeName) && !s.excludedNativeModelObject(guid, fileID) {
				return nil
			}
		}
		for _, doc := range model.Objects {
			if doc.ClassID == 1 {
				for _, component := range array(doc.Data["m_Component"]) {
					if !seen[fileID(object(component)["component"])] {
						return nil
					}
				}
			} else if owner := fileID(doc.Data["m_GameObject"]); owner != "0" && !seen[owner] {
				return nil
			}
		}
		payload, _ := json.Marshal(struct {
			Objects    []Document
			Identities map[string]string
		}{model.Objects, identities})
		digest := sha256.Sum256(payload)
		return &PrefabOverrideDiagnostic{Reason: "target_absent_from_verified_native_model", ProofBackend: s.catalog.Backend,
			ProofSourcePath: model.Path, ProofSourceSHA256: source.SHA256, ProofMetaSHA256: meta.SHA256, ProofInventorySHA256: hex.EncodeToString(digest[:])}
	}
	return nil
}

func (s *Session) nativeCatalogSourceMatches(path, expected string) bool {
	found := false
	for _, dependency := range s.catalog.SourceDependencies {
		if dependency.Path != path {
			continue
		}
		if dependency.SHA256 != expected {
			return false
		}
		found = true
	}
	return found
}

func nativeModelResourceType(typeName string) bool {
	switch typeName {
	case "UnityEngine.Mesh", "UnityEngine.Material", "UnityEngine.AnimationClip", "UnityEngine.Avatar", "UnityEngine.Texture2D":
		return true
	}
	return false
}

func (s *Session) excludedNativeModelObject(guid, id string) bool {
	for _, item := range s.catalog.ExcludedRuntimeAssets {
		if str(item["guid"]) == guid && str(item["fileId"]) == id {
			return true
		}
	}
	return false
}

func (s *Session) recordIgnoredPrefabOverride(proof *PrefabOverrideDiagnostic, path, instanceID, guid, fileID, property string) {
	if proof == nil {
		return
	}
	value := *proof
	value.Path, value.InstanceFileID, value.TargetGUID, value.TargetFileID, value.PropertyPath = path, instanceID, guid, fileID, property
	for _, existing := range s.ignoredPrefabOverrideDiagnostics {
		if existing == value {
			return
		}
	}
	s.ignoredPrefabOverrideDiagnostics = append(s.ignoredPrefabOverrideDiagnostics, value)
}

func (s *Session) sortedIgnoredPrefabOverrideDiagnostics() []PrefabOverrideDiagnostic {
	result := append([]PrefabOverrideDiagnostic{}, s.ignoredPrefabOverrideDiagnostics...)
	sort.Slice(result, func(i, j int) bool {
		first, second := result[i], result[j]
		return strings.Join([]string{first.Path, first.InstanceFileID, first.TargetGUID, first.TargetFileID, first.PropertyPath}, "\x00") <
			strings.Join([]string{second.Path, second.InstanceFileID, second.TargetGUID, second.TargetFileID, second.PropertyPath}, "\x00")
	})
	return result
}
