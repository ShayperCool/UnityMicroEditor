// A strictly fixture-scoped compatibility experiment, not a runtime converter.
package main

import (
	"bytes"
	"crypto/sha256"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"path/filepath"
)

func main() {
	input := flag.String("input", "", "verified FBX or WAV fixture's original native SerializedFile")
	output := flag.String("output", "", "new experimental SerializedFile copy")
	strip := flag.Bool("strip-editor-objects", false, "also remove fixture Editor-only object table entries")
	flag.Parse()
	if err := run(*input, *output, *strip); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func run(input, output string, strip bool) error {
	if !filepath.IsAbs(output) {
		return errors.New("output must be an absolute new path")
	}
	file, err := os.Open(input)
	if err != nil {
		return err
	}
	data, readErr := io.ReadAll(io.LimitReader(file, (1<<20)+1))
	closeErr := file.Close()
	if err := errors.Join(readErr, closeErr); err != nil {
		return err
	}
	if len(data) > 1<<20 {
		return errors.New("the verified fixture artifacts must be smaller than 1 MiB")
	}
	original := sha256.Sum256(data)
	digest := hex.EncodeToString(original[:])
	if digest != "5b5dccc69f6d5ca94a1d36b844f9ecb7f3e5a63969be9653d3273bfcd37fb6cb" &&
		digest != "a298627234ca6be015cd61f021e3930e32789d2bf11a9d09a4b27a23800d5aa6" {
		return errors.New("this experiment only accepts the two exact verified fixture artifacts")
	}
	if len(data) < 64 || binary.BigEndian.Uint32(data[8:12]) != 23 || data[16] != 0 {
		return errors.New("expected the exact little-endian version 23 fixture SerializedFile")
	}
	versionEnd := bytes.IndexByte(data[48:64], 0)
	if versionEnd < 0 {
		return errors.New("fixture metadata version string is missing")
	}
	platformOffset := 48 + versionEnd + 1
	if binary.LittleEndian.Uint32(data[platformOffset:platformOffset+4]) != 0xfffffffe {
		return errors.New("fixture must declare Editor build target -2")
	}
	// Exactly four metadata bytes change. All type trees, local IDs, object data,
	// Editor-only objects and references stay untouched for the runtime experiment.
	binary.LittleEndian.PutUint32(data[platformOffset:platformOffset+4], 24)
	var removed []removedObject
	if strip {
		removed, err = stripEditorObjectEntries(data, platformOffset+4)
		if err != nil {
			return err
		}
	}
	file, err = os.OpenFile(output, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0600)
	if err != nil {
		return err
	}
	_, writeErr := file.Write(data)
	closeErr = file.Close()
	if err := errors.Join(writeErr, closeErr); err != nil {
		return err
	}
	modified := sha256.Sum256(data)
	return json.NewEncoder(os.Stdout).Encode(struct {
		Input          string          `json:"input"`
		Output         string          `json:"output"`
		InputSHA256    string          `json:"inputSHA256"`
		OutputSHA256   string          `json:"outputSHA256"`
		PlatformOffset int             `json:"platformOffset"`
		OldTarget      int             `json:"oldTarget"`
		NewTarget      int             `json:"newTarget"`
		Removed        []removedObject `json:"removed,omitempty"`
	}{input, output, digest, hex.EncodeToString(modified[:]), platformOffset, -2, 24, removed})
}

type removedObject struct {
	LocalID int64 `json:"localID"`
	ClassID int32 `json:"classID"`
}

// The layout is documented by Unity's public UnityDataTools
// UnityBinaryFormat/SerializedFileDetector.cs. Original fixture hashes constrain
// this experiment; this is deliberately not a general Editor-content converter.
func stripEditorObjectEntries(data []byte, start int) ([]removedObject, error) {
	metaSize := int(binary.BigEndian.Uint32(data[20:24]))
	metaEnd := 48 + metaSize
	dataOffset := binary.BigEndian.Uint64(data[32:40])
	if metaEnd > len(data) || uint64(metaEnd) > dataOffset || dataOffset > uint64(len(data)) || data[start] != 1 {
		return nil, errors.New("fixture must have bounded inline type tree metadata")
	}
	pos := start + 1
	read32 := func() (uint32, error) {
		if pos < 48 || pos > metaEnd-4 {
			return 0, errors.New("metadata field exceeds its bounds")
		}
		value := binary.LittleEndian.Uint32(data[pos : pos+4])
		pos += 4
		return value, nil
	}
	typeCount, err := read32()
	if err != nil || typeCount > 32 {
		return nil, errors.New("unexpected fixture type count")
	}
	classes := make([]int32, typeCount)
	for index := range classes {
		classID, err := read32()
		if err != nil || pos > metaEnd-3 {
			return nil, errors.New("truncated fixture type entry")
		}
		classes[index] = int32(classID)
		scriptIndex := int16(binary.LittleEndian.Uint16(data[pos+1 : pos+3]))
		pos += 3
		if classes[index] == -1 || classes[index] == 114 || scriptIndex >= 0 {
			return nil, errors.New("script-backed content is outside this fixture experiment")
		}
		pos += 32 // Type structure hash + content hash.
		blobSize, err := read32()
		if err != nil || blobSize == 0 || uint64(pos)+uint64(blobSize) > uint64(metaEnd) {
			return nil, errors.New("fixture type tree is missing or exceeds metadata")
		}
		pos += int(blobSize)
		depCount, err := read32()
		if err != nil || depCount != 0 {
			return nil, errors.New("fixture contains unsupported serialized-reference dependencies")
		}
	}
	countOffset := pos
	objectCount, err := read32()
	if err != nil || objectCount > 16 {
		return nil, errors.New("unexpected fixture object count")
	}
	pos = (pos + 3) &^ 3
	tableStart := pos
	var retained bytes.Buffer
	var removed []removedObject
	for i := uint32(0); i < objectCount; i++ {
		if pos > metaEnd-24 {
			return nil, errors.New("fixture object table is truncated")
		}
		entry := data[pos : pos+24]
		localID := int64(binary.LittleEndian.Uint64(entry[0:8]))
		start := binary.LittleEndian.Uint64(entry[8:16])
		size := uint64(binary.LittleEndian.Uint32(entry[16:20]))
		typeIndex := binary.LittleEndian.Uint32(entry[20:24])
		if typeIndex >= typeCount || start > uint64(len(data))-dataOffset || size > uint64(len(data))-dataOffset-start {
			return nil, errors.New("fixture object identity/data range is invalid")
		}
		classID := classes[typeIndex]
		if classID == 1041 || classID == 1020 || classID == 1001480554 {
			if localID != 3 && (localID != 100100000 || classID != 1001480554) {
				return nil, errors.New("Editor-only fixture object has an unexpected local ID")
			}
			removed = append(removed, removedObject{localID, classID})
		} else {
			retained.Write(entry)
		}
		pos += 24
	}
	if len(removed) == 0 {
		return nil, errors.New("fixture did not contain expected Editor-only objects")
	}
	newMetaEnd := metaEnd - 24*len(removed)
	newTailStart := tableStart + retained.Len()
	copy(data[tableStart:newTailStart], retained.Bytes())
	copy(data[newTailStart:newMetaEnd], data[pos:metaEnd])
	clear(data[newMetaEnd:metaEnd])
	binary.LittleEndian.PutUint32(data[countOffset:countOffset+4], objectCount-uint32(len(removed)))
	binary.BigEndian.PutUint32(data[20:24], uint32(metaSize-24*len(removed)))
	return removed, nil
}
