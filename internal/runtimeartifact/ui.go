package runtimeartifact

import (
	"bytes"
	"crypto/sha256"
	"encoding/binary"
	"encoding/hex"
	"errors"
	"fmt"
	"strconv"
)

func uiDigest(data []byte) string {
	sum := sha256.Sum256(data)
	return hex.EncodeToString(sum[:])
}

// AdaptUI preserves the actual UI/Font importer payload and its type metadata.
// Only the known importer object entries and the Editor target are changed.
// Runtime compatibility is checked through ContentFile.GetObject by the caller.
func AdaptUI(input []byte, profile string) ([]byte, Metadata, error) {
	if profile != "uxml" && profile != "uss" && profile != "ttf" && profile != "otf" {
		return nil, Metadata{}, fmt.Errorf("unsupported UI runtime profile %q", profile)
	}
	return adaptWarmBuiltin(input, profile)
}

// AdaptTexture retains Unity's imported Texture2D, including its chosen native
// pixel format and mip chain. It never resizes or recompresses source pixels.
func AdaptTexture(input []byte, profile string) ([]byte, Metadata, error) {
	if profile != "png" && profile != "jpg" && profile != "jpeg" {
		return nil, Metadata{}, fmt.Errorf("unsupported texture runtime profile %q", profile)
	}
	return adaptWarmBuiltin(input, profile)
}

// An Editor Shader artifact contains parsed HLSL but no platform programs.
// Changing its target cannot supply the missing GPU compilation stage.
func AdaptShader(input []byte) ([]byte, Metadata, error) {
	return nil, Metadata{}, errors.New("native Editor Shader requires platform GPU compilation before runtime adaptation")
}

