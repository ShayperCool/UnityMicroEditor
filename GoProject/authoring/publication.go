package authoring

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"io"
	"os"
	"path/filepath"
	"strings"
	"unicode/utf8"
)

type PublicationInput struct {
	Path             string `json:"path"`
	ExpectedRevision string `json:"expectedRevision"`
}

type PublicationOutput struct {
	Path     string  `json:"path"`
	Content  string  `json:"content"`
	Metadata *string `json:"metadata,omitempty"`
}

type StagedPublicationRequest struct {
	OperationID   string              `json:"operationId"`
	Inputs        []PublicationInput  `json:"inputs"`
	Outputs       []PublicationOutput `json:"outputs"`
	BeforePublish func() error        `json:"-"`
}

type PublicationResult struct {
	OperationID string           `json:"operationId"`
	Assets      []MutationResult `json:"assets"`
	Changed     bool             `json:"changed"`
	Replayed    bool             `json:"replayed,omitempty"`
}

func dependencyPath(path string) bool {
	return strings.HasPrefix(path, "Assets/") || strings.HasPrefix(path, "ProjectSettings/") || strings.HasPrefix(path, "Packages/")
}

func hashRegular(path string) ([]byte, error) {
	file, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer file.Close()
	info, err := file.Stat()
	if err != nil {
		return nil, err
	}
	if !info.Mode().IsRegular() {
		return nil, failure("invalid_path", "dependency must be a regular file", nil)
	}
	hash := sha256.New()
	if _, err := io.Copy(hash, file); err != nil {
		return nil, err
	}
	return hash.Sum(nil), nil
}

func (s *Store) dependencyRevision(path string) (string, error) {
	if !dependencyPath(path) || strings.HasSuffix(path, ".meta") {
		return "", failure("invalid_path", "dependency must be a source file under Assets, ProjectSettings or Packages", nil)
	}
	absolute, err := s.safePath(path, false)
	if err != nil {
		return "", err
	}
	metaPath, err := s.safePath(path+".meta", false)
	if err != nil {
		return "", err
	}
	contentHash, err := hashRegular(absolute)
	if errors.Is(err, os.ErrNotExist) {
		if _, metaErr := os.Lstat(metaPath); errors.Is(metaErr, os.ErrNotExist) {
			return "absent", nil
		}
		return "", failure("invalid_identity", "absent source has existing metadata", map[string]any{"path": path})
	}
	if err != nil {
		return "", err
	}
	metaHash, err := hashRegular(metaPath)
	if errors.Is(err, os.ErrNotExist) {
		empty := sha256.Sum256(nil)
		metaHash = empty[:]
	} else if err != nil {
		return "", err
	}
	hash := sha256.New()
	hash.Write([]byte("unitymicroeditor-authored-v1\x00"))
	hash.Write(contentHash)
	hash.Write(metaHash)
	return hex.EncodeToString(hash.Sum(nil)), nil
}

// DependencyRevision hashes binary or text source plus optional .meta without
// reading large files into memory. It never reads Library or tool installation
// files, and has the same digest as Read for supported authored text assets.
func (s *Store) DependencyRevision(ctx context.Context, path string) (result string, err error) {
	err = s.locked(ctx, func() error {
		var readErr error
		result, readErr = s.dependencyRevision(path)
		return readErr
	})
	return
}

