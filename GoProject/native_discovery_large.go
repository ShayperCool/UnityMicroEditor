package goproject

import (
	"encoding/json"
	"fmt"
	"path/filepath"
	"strings"
	"syscall"
)

// A cooked AnimationClip may exceed the ordinary text parser bound. Discovery
// still scans bounded lines and verifies its entire exact source revision.
const maxCookedNativeDiscoveryBytes int64 = 1 << 30

type nativeDiscoveryPack struct {
	BundlePath    string `json:"bundlePath"`
	SHA256        string `json:"sha256"`
	Bytes         int64  `json:"bytes"`
	RegistryAsset string `json:"registryAsset"`
}

type largeNativeDiscoveryProof struct {
	logical, sourceSHA string
	observed           map[string]identity
}

func (s *Session) verifiedLargeNativeDiscovery(path string, size int64) (*largeNativeDiscoveryProof, error) {
	if size > maxCookedNativeDiscoveryBytes || !strings.EqualFold(filepath.Ext(path), ".anim") ||
		!s.catalog.Success || !s.catalog.CookedRegistryVerified || s.catalog.ContainsScenes || s.catalog.SceneLoader != "raw" ||
		(s.catalog.Backend != "unity_editor_public_api" && s.catalog.Backend != "unity_editor") {
		return nil, fmt.Errorf("unsupported oversized native discovery source without current cooked AnimationClip proof: %s", path)
	}
	var logical, guid string
	for _, asset := range s.catalog.Assets {
		if !strings.EqualFold(filepath.Ext(str(asset["path"])), ".anim") {
			continue
		}
		physical, err := s.source(str(asset["path"]))
		if err != nil || physical != path {
			continue
		}
		if logical != "" && (logical != str(asset["path"]) || guid != str(asset["guid"])) ||
			asset["kind"] != "imported" || number(asset["classId"]) != 74 || str(asset["typeName"]) != "UnityEngine.AnimationClip" ||
			!validNativeGUID(str(asset["guid"])) || !validNativeID(str(asset["fileId"])) || s.byPath[str(asset["path"])] != str(asset["guid"]) {
			return nil, fmt.Errorf("oversized native discovery AnimationClip identity is invalid: %s", path)
		}
		logical, guid = str(asset["path"]), str(asset["guid"])
	}
	if logical == "" {
		return nil, fmt.Errorf("oversized native discovery source is absent from the cooked AnimationClip inventory: %s", path)
	}
	proof := &largeNativeDiscoveryProof{logical: logical, observed: map[string]identity{}}
	var metaSHA string
	for _, dependency := range s.catalog.SourceDependencies {
		if dependency.Path == logical {
			if dependency.Bytes != size || !validNativeHex(dependency.SHA256, 64) || proof.sourceSHA != "" && proof.sourceSHA != dependency.SHA256 {
				return nil, fmt.Errorf("oversized native discovery source revision/size differs from its cooked catalog: %s", logical)
			}
			proof.sourceSHA = dependency.SHA256
		} else if dependency.Path == logical+".meta" {
			if !validNativeHex(dependency.SHA256, 64) || metaSHA != "" && metaSHA != dependency.SHA256 {
				return nil, fmt.Errorf("oversized native discovery metadata proof is invalid: %s", logical)
			}
			metaSHA = dependency.SHA256
		}
	}
	if proof.sourceSHA == "" || metaSHA == "" || s.critical[logical] != proof.sourceSHA || s.critical[logical+".meta"] != metaSHA {
		return nil, fmt.Errorf("oversized native discovery source lacks exact cooked source/metadata hashes: %s", logical)
	}
	metaPath, err := s.source(logical + ".meta")
	if err != nil {
		return nil, err
	}
	meta, actualMetaSHA, err := s.read(metaPath, true)
	if err != nil {
		return nil, err
	}
	match := guidLine.FindSubmatch(meta)
	if actualMetaSHA != metaSHA || match == nil || strings.ToLower(string(match[1])) != guid {
		return nil, &NeedsImportError{Path: logical + ".meta"}
	}
	proof.observed[metaPath] = s.observed[metaPath]
	var baselinePacks []nativeDiscoveryPack
	encoded, err := json.Marshal(s.baseline["importedAssetPacks"])
	if err == nil {
		err = json.Unmarshal(encoded, &baselinePacks)
	}
	if err != nil || len(baselinePacks) == 0 || len(baselinePacks) != len(s.catalog.Packs) || len(baselinePacks) > 1024 {
		return nil, fmt.Errorf("oversized native discovery source lacks bound cooked registry packs: %s", logical)
	}
	seen := map[string]bool{}
	for i, pack := range s.catalog.Packs {
		if pack != baselinePacks[i] || !IsOwnedCachePath(s.project, pack.BundlePath) || !validNativeHex(pack.SHA256, 64) ||
			pack.Bytes <= 0 || pack.RegistryAsset == "" || seen[pack.BundlePath] {
			return nil, fmt.Errorf("oversized native discovery cooked pack descriptor is invalid: %s", logical)
		}
		seen[pack.BundlePath] = true
		canonical, err := filepath.EvalSymlinks(pack.BundlePath)
		if err != nil || canonical != pack.BundlePath {
			return nil, fmt.Errorf("oversized native discovery cooked pack is missing or symlinked: %s", pack.BundlePath)
		}
		stamp, err := fileIdentity(pack.BundlePath)
		if err != nil || stamp.Mode&syscall.S_IFMT != syscall.S_IFREG || stamp.Size != pack.Bytes {
			return nil, fmt.Errorf("oversized native discovery cooked pack size/type differs: %s", pack.BundlePath)
		}
		_, actualSHA, err := s.read(pack.BundlePath, false)
		if err != nil {
			return nil, err
		}
		if actualSHA != pack.SHA256 {
			return nil, fmt.Errorf("oversized native discovery cooked registry pack changed: %s", pack.BundlePath)
		}
		proof.observed[pack.BundlePath] = s.observed[pack.BundlePath]
	}
	return proof, nil
}

func (proof *largeNativeDiscoveryProof) validateCurrent(sourceSHA string) error {
	if sourceSHA != proof.sourceSHA {
		return &NeedsImportError{Path: proof.logical}
	}
	for path, before := range proof.observed {
		after, err := fileIdentity(path)
		if err != nil || before != after {
			return fmt.Errorf("cooked native discovery proof changed while scanning, retry: %s", path)
		}
	}
	return nil
}
