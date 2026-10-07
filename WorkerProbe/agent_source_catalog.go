package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"io"
	"os"
	"path/filepath"
	"syscall"
	"unicode/utf8"

	goproject "unitymicroeditor/project"
)

const agentSourceCatalogMaximum = 32 << 20

type agentSourceCatalogReceipt struct {
	SchemaVersion  int    `json:"schemaVersion"`
	Project        string `json:"project"`
	SourcePath     string `json:"sourcePath"`
	SourceSHA256   string `json:"sourceSHA256"`
	SnapshotPath   string `json:"snapshotPath"`
	SnapshotSHA256 string `json:"snapshotSHA256"`
}

// Explicit external catalogs are read-only inputs. Preserve their JSON bytes,
// including signed 64-bit identities, while freezing a target-owned snapshot.
// Catalog semantics and all referenced content still require downstream checks.
func freezeAgentSourceCatalog(ctx context.Context, project, directory, inputPath string) (string, error) {
	if err := ctx.Err(); err != nil {
		return "", err
	}
	if inputPath == "" {
		return "", nil
	}
	if !filepath.IsAbs(inputPath) || agentCanonical(inputPath) != nil {
		return "", errors.New("source catalog input must be a canonical absolute regular file")
	}
	info, err := os.Lstat(inputPath)
	if err != nil {
		return "", err
	}
	if !info.Mode().IsRegular() || info.Size() > agentSourceCatalogMaximum {
		return "", errors.New("source catalog input must be a bounded regular file")
	}
	if goproject.IsOwnedCachePath(project, inputPath) {
		return inputPath, nil
	}
	snapshotPath := filepath.Join(directory, "native-input-catalog.json")
	receiptPath := filepath.Join(directory, "native-input-receipt.json")
	for _, path := range []string{directory, snapshotPath, receiptPath} {
		if !filepath.IsAbs(path) || !goproject.IsOwnedCachePath(project, path) || agentCanonical(path) != nil {
			return "", errors.New("source catalog snapshot output must be canonical and target-owned")
		}
	}
	content, err := readAgentSourceCatalog(ctx, inputPath)
	if err != nil {
		return "", err
	}
	var raw json.RawMessage
	if !utf8.Valid(content) || json.Unmarshal(content, &raw) != nil || len(bytes.TrimSpace(raw)) == 0 || bytes.TrimSpace(raw)[0] != '{' {
		return "", errors.New("source catalog input must contain one valid UTF-8 JSON object")
	}
	sourceHash := bytesSHA(content)
	receipt := agentSourceCatalogReceipt{SchemaVersion: 1, Project: project, SourcePath: inputPath, SourceSHA256: sourceHash,
		SnapshotPath: snapshotPath, SnapshotSHA256: sourceHash}
	var previous agentSourceCatalogReceipt
	if err := agentReadJSON(receiptPath, 1<<20, &previous); err == nil {
		if previous != receipt {
			return "", agentFailure("request_id_conflict", "external source catalog identity or content changed after its frozen snapshot")
		}
	} else if !errors.Is(err, os.ErrNotExist) {
		return "", err
	}
	// An interrupted first publication may have a snapshot without a receipt.
	// It must still match these exact source bytes before retrying publication.
	if previous, err := readAgentSourceCatalog(ctx, snapshotPath); err == nil {
		if !bytes.Equal(previous, content) {
			return "", agentFailure("request_id_conflict", "external source catalog content changed after its frozen snapshot")
		}
	} else if !errors.Is(err, os.ErrNotExist) {
		return "", err
	}
	if err := agentImmutableFile(snapshotPath, content); err != nil {
		return "", err
	}
	current, err := readAgentSourceCatalog(ctx, inputPath)
	if err != nil {
		return "", err
	}
	if bytesSHA(current) != sourceHash {
		return "", agentFailure("revision_conflict", "external source catalog changed while its snapshot was being published")
	}
	if err := agentImmutableJSON(receiptPath, receipt); err != nil {
		return "", err
	}
	if err := ctx.Err(); err != nil {
		return "", err
	}
	return snapshotPath, nil
}

// Bound reads through a directory descriptor and reject replacement, symlinks,
// special files, growth, or changed bytes during either source sample.
func readAgentSourceCatalog(ctx context.Context, path string) ([]byte, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	if !filepath.IsAbs(path) || agentCanonical(path) != nil {
		return nil, errors.New("source catalog path is not canonical")
	}
	before, err := os.Lstat(path)
	if err != nil {
		return nil, err
	}
	if !before.Mode().IsRegular() || before.Size() > agentSourceCatalogMaximum {
		return nil, errors.New("source catalog is not a bounded regular file")
	}
	root, err := os.OpenRoot(filepath.Dir(path))
	if err != nil {
		return nil, err
	}
	defer root.Close()
	file, err := root.OpenFile(filepath.Base(path), os.O_RDONLY|syscall.O_NOFOLLOW|syscall.O_NONBLOCK, 0)
	if err != nil {
		return nil, err
	}
	defer file.Close()
	opened, err := file.Stat()
	if err != nil || !opened.Mode().IsRegular() || !os.SameFile(before, opened) || before.Size() != opened.Size() || !before.ModTime().Equal(opened.ModTime()) {
		return nil, errors.New("source catalog identity changed while opening it")
	}
	content, err := io.ReadAll(io.LimitReader(file, agentSourceCatalogMaximum+1))
	if err != nil {
		return nil, err
	}
	after, statErr := file.Stat()
	current, pathErr := root.Lstat(filepath.Base(path))
	if statErr != nil || pathErr != nil || agentCanonical(path) != nil || !after.Mode().IsRegular() || !current.Mode().IsRegular() ||
		!os.SameFile(before, after) || !os.SameFile(before, current) || int64(len(content)) != before.Size() ||
		before.Size() != after.Size() || before.Size() != current.Size() || !before.ModTime().Equal(after.ModTime()) || !before.ModTime().Equal(current.ModTime()) {
		return nil, errors.New("source catalog changed during its bounded read")
	}
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	return content, nil
}
