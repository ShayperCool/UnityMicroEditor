package main

import (
	"bytes"
	"encoding/binary"
	"os"
	"path/filepath"
	"testing"
)

func udsStorageRecord(hash []byte, kind uint32, file, offset, size uint64) LMDBRecord {
	value := make([]byte, 32)
	binary.LittleEndian.PutUint32(value[:4], kind)
	binary.LittleEndian.PutUint64(value[8:16], file)
	binary.LittleEndian.PutUint64(value[16:24], offset)
	binary.LittleEndian.PutUint64(value[24:32], size)
	return LMDBRecord{Key: hash, Value: value}
}

func TestUDSReadsExactInlineAndConcatContent(t *testing.T) {
	directory := t.TempDir()
	inline, concat := []byte("inline exact bytes\x00"), []byte("file exact bytes\x01")
	inlineKey, concatKey := nativeSourceHash(inline), nativeSourceHash(concat)
	inlineHash, concatHash := inlineKey[:], concatKey[:]
	data := append([]byte("prefix"), concat...)
	data = append(data, []byte("suffix")...)
	if err := os.WriteFile(filepath.Join(directory, "UDSData_7.bin"), data, 0600); err != nil {
		t.Fatal(err)
	}
	store, err := artifactStoreFromRecords(directory, []LMDBRecord{
		udsStorageRecord(inlineHash, 3, 0, 0, uint64(len(inline))),
		udsStorageRecord(concatHash, 2, 7, 6, uint64(len(concat))),
	}, []LMDBRecord{{Key: inlineHash, Value: inline}})
	if err != nil {
		t.Fatal(err)
	}
	for index, hash := range [][]byte{inlineHash, concatHash} {
		content, proof, err := store.Read(hash)
		expected := [][]byte{inline, concat}[index]
		if err != nil || !bytes.Equal(content, expected) || proof.SHA256 != bytesSHA(expected) || !proof.ContentKeyVerified {
			t.Fatalf("content mismatch: %+v %v", proof, err)
		}
	}
	output := filepath.Join(t.TempDir(), "asset.bin")
	if _, err = store.Extract(concatHash, output); err != nil {
		t.Fatal(err)
	}
	if _, err = store.Extract(concatHash, output); err == nil {
		t.Fatal("existing extraction overwritten")
	}
	if _, err = store.Extract(concatHash, filepath.Join(directory, "must-not-write")); err == nil {
		t.Fatal("extraction wrote into input DataStore")
	}
	store.content[string(inlineHash)][0] ^= 1
	if _, _, err = store.Read(inlineHash); err == nil {
		t.Fatal("corrupted inline content passed native hash verification")
	}
	data[6] ^= 1
	if err = os.WriteFile(filepath.Join(directory, "UDSData_7.bin"), data, 0600); err != nil {
		t.Fatal(err)
	}
	if _, _, err = store.Read(concatHash); err == nil {
		t.Fatal("corrupted concatenated content passed native hash verification")
	}
}

func TestUDSRejectsTruncatedAndUnsupportedStorage(t *testing.T) {
	directory := t.TempDir()
	hash := bytes.Repeat([]byte{3}, 16)
	if err := os.WriteFile(filepath.Join(directory, "UDSData_0.bin"), []byte("short"), 0600); err != nil {
		t.Fatal(err)
	}
	for _, record := range []LMDBRecord{
		udsStorageRecord(hash, 2, 0, 4, 2),
		udsStorageRecord(hash, 2, 0, ^uint64(0), 2),
		udsStorageRecord(hash, 3, 0, 0, 2),
		udsStorageRecord(hash, 1, 0, 0, 2),
		udsStorageRecord(hash, 2, 0, 0, maxArtifactBytes+1),
	} {
		store, err := artifactStoreFromRecords(directory, []LMDBRecord{record}, nil)
		if err != nil {
			t.Fatal(err)
		}
		if _, _, err = store.Read(hash); err == nil {
			t.Fatalf("accepted bad record %+v", record)
		}
	}
	if _, err := artifactStoreFromRecords(directory, []LMDBRecord{{Key: hash, Value: []byte{1}}}, nil); err == nil {
		t.Fatal("accepted truncated storage record")
	}
}
