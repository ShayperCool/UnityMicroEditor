package goproject

import (
	"bufio"
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"syscall"
	"unicode/utf8"

	"gopkg.in/yaml.v3"
)

// PendingNativeImportSources walks authored references before model expansion.
// It returns only reachable cold-profile inputs lacking current native data;
// unrelated project assets and textures accepted by the raw loader are skipped.
func (s *Session) PendingNativeImportSources() ([]string, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	observed, stats, dependencies := s.observed, s.stats, s.dependencies
	s.observed, s.stats, s.dependencies = map[string]identity{}, CacheStats{}, map[string]dependency{}
	defer func() { s.observed, s.stats, s.dependencies = observed, stats, dependencies }()
	if err := s.indexGUIDs(); err != nil {
		return nil, err
	}
	queue := append([]string{str(s.baseline["scenePath"])}, s.extraAssets...)
	seen, heavy := map[string]bool{}, map[string]bool{}
	for len(queue) > 0 {
		logical := queue[0]
		queue = queue[1:]
		if seen[logical] {
			continue
		}
		seen[logical] = true
		if len(seen) > 32768 || len(queue) > 32768 {
			return nil, fmt.Errorf("authored native dependency closure exceeds supported source bound")
		}
		switch strings.ToLower(filepath.Ext(logical)) {
		case ".fbx", ".wav", ".png":
			heavy[logical] = true
			continue
		case ".unity", ".prefab", ".asset", ".mat", ".controller", ".overridecontroller", ".anim":
		default:
			continue
		}
		physical, err := s.source(logical)
		if err != nil {
			return nil, err
		}
		guids, err := s.nativeReferenceGUIDs(physical)
		if err != nil {
			return nil, err
		}
		for _, guid := range guids {
			if source := s.byGUID[guid]; source != "" && !seen[source] {
				queue = append(queue, source)
			}
		}
	}
	var pending []string
	for logical := range heavy {
		needed, err := s.needsNativeImport(logical)
		if err != nil {
			return nil, err
		}
		if needed {
			pending = append(pending, logical)
		}
	}
	sort.Strings(pending)
	return pending, nil
}

const nativeReferenceLineBytes = 64 << 10

