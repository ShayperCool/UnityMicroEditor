package main

import (
	"bytes"
	"encoding/binary"
	"encoding/hex"
	"errors"
	"fmt"
	"os"
	"path/filepath"
)

const maxArtifactBytes = 64 * 1024 * 1024

type UDSArtifactStore struct {
	directory string
	storage   map[string][]byte
	content   map[string][]byte
}

type ArtifactExtraction struct {
	RawHashKey         string `json:"rawHashKey"`
	StorageType        uint32 `json:"storageType"`
	SourcePath         string `json:"sourcePath,omitempty"`
	Offset             uint64 `json:"offset"`
	Bytes              uint64 `json:"bytes"`
	SHA256             string `json:"sha256"`
	ContentKeyVerified bool   `json:"contentKeyVerified"`
	Output             string `json:"output,omitempty"`
}

// OpenUDSArtifactStore reads a stopped, caller-owned DataStore snapshot without
// invoking Unity or taking native database locks. It decodes only the observed
// Linux x64 6000.6.3f1 StorageInfoTable/ContentDataTable layout.
func OpenUDSArtifactStore(directory string) (*UDSArtifactStore, error) {
	directory, err := filepath.Abs(directory)
	if err != nil {
		return nil, err
	}
	storage, err := ReadLMDBTable(filepath.Join(directory, "uds.db"), "StorageInfoTable")
	if err != nil {
		return nil, fmt.Errorf("UDS storage table: %w", err)
	}
	content, err := ReadLMDBTable(filepath.Join(directory, "uds.db"), "ContentDataTable")
	if err != nil {
		return nil, fmt.Errorf("UDS content table: %w", err)
	}
	return artifactStoreFromRecords(directory, storage, content)
}

func artifactStoreFromRecords(directory string, storage, content []LMDBRecord) (*UDSArtifactStore, error) {
	store := &UDSArtifactStore{directory: directory, storage: make(map[string][]byte), content: make(map[string][]byte)}
	for _, record := range storage {
		if len(record.Key) != 16 || len(record.Value) != 32 {
			return nil, errors.New("UDS storage record has unexpected size")
		}
		key := string(record.Key)
		if _, duplicate := store.storage[key]; duplicate {
			return nil, errors.New("duplicate UDS storage hash")
		}
		store.storage[key] = bytes.Clone(record.Value)
	}
	for _, record := range content {
		if len(record.Key) != 16 || len(record.Value) > maxArtifactBytes {
			return nil, errors.New("UDS content record is outside bounds")
		}
		key := string(record.Key)
		if _, duplicate := store.content[key]; duplicate {
			return nil, errors.New("duplicate UDS content hash")
		}
		store.content[key] = bytes.Clone(record.Value)
	}
	return store, nil
}

// Read uses the exact 16 raw bytes from a worker artifact hash, not a guessed
// string/endianness conversion. The native UDS memory hasher uses XXH3-128,
// stored as low/high little-endian words; verify it before returning content.
func (store *UDSArtifactStore) Read(hash []byte) ([]byte, ArtifactExtraction, error) {
	proof := ArtifactExtraction{RawHashKey: hex.EncodeToString(hash)}
	if len(hash) != 16 {
		return nil, proof, errors.New("UDS artifact hash must be 16 bytes")
	}
	info, found := store.storage[string(hash)]
	if !found {
		return nil, proof, errors.New("artifact hash has no UDS storage entry")
	}
	proof.StorageType = binary.LittleEndian.Uint32(info[:4])
	proof.Offset = binary.LittleEndian.Uint64(info[16:24])
	proof.Bytes = binary.LittleEndian.Uint64(info[24:32])
	if proof.Bytes == 0 || proof.Bytes > maxArtifactBytes {
		return nil, proof, errors.New("UDS artifact size is outside probe bounds")
	}
	var content []byte
	switch proof.StorageType {
	case 2:
		fileID := binary.LittleEndian.Uint64(info[8:16])
		proof.SourcePath = filepath.Join(store.directory, fmt.Sprintf("UDSData_%d.bin", fileID))
		file, err := os.Open(proof.SourcePath)
		if err != nil {
			return nil, proof, err
		}
		defer file.Close()
		stat, err := file.Stat()
		if err != nil {
			return nil, proof, err
		}
		if !stat.Mode().IsRegular() || proof.Offset > uint64(stat.Size()) || proof.Bytes > uint64(stat.Size())-proof.Offset {
			return nil, proof, errors.New("UDS artifact slice exceeds its data file")
		}
		content = make([]byte, int(proof.Bytes))
		if _, err = file.ReadAt(content, int64(proof.Offset)); err != nil {
			return nil, proof, err
		}
	case 3:
		value, found := store.content[string(hash)]
		if !found || proof.Offset != 0 || uint64(len(value)) != proof.Bytes {
			return nil, proof, errors.New("inline UDS artifact does not match storage size")
		}
		content = bytes.Clone(value)
	default:
		return nil, proof, fmt.Errorf("UDS storage type %d is not yet supported", proof.StorageType)
	}
	computed := nativeSourceHash(content)
	if !bytes.Equal(computed[:], hash) {
		return nil, proof, errors.New("UDS content bytes do not match their native XXH3-128 key")
	}
	proof.ContentKeyVerified = true
	proof.SHA256 = bytesSHA(content)
	return content, proof, nil
}

func (store *UDSArtifactStore) Extract(hash []byte, output string) (ArtifactExtraction, error) {
	content, proof, err := store.Read(hash)
	if err != nil {
		return proof, err
	}
	output, err = filepath.Abs(output)
	if err != nil {
		return proof, err
	}
	canonical, err := canonicalDestination(output)
	if err != nil {
		return proof, err
	}
	directory, err := filepath.EvalSymlinks(store.directory)
	if err != nil {
		return proof, err
	}
	if withinDirectory(canonical, directory) {
		return proof, errors.New("artifact extraction must not write into its input DataStore")
	}
	file, err := os.OpenFile(output, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0600)
	if err != nil {
		return proof, err
	}
	defer file.Close()
	if _, err = file.Write(content); err != nil {
		return proof, err
	}
	if err = file.Close(); err != nil {
		return proof, err
	}
	proof.Output = output
	return proof, nil
}

type SerializedArtifactHeader struct {
	Version       uint32 `json:"version"`
	MetadataBytes uint32 `json:"metadataBytes"`
	FileBytes     uint64 `json:"fileBytes"`
	DataOffset    uint64 `json:"dataOffset"`
	BigEndian     bool   `json:"bigEndian"`
}

type ArtifactTextInspection struct {
	Input      string                   `json:"input"`
	Output     string                   `json:"output"`
	Header     SerializedArtifactHeader `json:"header"`
	DumpSHA256 string                   `json:"dumpSha256"`
	DumpBytes  int                      `json:"dumpBytes"`
}

