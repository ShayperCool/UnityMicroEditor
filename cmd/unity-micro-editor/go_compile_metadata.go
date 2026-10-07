package main

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
)

// This bound applies only to the identity index. MonoScript/configuration
// metadata still uses its smaller limit and full accepted content hashes.
const goMetadataGUIDMaximum = 32 * 1024 * 1024
const goMetadataGUIDBufferSize = 64 * 1024

type goMetadataGUIDIdentity struct {
	GUID   string
	SHA256 string
	Bytes  int64
}

type goMetadataGUIDIndex struct {
	reader *bufio.Reader
}

func newGoMetadataGUIDIndex() *goMetadataGUIDIndex {
	return &goMetadataGUIDIndex{reader: bufio.NewReaderSize(nil, goMetadataGUIDBufferSize)}
}

// read hashes the complete bounded file, but retains only its top-level GUID.
// Model importer settings can have very long lines. ReadSlice discards their
// fragments without constructing an importer document or an unbounded token.
func (index *goMetadataGUIDIndex) read(filename string) (goMetadataGUIDIdentity, error) {
	var result goMetadataGUIDIdentity
	canonical, err := filepath.EvalSymlinks(filename)
	if err != nil || canonical != filename {
		return result, fmt.Errorf("asset metadata has a missing or symlink path: %s", filename)
	}
	before, err := os.Lstat(filename)
	if err != nil || !before.Mode().IsRegular() || before.Size() > goMetadataGUIDMaximum {
		return result, fmt.Errorf("asset metadata is not a bounded regular file: %s", filename)
	}
	file, err := os.Open(filename)
	if err != nil {
		return result, err
	}
	defer file.Close()
	opened, err := file.Stat()
	if err != nil || !opened.Mode().IsRegular() || !os.SameFile(before, opened) || before.Size() != opened.Size() || !before.ModTime().Equal(opened.ModTime()) {
		return result, fmt.Errorf("asset metadata changed before GUID indexing: %s", filename)
	}
	result, err = index.scan(io.LimitReader(file, before.Size()+1))
	if err != nil {
		return goMetadataGUIDIdentity{}, fmt.Errorf("asset metadata GUID indexing %s: %w", filename, err)
	}
	after, statErr := file.Stat()
	current, pathErr := os.Lstat(filename)
	currentCanonical, canonicalErr := filepath.EvalSymlinks(filename)
	if statErr != nil || pathErr != nil || canonicalErr != nil || currentCanonical != filename ||
		!current.Mode().IsRegular() || !os.SameFile(before, after) || !os.SameFile(before, current) ||
		result.Bytes != before.Size() || before.Size() != after.Size() || before.Size() != current.Size() ||
		!before.ModTime().Equal(after.ModTime()) || !before.ModTime().Equal(current.ModTime()) {
		return goMetadataGUIDIdentity{}, fmt.Errorf("asset metadata changed while GUID indexing: %s", filename)
	}
	return result, nil
}

func (index *goMetadataGUIDIndex) scan(input io.Reader) (goMetadataGUIDIdentity, error) {
	var result goMetadataGUIDIdentity
	index.reader.Reset(input)
	hash := sha256.New()
	firstFragment := true
	seenGUID := false
	for {
		fragment, err := index.reader.ReadSlice('\n')
		result.Bytes += int64(len(fragment))
		if result.Bytes > goMetadataGUIDMaximum {
			return goMetadataGUIDIdentity{}, errors.New("metadata exceeds GUID index byte limit")
		}
		hash.Write(fragment)
		if firstFragment && bytes.HasPrefix(fragment, []byte("guid:")) {
			if seenGUID {
				return goMetadataGUIDIdentity{}, errors.New("source .meta has duplicate top-level GUID fields")
			}
			if errors.Is(err, bufio.ErrBufferFull) {
				return goMetadataGUIDIdentity{}, errors.New("top-level GUID field exceeds its line bound")
			}
			seenGUID = true
			guid, parseErr := registryMetaGUID(fragment)
			if parseErr != nil {
				return goMetadataGUIDIdentity{}, parseErr
			}
			result.GUID = guid
		}
		if errors.Is(err, bufio.ErrBufferFull) {
			firstFragment = false
			continue
		}
		firstFragment = true
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return goMetadataGUIDIdentity{}, err
		}
	}
	if !seenGUID {
		return goMetadataGUIDIdentity{}, errors.New("source .meta must contain one nonzero 32-hex GUID")
	}
	result.SHA256 = hex.EncodeToString(hash.Sum(nil))
	return result, nil
}
