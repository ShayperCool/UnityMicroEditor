package goproject

import (
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"io"
	"os"
	"syscall"
)

type identity struct {
	Device, Inode      uint64
	Size, Mtime, Ctime int64
	Mode               uint32
}
type fileRecord struct {
	stamp   identity
	content []byte
	digest  string
}
type yamlRecord struct {
	digest    string
	documents []Document
}

type CacheStats struct {
	FileReads  int   `json:"fileReads"`
	FileHits   int   `json:"fileHits"`
	BytesRead  int64 `json:"bytesRead"`
	YAMLParses int   `json:"yamlParses"`
	YAMLHits   int   `json:"yamlHits"`
}

func fileIdentity(path string) (identity, error) {
	info, err := os.Stat(path)
	if err != nil {
		return identity{}, err
	}
	st, ok := info.Sys().(*syscall.Stat_t)
	if !ok {
		return identity{}, fmt.Errorf("file identity requires Linux stat")
	}
	return identity{uint64(st.Dev), st.Ino, info.Size(), st.Mtim.Sec*1e9 + st.Mtim.Nsec, st.Ctim.Sec*1e9 + st.Ctim.Nsec, uint32(st.Mode)}, nil
}

func (s *Session) read(path string, text bool) ([]byte, string, error) {
	before, err := fileIdentity(path)
	if err != nil {
		return nil, "", err
	}
	if before.Mode&syscall.S_IFMT != syscall.S_IFREG {
		return nil, "", fmt.Errorf("source is not a regular file: %s", path)
	}
	if text && before.Size > maxTextBytes {
		return nil, "", fmt.Errorf("text source exceeds %d bytes: %s", maxTextBytes, path)
	}
	if previous, exists := s.observed[path]; exists && previous != before {
		return nil, "", fmt.Errorf("source changed during preparation, retry: %s", path)
	}
	s.observed[path] = before
	if old, ok := s.files[path]; ok && old.stamp == before && (!text || old.content != nil) {
		s.stats.FileHits++
		return old.content, old.digest, nil
	}
	f, err := os.Open(path)
	if err != nil {
		return nil, "", err
	}
	defer f.Close()
	h := sha256.New()
	var content []byte
	if text {
		content, err = io.ReadAll(io.LimitReader(f, maxTextBytes+1))
		if err != nil {
			return nil, "", err
		}
		if len(content) > maxTextBytes {
			return nil, "", fmt.Errorf("text source grew while reading: %s", path)
		}
		_, err = h.Write(content)
	} else {
		_, err = io.Copy(h, f)
	}
	if err != nil {
		return nil, "", err
	}
	after, err := fileIdentity(path)
	if err != nil {
		return nil, "", err
	}
	if before != after {
		return nil, "", fmt.Errorf("source changed while reading, retry: %s", path)
	}
	opened, err := f.Stat()
	if err != nil {
		return nil, "", err
	}
	current, err := os.Stat(path)
	if err != nil {
		return nil, "", err
	}
	if !os.SameFile(opened, current) {
		return nil, "", fmt.Errorf("source replaced while reading, retry: %s", path)
	}
	digest := hex.EncodeToString(h.Sum(nil))
	s.files[path] = fileRecord{after, content, digest}
	s.stats.FileReads++
	s.stats.BytesRead += before.Size
	return content, digest, nil
}

func (s *Session) parse(path string) ([]Document, error) {
	content, digest, err := s.read(path, true)
	if err != nil {
		return nil, err
	}
	if old, ok := s.documents[path]; ok && old.digest == digest {
		s.stats.YAMLHits++
		return cloneDocuments(old.documents), nil
	}
	docs, err := ParseUnityYAML(content)
	if err != nil {
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	s.documents[path] = yamlRecord{digest, docs}
	s.stats.YAMLParses++
	return cloneDocuments(docs), nil
}

func clone(v any) any {
	switch value := v.(type) {
	case map[string]any:
		result := make(map[string]any, len(value))
		for k, item := range value {
			result[k] = clone(item)
		}
		return result
	case []any:
		result := make([]any, len(value))
		for i, item := range value {
			result[i] = clone(item)
		}
		return result
	default:
		return v
	}
}

func cloneDocuments(docs []Document) []Document {
	result := make([]Document, len(docs))
	copy(result, docs)
	for i := range result {
		result[i].Data = clone(result[i].Data).(map[string]any)
	}
	return result
}