func adaptWarmBuiltin(input []byte, profile string) ([]byte, Metadata, error) {
	var result Metadata
	texture := profile == "png" || profile == "jpg" || profile == "jpeg"
	if len(input) < 64 || len(input) > maximumBytes || binary.BigEndian.Uint32(input[8:12]) != 23 || input[16] != 0 {
		return nil, result, errors.New("UI runtime projection requires little-endian SerializedFile v23")
	}
	fileSize := binary.BigEndian.Uint64(input[24:32])
	dataOffset := binary.BigEndian.Uint64(input[32:40])
	metadataSize := uint64(binary.BigEndian.Uint32(input[20:24]))
	metadataEnd := 48 + metadataSize
	if fileSize != uint64(len(input)) || metadataEnd > dataOffset || dataOffset > fileSize {
		return nil, result, errors.New("UI SerializedFile header bounds are invalid")
	}
	r := reader{data: input[48:metadataEnd]}
	version, err := r.cstring(64)
	if err != nil || version != "0.0.0" && version != "6000.6.3f1" {
		return nil, result, errors.New("UI artifact must use the pinned Unity version")
	}
	platformOffset := 48 + r.pos
	platform, err := r.u32()
	if err != nil || int32(platform) != -2 {
		return nil, result, errors.New("UI artifact is not an Editor-target native artifact")
	}
	inline, err := r.take(1)
	if err != nil || inline[0] != 1 {
		return nil, result, errors.New("UI artifact requires inline type trees")
	}
	typeCount, err := r.u32()
	if err != nil || typeCount == 0 || typeCount > 256 {
		return nil, result, errors.New("UI artifact type count is invalid")
	}
	classes := make([]int32, typeCount)
	trees := map[int32][]byte{}
	for index := range classes {
		class, err := r.u32()
		if err != nil {
			return nil, result, err
		}
		classes[index] = int32(class)
		flags, err := r.take(3)
		if err != nil || flags[0] != 0 {
			return nil, result, errors.New("stripped UI types are unsupported")
		}
		if int16(binary.LittleEndian.Uint16(flags[1:])) >= 0 || class == 114 || int32(class) < 0 {
			if _, err := r.take(16); err != nil {
				return nil, result, err
			}
		}
		if _, err := r.take(32); err != nil {
			return nil, result, err
		}
		size, err := r.u32()
		if err != nil || size < 16 {
			return nil, result, errors.New("UI type tree is missing")
		}
		blob, err := r.take(int(size))
		if err != nil || binary.LittleEndian.Uint32(blob[:4]) != 0x7474686d || binary.LittleEndian.Uint32(blob[4:8]) != 23 {
			return nil, result, errors.New("UI type tree format is unsupported")
		}
		nodes, chars := uint64(binary.LittleEndian.Uint32(blob[8:12])), uint64(binary.LittleEndian.Uint32(blob[12:16]))
		if nodes == 0 || 16+nodes*32+chars != uint64(len(blob)) {
			return nil, result, errors.New("UI type tree bounds are invalid")
		}
		trees[int32(class)] = blob
		dependencies, err := r.u32()
		if err != nil || dependencies > 1024 {
			return nil, result, errors.New("UI type dependencies are invalid")
		}
		if _, err := r.take(int(dependencies) * 4); err != nil {
			return nil, result, err
		}
	}
	countOffset := 48 + r.pos
	count, err := r.u32()
	if err != nil || count == 0 || count > 1024 {
		return nil, result, errors.New("UI object table is invalid")
	}
	r.pos = (r.pos + 3) &^ 3
	tableStart := 48 + r.pos
	entries := make([]objectEntry, 0, count)
	ids := map[int64]bool{}
	for index := uint32(0); index < count; index++ {
		row, err := r.take(24)
		if err != nil {
			return nil, result, err
		}
		entry := objectEntry{id: int64(binary.LittleEndian.Uint64(row[:8])), offset: binary.LittleEndian.Uint64(row[8:16]), size: uint64(binary.LittleEndian.Uint32(row[16:20])), row: row}
		typeIndex := binary.LittleEndian.Uint32(row[20:24])
		if typeIndex >= typeCount || entry.id == 0 || ids[entry.id] || entry.offset > fileSize-dataOffset || entry.size > fileSize-dataOffset-entry.offset || entry.size < 4 {
			return nil, result, errors.New("UI object identity or bounds are invalid")
		}
		ids[entry.id], entry.classID = true, classes[typeIndex]
		entries = append(entries, entry)
	}
	tableEnd := 48 + r.pos
	scripts, err := r.u32()
	if err != nil || scripts > 256 {
		return nil, result, errors.New("UI script references are invalid")
	}
	if _, err := r.take(int(scripts) * 12); err != nil {
		return nil, result, err
	}
	externals, err := r.u32()
	if err != nil || externals > 256 {
		return nil, result, errors.New("UI external reference table is invalid")
	}
	for index := uint32(0); index < externals; index++ {
		if _, err := r.cstring(256); err != nil {
			return nil, result, err
		}
		guid, err := r.take(16)
		if err != nil {
			return nil, result, err
		}
		kind, err := r.u32()
		if err != nil || kind > 3 {
			return nil, result, errors.New("UI external reference type is invalid")
		}
		path, err := r.cstring(4096)
		if err != nil {
			return nil, result, err
		}
		ordinary := bytes.Clone(guid)
		for i, value := range ordinary {
			ordinary[i] = value<<4 | value>>4
		}
		result.ExternalReferences = append(result.ExternalReferences, ExternalReference{hex.EncodeToString(ordinary), int32(kind), path})
	}
	// Managed UXML serialized-data type trees and their reference table remain
	// intact. They are interpreted by the native Unity serializer, not recreated.
	var retained bytes.Buffer
	for _, entry := range entries {
		data := input[dataOffset+entry.offset : dataOffset+entry.offset+entry.size]
		id := strconv.FormatInt(entry.id, 10)
		if entry.classID == 2089858483 || entry.classID == 1001480554 || (profile == "ttf" || profile == "otf") && entry.classID == 1042 || texture && entry.classID == 1006 {
			result.Removed = append(result.Removed, RemovedObject{id, entry.classID, "editor-only"})
			continue
		}
		if binary.LittleEndian.Uint32(data[:4])&16 != 0 {
			result.Removed = append(result.Removed, RemovedObject{id, entry.classID, "dont-save-in-build"})
			continue
		}
		var typeName string
		if entry.classID == 114 && (profile == "uxml" || profile == "uss") {
			if len(data) < 80 || binary.LittleEndian.Uint32(data[60:64]) != 1 {
				return nil, result, errors.New("UI object has no verified builtin MonoScript reference")
			}
			script := binary.LittleEndian.Uint64(data[64:72])
			if script == 11998 {
				typeName = "UnityEngine.UIElements.StyleSheet"
			} else if script == 11995 && profile == "uxml" {
				typeName = "UnityEngine.UIElements.VisualTreeAsset"
			} else {
				return nil, result, fmt.Errorf("unsupported UI builtin MonoScript %d", script)
			}
			objectReader := reader{data: data, pos: 72}
			if _, err := objectReader.string(); err != nil {
				return nil, result, err
			}
			identifier, err := objectReader.string()
			if err != nil || identifier != "UnityEngine.dll::"+typeName {
				return nil, result, errors.New("UI serialized class identifier differs from builtin script identity")
			}
		} else if texture && entry.classID == 28 && entry.id == 2800000 {
			typeName = "UnityEngine.Texture2D"
			resources, metadata, err := readTextureResources(data, trees[28])
			if err != nil {
				return nil, result, err
			}
			result.Resources = append(result.Resources, resources...)
			result.Texture = metadata
		} else if profile == "ttf" || profile == "otf" {
			switch entry.classID {
			case 128:
				typeName = "UnityEngine.Font"
			case 21:
				typeName = "UnityEngine.Material"
			case 28:
				typeName = "UnityEngine.Texture2D"
			}
		}
		if typeName == "" {
			return nil, result, fmt.Errorf("native class %d is unsupported in UI profile %s", entry.classID, profile)
		}
		result.Objects = append(result.Objects, Object{id, entry.classID, typeName, uiDigest(data)})
		retained.Write(entry.row)
	}
	if len(result.Objects) == 0 {
		return nil, result, errors.New("UI artifact has no usable objects")
	}
	output := bytes.Clone(input)
	removed := 24 * len(result.Removed)
	newEnd := int(metadataEnd) - removed
	newTail := tableStart + retained.Len()
	copy(output[tableStart:newTail], retained.Bytes())
	copy(output[newTail:newEnd], input[tableEnd:metadataEnd])
	clear(output[newEnd:metadataEnd])
	binary.LittleEndian.PutUint32(output[platformOffset:platformOffset+4], 24)
	binary.LittleEndian.PutUint32(output[countOffset:countOffset+4], uint32(len(result.Objects)))
	binary.BigEndian.PutUint32(output[20:24], uint32(metadataSize)-uint32(removed))
	result.Profile, result.SerializedVersion, result.SourceTarget, result.RuntimeTarget = profile, 23, -2, 24
	result.InputSHA256, result.OutputSHA256 = uiDigest(input), uiDigest(output)
	return output, result, nil
}
