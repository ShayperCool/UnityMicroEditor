package authoring

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"
)

type InventoryRequest struct {
	Query                 string   `json:"query,omitempty"`
	PathPrefix            string   `json:"pathPrefix,omitempty"`
	Extensions            []string `json:"extensions,omitempty"`
	Offset                int      `json:"offset,omitempty"`
	Limit                 int      `json:"limit,omitempty"`
	ExpectedRevision      string   `json:"expectedRevision,omitempty"`
	IncludeSourceRevision bool     `json:"includeSourceRevision,omitempty"`
}

type InventoryEntry struct {
	Path             string `json:"path"`
	Extension        string `json:"extension"`
	GUID             string `json:"guid,omitempty"`
	Size             int64  `json:"size"`
	ModifiedUTC      string `json:"modifiedUtc"`
	MetadataRevision string `json:"metadataRevision,omitempty"`
	SourceRevision   string `json:"sourceRevision,omitempty"`
	IdentityIssue    string `json:"identityIssue,omitempty"`
}

type InventoryPage struct {
	Project      string           `json:"project"`
	UnityVersion string           `json:"unityVersion"`
	Revision     string           `json:"revision"`
	Entries      []InventoryEntry `json:"entries"`
	Total        int              `json:"total"`
	Offset       int              `json:"offset"`
	NextOffset   *int             `json:"nextOffset"`
}

// Inventory pages all ordinary Assets files, including native source inputs.
// Its snapshot revision binds names, stat values and metadata bytes, rather than
// claiming a content hash for large binary files. SourceRevision, when requested,
// is the exact source+metadata revision used by supported authoring APIs.
func (s *Store) Inventory(ctx context.Context, request InventoryRequest) (result InventoryPage, err error) {
	if request.Offset < 0 || request.Limit < 0 || request.Limit > 500 || len(request.Query) > 1024 || len(request.Extensions) > 64 {
		return result, failure("invalid_request", "inventory offset must be nonnegative and limit at most 500", nil)
	}
	if request.Limit == 0 {
		request.Limit = 100
	}
	if request.PathPrefix != "" {
		if request.PathPrefix != "Assets" && !strings.HasPrefix(request.PathPrefix, "Assets/") {
			return result, failure("invalid_path", "inventory pathPrefix must be Assets or a normalized child", nil)
		}
		if _, err := s.safePath(request.PathPrefix, true); err != nil {
			return result, err
		}
	}
	extensions := map[string]bool{}
	for _, extension := range request.Extensions {
		extension = strings.ToLower(extension)
		if len(extension) < 2 || len(extension) > 64 || extension[0] != '.' || strings.ContainsAny(extension, "/\\") {
			return result, failure("invalid_request", "inventory extensions must use .extension syntax", nil)
		}
		extensions[extension] = true
	}
	err = s.locked(ctx, func() error {
		version, err := s.currentVersion()
		if err != nil {
			return err
		}
		entries := []InventoryEntry{}
		guids := map[string][]int{}
		err = filepath.WalkDir(filepath.Join(s.root, "Assets"), func(path string, entry os.DirEntry, walkErr error) error {
			if err := ctx.Err(); err != nil {
				return err
			}
			if walkErr != nil {
				return walkErr
			}
			if entry.Type()&os.ModeSymlink != 0 || entry.IsDir() || strings.HasSuffix(path, ".meta") {
				return nil
			}
			info, err := entry.Info()
			if err != nil {
				return err
			}
			if !info.Mode().IsRegular() {
				return nil
			}
			relative, _ := filepath.Rel(s.root, path)
			relative = filepath.ToSlash(relative)
			item := InventoryEntry{Path: relative, Extension: strings.ToLower(filepath.Ext(relative)), Size: info.Size(), ModifiedUTC: info.ModTime().UTC().Format(time.RFC3339Nano)}
			metaPath, err := s.safePath(relative+".meta", false)
			if err != nil {
				item.IdentityIssue = err.Error()
			} else if meta, err := readLimited(metaPath); err == nil {
				digest := sha256.Sum256(meta)
				item.MetadataRevision = hex.EncodeToString(digest[:])
				matches := guidPattern.FindAllSubmatch(meta, -1)
				if len(matches) != 1 {
					item.IdentityIssue = "metadata requires exactly one GUID"
				} else {
					item.GUID = strings.ToLower(string(matches[0][1]))
					guids[item.GUID] = append(guids[item.GUID], len(entries))
				}
			} else if errors.Is(err, os.ErrNotExist) {
				item.IdentityIssue = "missing metadata"
			} else {
				item.IdentityIssue = err.Error()
			}
			entries = append(entries, item)
			return nil
		})
		if err != nil {
			return err
		}
		for guid, indices := range guids {
			if len(indices) > 1 {
				for _, index := range indices {
					entries[index].IdentityIssue = "duplicate GUID " + guid
				}
			}
		}
		sort.Slice(entries, func(i, j int) bool { return entries[i].Path < entries[j].Path })
		encoded, err := json.Marshal(struct {
			Version string
			Entries []InventoryEntry
		}{version, entries})
		if err != nil {
			return err
		}
		digest := sha256.Sum256(append([]byte("unitymicroeditor-inventory-v1\x00"), encoded...))
		result = InventoryPage{Project: s.root, UnityVersion: version, Revision: hex.EncodeToString(digest[:]), Entries: []InventoryEntry{}, Offset: request.Offset}
		if request.ExpectedRevision != "" && request.ExpectedRevision != result.Revision {
			return failure("revision_conflict", "Assets inventory changed between pages", map[string]any{"actualRevision": result.Revision, "expectedRevision": request.ExpectedRevision})
		}
		filtered := []InventoryEntry{}
		for _, item := range entries {
			if request.PathPrefix != "" && item.Path != request.PathPrefix && !strings.HasPrefix(item.Path, request.PathPrefix+"/") {
				continue
			}
			if len(extensions) > 0 && !extensions[item.Extension] || request.Query != "" && !strings.Contains(strings.ToLower(item.Path), strings.ToLower(request.Query)) {
				continue
			}
			filtered = append(filtered, item)
		}
		result.Total = len(filtered)
		if request.Offset > result.Total {
			return failure("invalid_request", "inventory offset exceeds filtered entry count", nil)
		}
		end := request.Offset + request.Limit
		if end > result.Total {
			end = result.Total
		}
		result.Entries = filtered[request.Offset:end]
		if end < result.Total {
			result.NextOffset = &end
		}
		if request.IncludeSourceRevision {
			for i := range result.Entries {
				item := &result.Entries[i]
				if item.IdentityIssue == "" {
					switch item.Extension {
					case ".unity", ".prefab", ".mat", ".asset", ".cs":
						source, err := s.readSource(item.Path)
						if err != nil {
							return err
						}
						item.SourceRevision = source.Revision
					}
				}
			}
		}
		return nil
	})
	return
}
