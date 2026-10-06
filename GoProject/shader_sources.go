package goproject

import (
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"os"
	"path"
	"path/filepath"
	"reflect"
	"regexp"
	"sort"
	"strings"
)

var shaderIncludeDirective = regexp.MustCompile(`(?m)^\s*#\s*(?:include|include_with_pragmas)\b([^\r\n]*)`)
var shaderIncludeLiteral = regexp.MustCompile(`^\s*(?:"([^"\r\n]+)"|<([^>\r\n]+)>)\s*$`)

// CollectShaderSourceRequirements snapshots all literal includes, including
// conditional branches. Missing optional includes are recorded as absent and
// must remain absent. This is a guard on later edits, not build provenance.
func CollectShaderSourceRequirements(project, source string) ([]ShaderSourceRequirement, []string, error) {
	canonical, err := filepath.EvalSymlinks(project)
	if err != nil {
		return nil, nil, err
	}
	canonical, err = filepath.Abs(canonical)
	if err != nil {
		return nil, nil, err
	}
	if !validShaderSource(source) || !strings.HasPrefix(source, "Assets/") || path.Ext(source) != ".shader" {
		return nil, nil, fmt.Errorf("Shader source path is invalid: %s", source)
	}
	seen, missing := map[string]bool{}, map[string]bool{}
	files := map[string]ShaderSourceRequirement{}
	var visit func(string) error
	visit = func(logical string) error {
		if seen[logical] {
			return nil
		}
		if len(seen) >= 2048 {
			return errors.New("Shader include closure exceeds the supported size")
		}
		seen[logical] = true
		physical, err := shaderSourcePath(canonical, logical)
		if errors.Is(err, os.ErrNotExist) && logical != source {
			missing[logical] = true
			return nil
		}
		if err != nil {
			return fmt.Errorf("resolve Shader source/include %s: %w", logical, err)
		}
		data, err := readShaderSource(physical)
		if err != nil {
			return err
		}
		sum := sha256.Sum256(data)
		files[logical] = ShaderSourceRequirement{Path: logical, SourcePath: physical, SHA256: hex.EncodeToString(sum[:])}
		cleaned, err := shaderWithoutComments(string(data))
		if err != nil {
			return fmt.Errorf("read Shader includes %s: %w", logical, err)
		}
		for _, directive := range shaderIncludeDirective.FindAllStringSubmatch(cleaned, -1) {
			literal := shaderIncludeLiteral.FindStringSubmatch(directive[1])
			if literal == nil {
				return fmt.Errorf("Shader snapshot requires a literal include in %s: %s", logical, strings.TrimSpace(directive[1]))
			}
			name := literal[1]
			if name == "" {
				name = literal[2]
			}
			if strings.Contains(name, "\\") || path.IsAbs(name) {
				return fmt.Errorf("Shader include path is unsupported in %s: %s", logical, name)
			}
			included := path.Clean(name)
			if !strings.HasPrefix(included, "Assets/") && !strings.HasPrefix(included, "Packages/") {
				included = path.Clean(path.Join(path.Dir(logical), name))
			}
			if !validShaderSource(included) {
				return fmt.Errorf("Shader include escapes source roots in %s: %s", logical, name)
			}
			if err := visit(included); err != nil {
				return err
			}
		}
		return nil
	}
	if err := visit(source); err != nil {
		return nil, nil, err
	}
	result := make([]ShaderSourceRequirement, 0, len(files))
	for _, file := range files {
		result = append(result, file)
	}
	sort.Slice(result, func(i, j int) bool { return result[i].Path < result[j].Path })
	absent := make([]string, 0, len(missing))
	for logical := range missing {
		absent = append(absent, logical)
	}
	sort.Strings(absent)
	return result, absent, nil
}

