package authoring

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"unicode/utf8"
)

var regexpField = regexp.MustCompile(`^[A-Za-z_][A-Za-z0-9_]*$`)
var scriptGUID = regexp.MustCompile(`^[0-9a-f]{32}$`)

type ScriptRequest struct {
	Path             string `json:"path"`
	ExpectedRevision string `json:"expectedRevision" jsonschema:"use absent only when creating a new script"`
	OperationID      string `json:"operationId"`
	Content          string `json:"content"`
	GUID             string `json:"guid,omitempty" jsonschema:"optional lowercase 32-hex GUID for a newly created script"`
}

func validateScript(path, content string) error {
	base := strings.TrimSuffix(filepath.Base(path), ".cs")
	if !regexpField.MatchString(base) {
		return failure("invalid_script", "script filename must be a valid simple C# identifier", nil)
	}
	if len(content) == 0 || len(content) > 1<<20 || !utf8.ValidString(content) || strings.ContainsRune(content, '\x00') {
		return failure("invalid_script", "script content must be nonempty UTF-8 without NUL, at most 1 MiB", nil)
	}
	// Syntax and assembly graph checks belong to the SDK compiler; do not claim
	// that successfully writing source implies it compiled or attached correctly.
	return nil
}
func (s *Store) ReadScript(ctx context.Context, path string) (Source, error) {
	if _, err := s.assetPath(path, ".cs"); err != nil {
		return Source{}, err
	}
	return s.Read(ctx, path)
}
func (s *Store) CreateScript(ctx context.Context, request ScriptRequest) (MutationResult, error) {
	if _, err := s.assetPath(request.Path, ".cs"); err != nil {
		return MutationResult{}, err
	}
	if err := validateScript(request.Path, request.Content); err != nil {
		return MutationResult{}, err
	}
	if request.ExpectedRevision != "absent" {
		return MutationResult{}, failure("invalid_request", "new script creation requires expectedRevision absent", nil)
	}
	if request.GUID != "" && (!scriptGUID.MatchString(request.GUID) || request.GUID == strings.Repeat("0", 32)) {
		return MutationResult{}, failure("invalid_identity", "new script GUID must be nonzero lowercase hexadecimal", nil)
	}
	return s.mutate(ctx, request.Path, request.ExpectedRevision, request.OperationID, request, func() (mutationPlan, error) {
		path, err := s.assetPath(request.Path, ".cs")
		if err != nil {
			return mutationPlan{}, err
		}
		for _, candidate := range []string{path, path + ".meta"} {
			if _, err := os.Lstat(candidate); err == nil {
				return mutationPlan{}, failure("revision_conflict", "new script or its metadata already exists", map[string]any{"path": request.Path, "actualRevision": "present"})
			} else if !errors.Is(err, os.ErrNotExist) {
				return mutationPlan{}, err
			}
		}
		parent := filepath.Dir(path)
		info, err := os.Stat(parent)
		if err != nil || !info.IsDir() {
			return mutationPlan{}, failure("invalid_path", "script parent directory must already exist", nil)
		}
		guid := request.GUID
		if guid == "" {
			var random [16]byte
			if _, err := rand.Read(random[:]); err != nil {
				return mutationPlan{}, err
			}
			guid = hex.EncodeToString(random[:])
		}
		if err := s.checkGUIDAvailable(guid); err != nil {
			return mutationPlan{}, err
		}
		meta := "fileFormatVersion: 2\nguid: " + guid + "\nMonoImporter:\n  externalObjects: {}\n  serializedVersion: 2\n  defaultReferences: []\n  executionOrder: 0\n  icon: {fileID: 0}\n  userData: \n  assetBundleName: \n  assetBundleVariant: \n"
		return mutationPlan{source: Source{Path: request.Path, Revision: "absent"}, content: []byte(request.Content), metadata: []byte(meta), changes: []Change{{Action: "create_script", After: request.Path}}}, nil
	})
}
func (s *Store) EditScript(ctx context.Context, request ScriptRequest) (MutationResult, error) {
	if _, err := s.assetPath(request.Path, ".cs"); err != nil {
		return MutationResult{}, err
	}
	if err := validateScript(request.Path, request.Content); err != nil {
		return MutationResult{}, err
	}
	if request.GUID != "" {
		return MutationResult{}, failure("protected_property", "editing a script preserves its existing metadata GUID", nil)
	}
	return s.mutate(ctx, request.Path, request.ExpectedRevision, request.OperationID, request, func() (mutationPlan, error) {
		source, err := s.readSource(request.Path)
		if err != nil {
			return mutationPlan{}, err
		}
		return mutationPlan{source: source, content: []byte(request.Content), metadata: []byte(source.Metadata), changes: []Change{{Action: "edit_script"}}}, nil
	})
}
func (s *Store) checkGUIDAvailable(guid string) error {
	return filepath.WalkDir(filepath.Join(s.root, "Assets"), func(path string, entry os.DirEntry, walkErr error) error {
		if walkErr != nil {
			return walkErr
		}
		if entry.IsDir() || entry.Type()&os.ModeSymlink != 0 || !strings.HasSuffix(path, ".meta") {
			return nil
		}
		data, err := readLimited(path)
		if err != nil {
			return err
		}
		for _, match := range guidPattern.FindAllSubmatch(data, -1) {
			if strings.ToLower(string(match[1])) == guid {
				relative, _ := filepath.Rel(s.root, path)
				return failure("duplicate_guid", "new script GUID is already used", map[string]any{"guid": guid, "path": filepath.ToSlash(relative)})
			}
		}
		return nil
	})
}