// Discovery only needs PPtr targets. Building and caching the whole YAML graph
// here duplicates large mesh buffers and font tables before Prepare even starts.
// Hash the current source while scanning bounded lines; preserve the full schema
// validation in the ordinary asset preparation. Noncanonical YAML uses the full
// parser below, without retaining its documents in the Session cache.
func (s *Session) nativeReferenceGUIDs(path string) ([]string, error) {
	before, err := fileIdentity(path)
	if err != nil {
		return nil, err
	}
	if before.Mode&syscall.S_IFMT != syscall.S_IFREG || before.Size > maxTextBytes {
		return nil, fmt.Errorf("unsupported nonregular or oversized native discovery source: %s", path)
	}
	if previous, exists := s.observed[path]; exists && previous != before {
		return nil, fmt.Errorf("source changed during native discovery, retry: %s", path)
	}
	s.observed[path] = before
	f, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	hash := sha256.New()
	input := &io.LimitedReader{R: io.TeeReader(f, hash), N: maxTextBytes + 1}
	reader := bufio.NewReaderSize(input, nativeReferenceLineBytes)
	guids, fallback, err := scanNativeReferenceGUIDs(reader)
	if err != nil {
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	// A binary source or an early fallback must still get its entire current hash.
	if _, err := io.Copy(io.Discard, reader); err != nil {
		return nil, err
	}
	if input.N <= 0 {
		return nil, fmt.Errorf("native discovery source grew beyond its byte bound: %s", path)
	}
	after, err := fileIdentity(path)
	if err != nil {
		return nil, err
	}
	opened, err := f.Stat()
	if err != nil {
		return nil, err
	}
	current, err := os.Stat(path)
	if err != nil {
		return nil, err
	}
	if before != after || !os.SameFile(opened, current) {
		return nil, fmt.Errorf("source changed while scanning native dependencies, retry: %s", path)
	}
	digest := hex.EncodeToString(hash.Sum(nil))
	// Keep an existing text cache only if it already belongs to this revision.
	content := []byte(nil)
	if old, ok := s.files[path]; ok && old.stamp == after {
		content = old.content
	}
	s.files[path] = fileRecord{after, content, digest}
	s.stats.FileReads++
	s.stats.BytesRead += before.Size
	if !fallback {
		return guids, nil
	}
	content, _, err = s.read(path, true)
	if err != nil {
		return nil, err
	}
	// The fallback retains no additional source text or parsed arrays in Session.
	record := s.files[path]
	record.content = nil
	s.files[path] = record
	documents, err := ParseUnityYAML(content)
	if err != nil {
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	targets := map[string]bool{}
	for _, document := range documents {
		collectNativeGUIDs(document.Data, targets)
	}
	return sortedNativeGUIDs(targets), nil
}

func scanNativeReferenceGUIDs(reader *bufio.Reader) ([]string, bool, error) {
	targets := map[string]bool{}
	first, blockIndent := true, -1
	for {
		line, err := reader.ReadSlice('\n')
		long := errors.Is(err, bufio.ErrBufferFull)
		if err != nil && !long && !errors.Is(err, io.EOF) {
			return nil, false, err
		}
		if errors.Is(err, io.EOF) && len(line) == 0 {
			break
		}
		if first {
			first = false
			line = bytes.TrimPrefix(line, []byte{0xef, 0xbb, 0xbf})
			// Native binary .asset inputs are handled by their normal loader path.
			if !bytes.HasPrefix(line, []byte("%YAML")) && !bytes.HasPrefix(line, []byte("--- !u!")) {
				return nil, false, nil
			}
		}
		trimmed := bytes.TrimSpace(line)
		indent := len(line) - len(bytes.TrimLeft(line, " \t"))
		if blockIndent >= 0 && (len(trimmed) == 0 || indent > blockIndent) {
			if long {
				if err := discardNativeLine(reader); err != nil {
					return nil, false, err
				}
			}
			continue
		}
		blockIndent = -1
		if len(trimmed) == 0 || trimmed[0] == '%' || trimmed[0] == '#' || bytes.HasPrefix(trimmed, []byte("--- !u!")) {
			continue
		}
		if !long && !utf8.Valid(line) {
			return nil, false, fmt.Errorf("unsupported binary Unity text asset")
		}
		key, value, canonical := nativeLineValue(trimmed)
		if !canonical || bytes.Equal(key, []byte("fileID")) {
			return nil, true, nil
		}
		if len(value) == 0 {
			continue
		}
		switch value[0] {
		case '|', '>':
			blockIndent = indent
		case '&', '*', '!':
			return nil, true, nil
		case '\'', '"':
			if long || !nativeQuotedScalarComplete(value) {
				return nil, true, nil
			}
		case '{', '[':
			if long {
				return nil, true, nil
			}
			// Ordinary vectors/color maps never contain object references.
			if bytes.Contains(value, []byte("fileID")) || bytes.ContainsAny(value, "&*!") {
				var node yaml.Node
				if err := yaml.Unmarshal(value, &node); err != nil {
					return nil, true, nil
				}
				if len(node.Content) != 1 {
					return nil, true, nil
				}
				parsed, err := yamlValue(node.Content[0], "", 0)
				if err != nil {
					return nil, true, nil
				}
				collectNativeGUIDs(parsed, targets)
				if len(targets) > 32768 {
					return nil, false, fmt.Errorf("native PPtr target count exceeds supported bound")
				}
			}
		}
		if long {
			// Huge canonical unquoted scalar lines are mesh hex buffers. A brace
			// later in a plain scalar is text, not a YAML flow mapping.
			if err := discardNativeLine(reader); err != nil {
				return nil, false, err
			}
		}
		if errors.Is(err, io.EOF) {
			break
		}
	}
	return sortedNativeGUIDs(targets), false, nil
}

func discardNativeLine(reader *bufio.Reader) error {
	for {
		_, err := reader.ReadSlice('\n')
		if errors.Is(err, bufio.ErrBufferFull) {
			continue
		}
		if errors.Is(err, io.EOF) {
			return nil
		}
		return err
	}
}

func nativeLineValue(line []byte) ([]byte, []byte, bool) {
	sequence := bytes.HasPrefix(line, []byte("- "))
	if sequence {
		line = bytes.TrimSpace(line[2:])
		if len(line) > 0 && (line[0] == '{' || line[0] == '[') {
			return nil, line, true
		}
	}
	quote := byte(0)
	for index := 0; index < len(line); index++ {
		c := line[index]
		if quote != 0 {
			if quote == '"' && c == '\\' {
				index++
				continue
			}
			if c == quote {
				if quote == '\'' && index+1 < len(line) && line[index+1] == '\'' {
					index++
					continue
				}
				quote = 0
			}
			continue
		}
		if c == '\'' || c == '"' {
			quote = c
			continue
		}
		if c == ':' && (index+1 == len(line) || line[index+1] == ' ' || line[index+1] == '\t') {
			key := bytes.TrimSpace(line[:index])
			if len(key) == 0 || key[0] == '\'' || key[0] == '"' || key[0] == '?' {
				return nil, nil, false
			}
			return key, bytes.TrimSpace(line[index+1:]), true
		}
	}
	// Canonical Unity arrays also contain scalar items, e.g. font glyph IDs.
	if sequence || bytes.Equal(line, []byte("-")) {
		return nil, line, true
	}
	return nil, nil, false
}

func nativeQuotedScalarComplete(value []byte) bool {
	quote := value[0]
	for index := 1; index < len(value); index++ {
		if quote == '"' && value[index] == '\\' {
			index++
			continue
		}
		if value[index] != quote {
			continue
		}
		if quote == '\'' && index+1 < len(value) && value[index+1] == '\'' {
			index++
			continue
		}
		rest := bytes.TrimSpace(value[index+1:])
		return len(rest) == 0 || rest[0] == '#'
	}
	return false
}

func collectNativeGUIDs(value any, targets map[string]bool) {
	visitReferences(value, "", func(_ string, reference map[string]any) {
		guid := str(reference["guid"])
		if fileID(reference) != "0" && validNativeGUID(guid) {
			targets[guid] = true
		}
	})
}

func sortedNativeGUIDs(targets map[string]bool) []string {
	result := make([]string, 0, len(targets))
	for guid := range targets {
		result = append(result, guid)
	}
	sort.Strings(result)
	return result
}

func (s *Session) needsNativeImport(logical string) (bool, error) {
	physical, err := s.source(logical)
	if err != nil {
		return false, err
	}
	_, sourceSHA, err := s.read(physical, false)
	if err != nil {
		return false, err
	}
	metaPhysical, err := s.source(logical + ".meta")
	if err != nil {
		return false, err
	}
	_, metaSHA, err := s.read(metaPhysical, true)
	if err != nil {
		return false, err
	}
	guid := s.byPath[logical]
	if native, exists := s.nativeOverlays[guid]; exists {
		return native.SourceSHA256 != sourceSHA || native.MetaSHA256 != metaSHA, nil
	}
	current := s.critical[logical] == sourceSHA && s.critical[logical+".meta"] == metaSHA
	if strings.EqualFold(filepath.Ext(logical), ".fbx") {
		return !current || len(s.models[guid]) == 0, nil
	}
	if current {
		for _, asset := range s.assets {
			if str(asset["guid"]) == guid && (number(asset["classId"]) == 83 || number(asset["classId"]) == 28) {
				return false, nil
			}
		}
		for _, asset := range s.catalog.Assets {
			if str(asset["guid"]) == guid && (number(asset["classId"]) == 83 || number(asset["classId"]) == 28) {
				return false, nil
			}
		}
	}
	if strings.EqualFold(filepath.Ext(logical), ".png") {
		err := s.refreshAuthoredTexture(map[string]any{"path": logical})
		if err == nil {
			return false, nil
		}
		var needed *NeedsImportError
		if !errors.As(err, &needed) {
			return false, err
		}
	}
	return true, nil
}
