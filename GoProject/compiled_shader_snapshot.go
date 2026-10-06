package goproject

import (
	"fmt"
	"path/filepath"
	"sort"
	"strings"
)

type ShaderSourceRequirement struct {
	Path       string `json:"path"`
	SourcePath string `json:"sourcePath"`
	SHA256     string `json:"sha256"`
}

// This snapshot preserves an existing compiled build asset. Source requirements
// guard subsequent edits; they are explicitly not proof that the old build was
// produced from this source revision.
type CompiledShaderSnapshot struct {
	SchemaVersion                  int                       `json:"schemaVersion"`
	CompiledSourceRevisionVerified bool                      `json:"compiledSourceRevisionVerified"`
	ShaderName                     string                    `json:"shaderName"`
	SourcePath                     string                    `json:"sourcePath"`
	ArchiveFileID                  string                    `json:"archiveFileId"`
	BundlePath                     string                    `json:"bundlePath"`
	BundleSHA256                   string                    `json:"bundleSha256"`
	MemberPath                     string                    `json:"memberPath"`
	MemberName                     string                    `json:"memberName"`
	MemberSHA256                   string                    `json:"memberSha256"`
	PayloadSHA256                  string                    `json:"payloadSha256"`
	TypeTreeSHA256                 string                    `json:"typeTreeSha256"`
	SourceRequirements             []ShaderSourceRequirement `json:"sourceRequirements"`
	MissingIncludes                []string                  `json:"missingIncludes"`
}

func (s *Session) validateCompiledShaderSnapshot(r NativeReplacement) error {
	p := r.CompiledShaderSnapshot
	if p == nil {
		return nil
	} // Existing experiment descriptors remain supported.
	if filepath.Ext(r.SourcePath) != ".shader" || p.SourcePath != r.SourcePath || p.SchemaVersion != 1 || p.CompiledSourceRevisionVerified || p.ShaderName == "" || p.MemberName == "" || len(p.MemberName) > 4096 || strings.ContainsRune(p.MemberName, 0) || !validNativeID(p.ArchiveFileID) || !validNativeHex(p.PayloadSHA256, 64) || !validNativeHex(p.TypeTreeSHA256, 64) || len(p.SourceRequirements) == 0 {
		return fmt.Errorf("compiled Shader snapshot identity/provenance is unsupported")
	}
	for _, artifact := range []struct{ path, sha string }{{p.BundlePath, p.BundleSHA256}, {p.MemberPath, p.MemberSHA256}} {
		if !filepath.IsAbs(artifact.path) || !validNativeHex(artifact.sha, 64) {
			return fmt.Errorf("compiled Shader snapshot artifact identity is invalid")
		}
		_, sha, err := s.read(artifact.path, false)
		if err != nil {
			return err
		}
		if sha != artifact.sha {
			return fmt.Errorf("compiled Shader snapshot artifact changed: %s", artifact.path)
		}
	}
	seen := map[string]bool{}
	own := false
	for _, requirement := range p.SourceRequirements {
		if seen[requirement.Path] {
			return fmt.Errorf("compiled Shader snapshot has duplicate source requirements")
		}
		seen[requirement.Path] = true
		resolved, err := s.compiledShaderSource(requirement)
		if err != nil {
			return err
		}
		_, sha, err := s.read(resolved, false)
		if err != nil {
			return err
		}
		if sha != requirement.SHA256 {
			return fmt.Errorf("compiled Shader snapshot source/include changed: %s", requirement.Path)
		}
		if requirement.Path == r.SourcePath {
			if sha != r.SourceSHA256 {
				return fmt.Errorf("compiled Shader snapshot source hash disagrees")
			}
			own = true
		}
	}
	if !own {
		return fmt.Errorf("compiled Shader snapshot has no exact authored source guard")
	}
	if err := ValidateShaderSourceRequirements(s.project, r.SourcePath, p.SourceRequirements, p.MissingIncludes); err != nil {
		return err
	}
	return nil
}

func (s *Session) compiledShaderSource(requirement ShaderSourceRequirement) (string, error) {
	logical := requirement.Path
	if !validNativeHex(requirement.SHA256, 64) || !filepath.IsAbs(requirement.SourcePath) || filepath.ToSlash(filepath.Clean(logical)) != logical || (!strings.HasPrefix(logical, "Assets/") && !strings.HasPrefix(logical, "Packages/")) {
		return "", fmt.Errorf("compiled Shader snapshot source requirement is invalid: %s", logical)
	}
	// Raw authored source remains confined to the project. Package requirements
	// additionally admit this project's selected PackageCache source, rather than
	// silently pinning an old importer project's package copy.
	canonical, err := shaderSourcePath(s.project, logical)
	if err != nil {
		return "", err
	}
	expected, err := filepath.EvalSymlinks(requirement.SourcePath)
	if err != nil {
		return "", err
	}
	expected, err = filepath.Abs(expected)
	if err != nil {
		return "", err
	}
	relative, err := filepath.Rel(s.project, canonical)
	if err != nil || relative == ".." || strings.HasPrefix(relative, ".."+string(filepath.Separator)) || canonical != expected {
		return "", fmt.Errorf("compiled Shader source requirement is not its current project source: %s", logical)
	}
	return canonical, nil
}

func (s *Session) publishCompiledShaderSnapshot(r NativeReplacement, manifest map[string]any) error {
	if r.CompiledShaderSnapshot == nil {
		return nil
	}
	if err := s.validateCompiledShaderSnapshot(r); err != nil {
		return err
	}
	p := r.CompiledShaderSnapshot
	for _, requirement := range p.SourceRequirements {
		physical, err := s.compiledShaderSource(requirement)
		if err != nil {
			return err
		}
		data, sha, err := s.read(physical, false)
		if err != nil {
			return err
		}
		s.dependencies[requirement.Path] = dependency{Path: requirement.Path, SHA256: sha, Bytes: int64(len(data)), SourcePath: physical}
	}
	for _, artifact := range []struct{ path, sha string }{{p.BundlePath, p.BundleSHA256}, {p.MemberPath, p.MemberSHA256}} {
		data, _, err := s.read(artifact.path, false)
		if err != nil {
			return err
		}
		s.dependencies[artifact.path] = dependency{Path: artifact.path, SHA256: artifact.sha, Bytes: int64(len(data)), SourcePath: artifact.path}
	}
	// Preserve the explicit provenance limitation in the reviewable manifest.
	proofs := map[string]any{}
	for _, entry := range array(manifest["compiledShaderSnapshots"]) {
		v := object(entry)
		proofs[str(v["guid"])] = v
	}
	encoded, err := compactJSON(p)
	if err != nil {
		return err
	}
	var value map[string]any
	if err := jsonDecodeString(encoded, &value); err != nil {
		return err
	}
	value["guid"] = r.GUID
	proofs[r.GUID] = value
	keys := make([]string, 0, len(proofs))
	for key := range proofs {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	list := []any{}
	for _, key := range keys {
		list = append(list, proofs[key])
	}
	manifest["compiledShaderSnapshots"] = list
	return nil
}