// PublishStaged is a publication boundary, not an Editor execution service.
// Every dependency is checked before any write; staged content is validated and
// published through the same recoverable journal as source edits. Readers outside
// Store must stay closed during publication: POSIX cannot atomically rename many
// files at once, and their partial visibility is not presented as isolation.
func (s *Store) PublishStaged(ctx context.Context, request StagedPublicationRequest) (result PublicationResult, err error) {
	if request.OperationID == "" || len(request.OperationID) > 128 || strings.TrimSpace(request.OperationID) != request.OperationID || len(request.Inputs) == 0 || len(request.Inputs) > 4096 || len(request.Outputs) == 0 || len(request.Outputs) > 128 {
		return result, failure("invalid_request", "staged publication requires operationId, 1..4096 inputs and 1..128 outputs", nil)
	}
	inputs := map[string]string{}
	for _, input := range request.Inputs {
		if input.ExpectedRevision == "" || inputs[input.Path] != "" {
			return result, failure("invalid_request", "each input must have a unique path and expectedRevision", nil)
		}
		if !dependencyPath(input.Path) {
			return result, failure("invalid_path", "publication input is outside project source roots", nil)
		}
		inputs[input.Path] = input.ExpectedRevision
	}
	seen := map[string]bool{}
	total := 0
	for _, output := range request.Outputs {
		if _, err := s.assetPath(output.Path, ".unity", ".prefab", ".mat", ".asset", ".cs"); err != nil {
			return result, err
		}
		if seen[output.Path] || inputs[output.Path] == "" {
			return result, failure("invalid_request", "every output requires one corresponding input revision", map[string]any{"path": output.Path})
		}
		seen[output.Path] = true
		total += len(output.Content)
		if output.Metadata != nil {
			total += len(*output.Metadata)
		}
		if len(output.Content) == 0 || len(output.Content) > maxSourceBytes || !utf8.ValidString(output.Content) || strings.ContainsRune(output.Content, '\x00') || total > 64<<20 {
			return result, failure("invalid_request", "outputs must be UTF-8 text without NUL, at most 32 MiB each and 64 MiB total", nil)
		}
		if output.Metadata != nil && (len(*output.Metadata) > maxSourceBytes || !utf8.ValidString(*output.Metadata)) {
			return result, failure("invalid_identity", "staged metadata must be UTF-8 at most 32 MiB", nil)
		}
	}
	encoded, err := json.Marshal(request)
	if err != nil {
		return result, err
	}
	digest := sha256.Sum256(encoded)
	fingerprint := hex.EncodeToString(digest[:])
	idDigest := sha256.Sum256([]byte(request.OperationID))
	journalPath := filepath.Join(s.state, hex.EncodeToString(idDigest[:])+".json")
	err = s.locked(ctx, func() error {
		version, err := s.currentVersion()
		if err != nil {
			return err
		}
		if version != VerifiedUnityVersion {
			return failure("unsupported_unity_version", "staged publication requires the verified Unity profile", nil)
		}
		if data, readErr := os.ReadFile(journalPath); readErr == nil {
			var original journal
			if err := json.Unmarshal(data, &original); err != nil {
				return err
			}
			if original.Fingerprint != fingerprint || original.BatchResult == nil {
				return failure("idempotency_conflict", "operationId was already used for different arguments", nil)
			}
			if original.State != "committed" {
				return failure("rolled_back", "interrupted staged publication was rolled back; use a new operationId", nil)
			}
			result = *original.BatchResult
			result.Replayed = true
			return nil
		} else if !errors.Is(readErr, os.ErrNotExist) {
			return readErr
		}
		checkInputs := func() error {
			for _, input := range request.Inputs {
				actual, err := s.dependencyRevision(input.Path)
				if err != nil {
					return err
				}
				if actual != input.ExpectedRevision {
					return failure("revision_conflict", "staged authoring input changed", map[string]any{"path": input.Path, "expectedRevision": input.ExpectedRevision, "actualRevision": actual})
				}
			}
			return nil
		}
		if err := checkInputs(); err != nil {
			return err
		}
		result = PublicationResult{OperationID: request.OperationID, Assets: []MutationResult{}}
		transaction := journal{OperationID: request.OperationID, Fingerprint: fingerprint, State: "prepared"}
		overlay := map[string]Source{}
		oldSources := map[string]Source{}
		newGUIDs := map[string]string{}
		for _, output := range request.Outputs {
			if err := ctx.Err(); err != nil {
				return err
			}
			source := Source{Path: output.Path, Revision: "absent"}
			if inputs[output.Path] != "absent" {
				source, err = s.readSource(output.Path)
				if err != nil {
					return err
				}
			}
			oldSources[output.Path] = source
			metadata := source.Metadata
			if output.Metadata != nil {
				metadata = *output.Metadata
			}
			if source.Revision != "absent" && metadata != source.Metadata {
				return failure("protected_property", "staged publication must preserve existing metadata byte-for-byte", map[string]any{"path": output.Path})
			}
			matches := guidPattern.FindAllStringSubmatch(metadata, -1)
			if len(matches) != 1 || matches[0][1] == strings.Repeat("0", 32) {
				return failure("invalid_identity", "each staged asset requires exactly one nonzero metadata GUID", map[string]any{"path": output.Path})
			}
			guid := strings.ToLower(matches[0][1])
			if source.Revision == "absent" {
				if err := s.checkGUIDAvailable(guid); err != nil {
					return err
				}
				if previous := newGUIDs[guid]; previous != "" {
					return failure("duplicate_guid", "staged outputs reuse one GUID", map[string]any{"path": output.Path, "otherPath": previous})
				}
				newGUIDs[guid] = output.Path
			}
			if filepath.Ext(output.Path) == ".cs" {
				if err := validateScript(output.Path, output.Content); err != nil {
					return err
				}
			} else {
				asset, err := parseAsset([]byte(output.Content))
				if err != nil {
					return err
				}
				if err := asset.validate(guid); err != nil {
					return err
				}
				if filepath.Ext(output.Path) == ".prefab" {
					if err := asset.validatePrefabRoot(); err != nil {
						return err
					}
				}
			}
			next := Source{Path: output.Path, GUID: guid, Revision: revision([]byte(output.Content), []byte(metadata)), Content: output.Content, Metadata: metadata}
			overlay[output.Path] = next
			assetResult := MutationResult{OperationID: request.OperationID, Path: output.Path, GUID: guid, PreviousRevision: source.Revision, Revision: next.Revision, Changed: source.Content != output.Content || source.Metadata != metadata}
			result.Assets = append(result.Assets, assetResult)
			result.Changed = result.Changed || assetResult.Changed
			for _, item := range []struct{ path, before, after string }{{output.Path, source.Content, output.Content}, {output.Path + ".meta", source.Metadata, metadata}} {
				absolute, err := s.safePath(item.path, false)
				if err != nil {
					return err
				}
				parentInfo, err := os.Stat(filepath.Dir(absolute))
				if err != nil || !parentInfo.IsDir() {
					return failure("invalid_path", "output parent directory must already exist", map[string]any{"path": item.path})
				}
				if source.Revision == "absent" || item.before != item.after {
					mode := uint32(0644)
					if info, err := os.Stat(absolute); err == nil {
						mode = uint32(info.Mode().Perm())
					}
					transaction.Files = append(transaction.Files, journalFile{Path: item.path, Before: []byte(item.before), After: []byte(item.after), Existed: source.Revision != "absent", Mode: mode})
				}
			}
		}
		for path, source := range oldSources {
			if source.Revision == "absent" || filepath.Ext(path) == ".cs" {
				continue
			}
			before, err := parseAsset([]byte(source.Content))
			if err != nil {
				return err
			}
			after, _ := parseAsset([]byte(overlay[path].Content))
			removed := map[string]bool{}
			for _, document := range before.docs {
				if after.find(document.id) == nil {
					removed[document.id] = true
				}
			}
			if len(removed) > 0 {
				if err := s.checkExternalReferencesOverlay(source, removed, overlay); err != nil {
					return err
				}
			}
		}
		transaction.BatchResult = &result
		if request.BeforePublish != nil {
			if err := request.BeforePublish(); err != nil {
				return err
			}
		}
		if err := checkInputs(); err != nil {
			return err
		}
		if err := writeJournal(journalPath, transaction); err != nil {
			return err
		}
		for _, file := range transaction.Files {
			absolute, err := s.safePath(file.Path, false)
			if err != nil {
				return err
			}
			if err := atomicWrite(absolute, file.After, os.FileMode(file.Mode)); err != nil {
				return failure("outcome_unknown", "staged publication interrupted; retry recovers the journal", map[string]any{"operationId": request.OperationID, "cause": err.Error()})
			}
		}
		transaction.State = "committed"
		if err := writeJournal(journalPath, transaction); err != nil {
			return failure("outcome_unknown", "staged bytes written but receipt was not committed", map[string]any{"operationId": request.OperationID})
		}
		return nil
	})
	return
}
