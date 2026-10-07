package authoring

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
)

type UndoRequest struct {
	Path                string `json:"path"`
	OriginalOperationID string `json:"originalOperationId"`
	ExpectedRevision    string `json:"expectedRevision"`
	OperationID         string `json:"operationId"`
}

// Undo restores an existing authored file only when it still has exactly the
// revision produced by the original operation. It never erases intervening edits.
// New-file creation cannot be undone through this API because deleting a script
// also requires checking Unity compiler and cross-project dependencies.
func (s *Store) Undo(ctx context.Context, request UndoRequest) (MutationResult, error) {
	if _, err := s.assetPath(request.Path, ".unity", ".prefab", ".asset", ".mat", ".cs"); err != nil {
		return MutationResult{}, err
	}
	if request.OriginalOperationID == "" || len(request.OriginalOperationID) > 128 {
		return MutationResult{}, failure("invalid_request", "originalOperationId is required", nil)
	}
	return s.mutate(ctx, request.Path, request.ExpectedRevision, request.OperationID, request, func() (mutationPlan, error) {
		source, err := s.readSource(request.Path)
		if err != nil {
			return mutationPlan{}, err
		}
		hash := sha256.Sum256([]byte(request.OriginalOperationID))
		data, err := os.ReadFile(filepath.Join(s.state, hex.EncodeToString(hash[:])+".json"))
		if os.IsNotExist(err) {
			return mutationPlan{}, failure("not_found", "original operation journal is absent", nil)
		}
		if err != nil {
			return mutationPlan{}, err
		}
		var original journal
		if err := json.Unmarshal(data, &original); err != nil {
			return mutationPlan{}, err
		}
		if original.State != "committed" || original.Result.Path != request.Path {
			return mutationPlan{}, failure("invalid_request", "original operation did not commit the requested file", nil)
		}
		if source.Revision != original.Result.Revision {
			return mutationPlan{}, failure("revision_conflict", "undo would overwrite changes made after the original operation", map[string]any{"actualRevision": source.Revision, "originalRevision": original.Result.Revision})
		}
		content, metadata := []byte(source.Content), []byte(source.Metadata)
		for _, file := range original.Files {
			if !file.Existed {
				return mutationPlan{}, failure("unsupported_undo", "new-file creation cannot be undone through source_undo", nil)
			}
			switch file.Path {
			case request.Path:
				content = file.Before
			case request.Path + ".meta":
				metadata = file.Before
			default:
				return mutationPlan{}, failure("invalid_request", "original operation includes an unexpected file", nil)
			}
		}
		if !strings.HasSuffix(request.Path, ".cs") {
			current, err := parseAsset([]byte(source.Content))
			if err != nil {
				return mutationPlan{}, err
			}
			restored, err := parseAsset(content)
			if err != nil {
				return mutationPlan{}, err
			}
			if err := restored.validate(source.GUID); err != nil {
				return mutationPlan{}, err
			}
			if err := restored.preservePrefabDocuments(current); err != nil {
				return mutationPlan{}, err
			}
			removed := map[string]bool{}
			for _, doc := range current.docs {
				if restored.find(doc.id) == nil {
					removed[doc.id] = true
				}
			}
			if len(removed) != 0 {
				if err := s.checkExternalReferences(source, removed); err != nil {
					return mutationPlan{}, err
				}
			}
		}
		return mutationPlan{source: source, content: content, metadata: metadata, changes: []Change{{Action: "undo", Before: request.OriginalOperationID}}}, nil
	})
}
