package authoring

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"unicode/utf8"
)

type AssetCreateRequest struct {
	Path             string `json:"path"`
	ExpectedRevision string `json:"expectedRevision"`
	OperationID      string `json:"operationId"`
	Content          string `json:"content"`
	GUID             string `json:"guid,omitempty"`
}

// CreateAsset accepts explicit authored serialization rather than inventing
// shader, ScriptableObject or prefab defaults. Normal source editing can then
// change scalar fields, reference mappings and nested serialized collections.
func (s *Store) CreateAsset(ctx context.Context, request AssetCreateRequest) (MutationResult, error) {
	if _, err := s.assetPath(request.Path, ".mat", ".asset", ".prefab"); err != nil {
		return MutationResult{}, err
	}
	if request.ExpectedRevision != "absent" || len(request.Content) == 0 || len(request.Content) > maxSourceBytes || !utf8.ValidString(request.Content) || strings.ContainsRune(request.Content, '\x00') {
		return MutationResult{}, failure("invalid_request", "asset creation requires expectedRevision absent and nonempty Unity text at most 32 MiB", nil)
	}
	if request.GUID != "" && (!scriptGUID.MatchString(request.GUID) || request.GUID == strings.Repeat("0", 32)) {
		return MutationResult{}, failure("invalid_identity", "new asset GUID must be nonzero lowercase hexadecimal", nil)
	}
	return s.mutate(ctx, request.Path, request.ExpectedRevision, request.OperationID, request, func() (mutationPlan, error) {
		absolute, err := s.assetPath(request.Path, ".mat", ".asset", ".prefab")
		if err != nil {
			return mutationPlan{}, err
		}
		for _, path := range []string{absolute, absolute + ".meta"} {
			if _, err := os.Lstat(path); err == nil {
				return mutationPlan{}, failure("revision_conflict", "new asset or its metadata already exists", nil)
			} else if !errors.Is(err, os.ErrNotExist) {
				return mutationPlan{}, err
			}
		}
		if info, err := os.Stat(filepath.Dir(absolute)); err != nil || !info.IsDir() {
			return mutationPlan{}, failure("invalid_path", "asset parent directory must already exist", nil)
		}
		asset, err := parseAsset([]byte(request.Content))
		if err != nil {
			return mutationPlan{}, err
		}
		guid := request.GUID
		if guid == "" {
			var bytes [16]byte
			if _, err := rand.Read(bytes[:]); err != nil {
				return mutationPlan{}, err
			}
			guid = hex.EncodeToString(bytes[:])
		}
		if err := s.checkGUIDAvailable(guid); err != nil {
			return mutationPlan{}, err
		}
		if err := asset.validate(guid); err != nil {
			return mutationPlan{}, err
		}
		if filepath.Ext(request.Path) == ".prefab" {
			if err := asset.validatePrefabRoot(); err != nil {
				return mutationPlan{}, err
			}
		}
		mainID := ""
		for _, document := range asset.docs {
			switch filepath.Ext(request.Path) {
			case ".mat":
				if document.classID == 21 && mainID == "" {
					mainID = document.id
				} else if document.classID != 114 || document.data["m_EditorClassIdentifier"] != "Unity.RenderPipelines.Universal.Editor::UnityEditor.Rendering.Universal.AssetVersion" {
					return mutationPlan{}, failure("unsupported_asset", "material requires one class21 object and only supported version subassets", nil)
				}
			case ".asset":
				if document.classID != 114 || idReference(document.data["m_GameObject"]) != "0" || idReference(document.data["m_Script"]) == "0" {
					return mutationPlan{}, failure("unsupported_asset", "ScriptableObject asset requires class114 objects with explicit MonoScript references and no GameObject owner", nil)
				}
				if mainID == "" {
					mainID = document.id
				}
			case ".prefab":
				if document.classID == 1 && !document.stripped && mainID == "" {
					mainID = document.id
				}
				if document.classID == 1001 && mainID == "" {
					mainID = document.id
				}
			}
		}
		if mainID == "" {
			return mutationPlan{}, failure("unsupported_asset", "asset has no supported main object", nil)
		}
		importer := fmt.Sprintf("NativeFormatImporter:\n  externalObjects: {}\n  mainObjectFileID: %s\n  userData: \n  assetBundleName: \n  assetBundleVariant: \n", mainID)
		if filepath.Ext(request.Path) == ".prefab" {
			importer = "PrefabImporter:\n  externalObjects: {}\n  userData: \n  assetBundleName: \n  assetBundleVariant: \n"
		}
		metadata := "fileFormatVersion: 2\nguid: " + guid + "\n" + importer
		return mutationPlan{source: Source{Path: request.Path, Revision: "absent"}, content: []byte(request.Content), metadata: []byte(metadata), changes: []Change{{Action: "create_asset", After: request.Path}}}, nil
	})
}
