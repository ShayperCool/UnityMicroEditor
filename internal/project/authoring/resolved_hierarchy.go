package authoring

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"sort"
	"strings"

	goproject "unitymicroeditor/internal/project"
)

type HierarchyRequest struct {
	Path             string `json:"path"`
	Query            string `json:"query,omitempty"`
	Offset           int    `json:"offset,omitempty"`
	Limit            int    `json:"limit,omitempty"`
	ExpectedRevision string `json:"expectedRevision,omitempty"`
	IncludeFields    *bool  `json:"includeFields,omitempty"`
}

type ObjectProvenance struct {
	SourcePath string                       `json:"sourcePath"`
	SourceGUID string                       `json:"sourceGuid"`
	Identities []goproject.AuthoredIdentity `json:"identities"`
	LocalIDs   []string                     `json:"localIds"`
}

type ResolvedHierarchyPage struct {
	Hierarchy
	Provenance          map[string]ObjectProvenance `json:"provenance"`
	DependencyRevisions map[string]string           `json:"dependencyRevisions"`
	SourceRevision      string                      `json:"sourceRevision"`
	Total               int                         `json:"total"`
	Offset              int                         `json:"offset"`
	NextOffset          *int                        `json:"nextOffset"`
}

func (s *Store) resolvedGraph(path string) (goproject.AuthoredGraph, map[string]string, string, error) {
	if _, err := s.assetPath(path, ".unity", ".prefab"); err != nil {
		return goproject.AuthoredGraph{}, nil, "", err
	}
	graph, err := goproject.ResolveAuthoredGraph(s.root, path)
	if err != nil {
		return graph, nil, "", failure("prefab_resolution_failed", err.Error(), nil)
	}
	revisions := map[string]string{}
	for _, input := range graph.Dependencies {
		if strings.HasSuffix(input.Path, ".meta") {
			continue
		}
		revision, err := s.dependencyRevision(input.Path)
		if err != nil {
			return graph, nil, "", err
		}
		revisions[input.Path] = revision
	}
	encoded, err := json.Marshal(revisions)
	if err != nil {
		return graph, nil, "", err
	}
	digest := sha256.Sum256(append([]byte("unitymicroeditor-resolved-graph-v1\x00"), encoded...))
	return graph, revisions, hex.EncodeToString(digest[:]), nil
}

// ResolvedHierarchy uses the existing runtime source expander, including nested
// prefabs, variants, scalar/reference overrides and structural additions/removals.
// Composite object IDs describe this read-only expanded view. Editable local IDs
// and original prefab identities are reported separately through provenance.
func (s *Store) ResolvedHierarchy(ctx context.Context, request HierarchyRequest) (result ResolvedHierarchyPage, err error) {
	if request.Offset < 0 || request.Limit < 0 || request.Limit > 500 {
		return result, failure("invalid_request", "hierarchy offset must be nonnegative and limit at most 500", nil)
	}
	if request.Limit == 0 {
		request.Limit = 100
	}
	err = s.locked(ctx, func() error {
		source, err := s.readSource(request.Path)
		if err != nil {
			return err
		}
		graph, revisions, graphRevision, err := s.resolvedGraph(request.Path)
		if err != nil {
			return err
		}
		if request.ExpectedRevision != "" && request.ExpectedRevision != graphRevision {
			return failure("revision_conflict", "authored prefab dependency graph changed between pages", map[string]any{"actualRevision": graphRevision})
		}
		virtual := &yamlAsset{}
		provenance := map[string]ObjectProvenance{}
		localsByExpanded := map[string][]string{}
		for local, expanded := range graph.LocalIDs {
			localsByExpanded[expanded] = append(localsByExpanded[expanded], local)
		}
		for _, value := range graph.Objects {
			virtual.docs = append(virtual.docs, &yamlDocument{id: value.FileID, classID: value.ClassID, typeName: value.TypeName, data: value.Data})
			localIDs := localsByExpanded[value.FileID]
			if localIDs == nil {
				localIDs = []string{}
			}
			sort.Strings(localIDs)
			provenance[value.FileID] = ObjectProvenance{SourcePath: value.SourcePath, SourceGUID: value.GUID, Identities: value.Identities, LocalIDs: localIDs}
		}
		view, err := hierarchy(source, virtual, request.Query)
		if err != nil {
			return err
		}
		view.Revision = graphRevision
		result = ResolvedHierarchyPage{Hierarchy: view, Provenance: map[string]ObjectProvenance{}, DependencyRevisions: revisions, SourceRevision: source.Revision, Total: len(view.Objects), Offset: request.Offset}
		if request.Offset > result.Total {
			return failure("invalid_request", "hierarchy offset exceeds filtered object count", nil)
		}
		end := request.Offset + request.Limit
		if end > result.Total {
			end = result.Total
		}
		result.Objects = result.Objects[request.Offset:end]
		for i := range result.Objects {
			object := &result.Objects[i]
			result.Provenance[object.FileID] = provenance[object.FileID]
			for j := range object.Components {
				component := &object.Components[j]
				result.Provenance[component.FileID] = provenance[component.FileID]
				if request.IncludeFields != nil && !*request.IncludeFields {
					component.Fields = nil
				}
			}
		}
		if end < result.Total {
			result.NextOffset = &end
		}
		return nil
	})
	return
}