func ValidateShaderSourceRequirements(project, source string, expected []ShaderSourceRequirement, missing []string) error {
	actual, absent, err := CollectShaderSourceRequirements(project, source)
	if err != nil {
		return err
	}
	owned := append([]ShaderSourceRequirement(nil), expected...)
	sort.Slice(owned, func(i, j int) bool { return owned[i].Path < owned[j].Path })
	if !reflect.DeepEqual(actual, owned) {
		return errors.New("compiled Shader snapshot source/include closure changed or was incomplete")
	}
	wantedAbsent := append([]string{}, missing...)
	sort.Strings(wantedAbsent)
	if !reflect.DeepEqual(absent, wantedAbsent) {
		return errors.New("compiled Shader snapshot optional include availability changed")
	}
	return nil
}

func validShaderSource(logical string) bool {
	return path.Clean(logical) == logical && !strings.Contains(logical, "\\") && (strings.HasPrefix(logical, "Assets/") || strings.HasPrefix(logical, "Packages/"))
}

func shaderSourcePath(project, logical string) (string, error) {
	if !validShaderSource(logical) {
		return "", fmt.Errorf("Shader source/include path is invalid: %s", logical)
	}
	physical := filepath.Join(project, filepath.FromSlash(logical))
	canonical, err := filepath.EvalSymlinks(physical)
	if errors.Is(err, os.ErrNotExist) && strings.HasPrefix(logical, "Packages/") {
		parts := strings.Split(logical, "/")
		if len(parts) < 3 || strings.ContainsAny(parts[1], "*?[") {
			return "", fmt.Errorf("Shader package path is incomplete or invalid: %s", logical)
		}
		candidates, globErr := filepath.Glob(filepath.Join(project, "Library/PackageCache", parts[1]+"@*"))
		if globErr != nil || len(candidates) > 1 {
			return "", fmt.Errorf("Shader package source is ambiguous: %s", logical)
		}
		if len(candidates) == 0 {
			return "", os.ErrNotExist
		}
		canonical, err = filepath.EvalSymlinks(filepath.Join(candidates[0], filepath.FromSlash(strings.Join(parts[2:], "/"))))
	}
	if err != nil {
		return "", err
	}
	canonical, err = filepath.Abs(canonical)
	if err != nil {
		return "", err
	}
	relative, err := filepath.Rel(project, canonical)
	if err != nil || relative == ".." || strings.HasPrefix(relative, ".."+string(filepath.Separator)) {
		return "", fmt.Errorf("Shader include leaves current project: %s", logical)
	}
	return canonical, nil
}

func readShaderSource(physical string) ([]byte, error) {
	f, err := os.Open(physical)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	data, err := io.ReadAll(io.LimitReader(f, (4<<20)+1))
	if err != nil {
		return nil, err
	}
	if len(data) > 4<<20 {
		return nil, fmt.Errorf("Shader source/include exceeds supported size: %s", physical)
	}
	return data, nil
}

// Preserve line boundaries and quoted paths while removing comments. A regex
// comment stripper would mistake // inside a quoted string for a comment.
func shaderWithoutComments(input string) (string, error) {
	out := []byte(input)
	quoted, block, line := false, false, false
	for i := 0; i < len(out); i++ {
		c := out[i]
		if line {
			if c == '\n' || c == '\r' {
				line = false
			} else {
				out[i] = ' '
			}
			continue
		}
		if block {
			if c == '*' && i+1 < len(out) && out[i+1] == '/' {
				out[i], out[i+1] = ' ', ' '
				block = false
				i++
			} else if c != '\n' && c != '\r' {
				out[i] = ' '
			}
			continue
		}
		if quoted {
			if c == '\\' && i+1 < len(out) {
				i++
			} else if c == '"' {
				quoted = false
			}
			continue
		}
		if c == '"' {
			quoted = true
		} else if c == '/' && i+1 < len(out) && (out[i+1] == '*' || out[i+1] == '/') {
			block, line = out[i+1] == '*', out[i+1] == '/'
			out[i], out[i+1] = ' ', ' '
			i++
		}
	}
	if block {
		return "", errors.New("unterminated Shader source block comment")
	}
	return string(out), nil
}
