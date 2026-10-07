package authoring

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"os"
	"path/filepath"
)

type PublicationUndoRequest struct {
	OperationID         string            `json:"operationId"`
	OriginalOperationID string            `json:"originalOperationId"`
	ExpectedRevisions   map[string]string `json:"expectedRevisions"`
}

// UndoPublication restores all changed existing-file assets in one journaled
// transaction. It deliberately cannot erase assets created by the original
// operation; source/compiler dependencies may still reference those identities.
func (s *Store) UndoPublication(ctx context.Context, request PublicationUndoRequest) (PublicationResult, error) {
	var original journal
	err := s.locked(ctx, func() error {
		if request.OriginalOperationID == "" || len(request.OriginalOperationID) > 128 {
			return failure("invalid_request", "originalOperationId is required", nil)
		}
		digest := sha256.Sum256([]byte(request.OriginalOperationID))
		data, err := os.ReadFile(filepath.Join(s.state, hex.EncodeToString(digest[:])+".json"))
		if os.IsNotExist(err) {
			return failure("not_found", "original publication journal is absent", nil)
		}
		if err != nil {
			return err
		}
		if err := json.Unmarshal(data, &original); err != nil {
			return err
		}
		if original.State != "committed" || original.BatchResult == nil {
			return failure("invalid_request", "original operation is not a committed multi-file publication", nil)
		}
		return nil
	})
	if err != nil {
		return PublicationResult{}, err
	}
	publication := StagedPublicationRequest{OperationID: request.OperationID}
	revisions := map[string]string{}
	for _, asset := range original.BatchResult.Assets {
		revisions[asset.Path] = asset.Revision
	}
	for _, file := range original.Files {
		if !file.Existed {
			return PublicationResult{}, failure("unsupported_undo", "new-file creation cannot be erased by publication undo", nil)
		}
		if filepath.Ext(file.Path) == ".meta" {
			continue
		}
		if request.ExpectedRevisions[file.Path] != revisions[file.Path] {
			return PublicationResult{}, failure("revision_conflict", "undo requires the exact original publication revisions", map[string]any{"path": file.Path, "originalRevision": revisions[file.Path]})
		}
		publication.Inputs = append(publication.Inputs, PublicationInput{Path: file.Path, ExpectedRevision: revisions[file.Path]})
		publication.Outputs = append(publication.Outputs, PublicationOutput{Path: file.Path, Content: string(file.Before)})
	}
	if len(publication.Outputs) == 0 {
		return PublicationResult{}, failure("unsupported_undo", "original publication changed no existing source files", nil)
	}
	return s.PublishStaged(ctx, publication)
}
