// Package runtimeartifact projects Unity 6000.6.3f1 native worker artifacts into
// Linux Mono ContentFiles. Supported native data stays byte-for-byte unchanged.
package runtimeartifact

import (
	"bytes"
	"crypto/sha256"
	"encoding/binary"
	"encoding/hex"
	"errors"
	"fmt"
	"math"
	"strconv"
	"strings"
)

const maximumBytes = 64 << 20

type Object struct {
	FileID   string `json:"fileId"`
	ClassID  int32  `json:"classId"`
	TypeName string `json:"typeName"`
	SHA256   string `json:"dataSHA256"`
}

type RemovedObject struct {
	FileID  string `json:"fileId"`
	ClassID int32  `json:"classId"`
	Reason  string `json:"reason"`
}

type ExternalReference struct {
	GUID string `json:"guid"`
	Type int32  `json:"type"`
	Path string `json:"path"`
}

type Resource struct {
	VirtualPath string `json:"virtualPath"`
	Offset      uint64 `json:"offset"`
	Size        uint64 `json:"size"`
}

type AudioMetadata struct {
	LoadType          uint32  `json:"loadType"`
	CompressionFormat uint32  `json:"compressionFormat"`
	Channels          uint32  `json:"channels"`
	Frequency         uint32  `json:"frequency"`
	Length            float32 `json:"length"`
}

type Metadata struct {
	Profile                     string              `json:"profile"`
	SerializedVersion           uint32              `json:"serializedVersion"`
	SourceTarget                int32               `json:"sourceTarget"`
	RuntimeTarget               int32               `json:"runtimeTarget"`
	InputSHA256                 string              `json:"inputSHA256"`
	OutputSHA256                string              `json:"outputSHA256"`
	Objects                     []Object            `json:"objects"`
	Removed                     []RemovedObject     `json:"removed"`
	ExternalReferences          []ExternalReference `json:"externalReferences"`
	Resources                   []Resource          `json:"resources"`
	Audio                       *AudioMetadata      `json:"audio,omitempty"`
	Texture                     *TextureMetadata    `json:"texture,omitempty"`
	ModelGraph                  []ModelGraphObject  `json:"modelGraph,omitempty"`
	ModelTopologySHA256         string              `json:"modelTopologySHA256,omitempty"`
	ModelGraphUnsupportedReason string              `json:"modelGraphUnsupportedReason,omitempty"`
}

var fbxClasses = map[int32]string{
	1: "UnityEngine.GameObject", 4: "UnityEngine.Transform", 20: "UnityEngine.Camera",
	21: "UnityEngine.Material", 23: "UnityEngine.MeshRenderer", 33: "UnityEngine.MeshFilter",
	43: "UnityEngine.Mesh", 74: "UnityEngine.AnimationClip", 90: "UnityEngine.Avatar",
	95: "UnityEngine.Animator", 108: "UnityEngine.Light", 111: "UnityEngine.Animation",
	137: "UnityEngine.SkinnedMeshRenderer", 205: "UnityEngine.LODGroup",
}

type objectEntry struct {
	id           int64
	classID      int32
	offset, size uint64
	row          []byte
}

// Adapt accepts only little-endian, inline-type-tree SerializedFile v23 native
// FBX or preloaded PCM/Vorbis WAV artifacts. Shader/script/other import profiles fail
// closed. It removes known importer/prefab/preview object table entries, keeps
// local IDs and every retained object's data unchanged, and returns identities
// for public ContentFile.GetObject readback. Native import content hashes must
// be verified by the caller before using this projection.
func Adapt(input []byte, profile string) ([]byte, Metadata, error) {
	var result Metadata
	if profile != "fbx" && profile != "wav" {
		return nil, result, fmt.Errorf("unsupported native runtime profile %q", profile)
	}
	if len(input) < 64 || len(input) > maximumBytes || binary.BigEndian.Uint32(input[8:12]) != 23 || input[16] != 0 {
		return nil, result, errors.New("native runtime projection requires little-endian SerializedFile v23, at most 64 MiB")
	}
	fileSize := binary.BigEndian.Uint64(input[24:32])
	dataOffset := binary.BigEndian.Uint64(input[32:40])
	metadataSize := uint64(binary.BigEndian.Uint32(input[20:24]))
	metadataEnd := 48 + metadataSize
	if fileSize != uint64(len(input)) || metadataEnd > dataOffset || dataOffset > fileSize {
		return nil, result, errors.New("native SerializedFile header bounds are invalid")
	}
	r := reader{data: input[48:metadataEnd]}
	version, err := r.cstring(64)
	if err != nil || version != "0.0.0" && version != "6000.6.3f1" {
		return nil, result, errors.New("artifact must be from the pinned Unity 6000.6.3f1 format")
	}
	platformOffset := 48 + r.pos
	platform, err := r.u32()
	if err != nil || int32(platform) != -2 {
		return nil, result, errors.New("artifact must declare native Editor target -2 before runtime projection")
	}
	inline, err := r.take(1)
	if err != nil || inline[0] != 1 {
		return nil, result, errors.New("native runtime projection requires inline type trees")
	}
	typeCount, err := r.u32()
	if err != nil || typeCount == 0 || typeCount > 256 {
		return nil, result, errors.New("artifact type count is invalid")
	}
	classes := make([]int32, typeCount)
	typeTrees := make(map[int32]string, typeCount)
	for index := range classes {
		classID, err := r.u32()
		if err != nil {
			return nil, result, err
		}
		classes[index] = int32(classID)
		flags, err := r.take(3)
		if err != nil || flags[0] != 0 || int16(binary.LittleEndian.Uint16(flags[1:])) != -1 || classID == 114 || int32(classID) == -1 {
			return nil, result, errors.New("stripped or script-backed native content is outside supported profiles")
		}
		if _, err := r.take(32); err != nil { // Structure and inline type tree content hashes.
			return nil, result, err
		}
		size, err := r.u32()
		if err != nil || size < 16 {
			return nil, result, errors.New("native type tree is missing")
		}
		blob, err := r.take(int(size))
		if err != nil || binary.LittleEndian.Uint32(blob[:4]) != 0x7474686d || binary.LittleEndian.Uint32(blob[4:8]) != 23 {
			return nil, result, errors.New("unsupported inline native type tree format")
		}
		nodes := uint64(binary.LittleEndian.Uint32(blob[8:12]))
		chars := uint64(binary.LittleEndian.Uint32(blob[12:16]))
		if nodes == 0 || 16+nodes*32+chars != uint64(len(blob)) {
			return nil, result, errors.New("native type tree node/string bounds are invalid")
		}
		treeHash := sha256.Sum256(blob)
		typeTrees[int32(classID)] = hex.EncodeToString(treeHash[:])
		dependencies, err := r.u32()
		if err != nil || dependencies != 0 {
			return nil, result, errors.New("native serialized-reference dependencies are unsupported")
		}
	}
	countOffset := 48 + r.pos
	objectCount, err := r.u32()
	if err != nil || objectCount == 0 || objectCount > 100000 {
		return nil, result, errors.New("native object table count is invalid")
	}
	r.pos = (r.pos + 3) &^ 3
	tableStart := 48 + r.pos
	entries := make([]objectEntry, 0, objectCount)
	ids := make(map[int64]bool, objectCount)
	for index := uint32(0); index < objectCount; index++ {
		row, err := r.take(24)
		if err != nil {
			return nil, result, err
		}
		entry := objectEntry{id: int64(binary.LittleEndian.Uint64(row[:8])), offset: binary.LittleEndian.Uint64(row[8:16]), size: uint64(binary.LittleEndian.Uint32(row[16:20])), row: row}
		typeIndex := binary.LittleEndian.Uint32(row[20:24])
		if typeIndex >= typeCount || entry.id == 0 || ids[entry.id] || entry.offset > fileSize-dataOffset || entry.size > fileSize-dataOffset-entry.offset || entry.size < 4 {
			return nil, result, errors.New("native object identity or data bounds are invalid")
		}
		ids[entry.id] = true
		entry.classID = classes[typeIndex]
		entries = append(entries, entry)
	}
	tableEnd := 48 + r.pos
	scripts, err := r.u32()
	if err != nil || scripts != 0 {
		return nil, result, errors.New("script type references are unsupported in native FBX/WAV profiles")
	}
	externals, err := r.u32()
	if err != nil || externals > 1024 {
		return nil, result, errors.New("native external reference table is invalid")
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
			return nil, result, errors.New("native external reference type is invalid")
		}
		path, err := r.cstring(4096)
		if err != nil {
			return nil, result, err
		}
		ordinary := bytes.Clone(guid)
		for index, value := range ordinary {
			ordinary[index] = value<<4 | value>>4
		}
		result.ExternalReferences = append(result.ExternalReferences, ExternalReference{hex.EncodeToString(ordinary), int32(kind), path})
	}
	refTypes, err := r.u32()
	if err != nil || refTypes != 0 {
		return nil, result, errors.New("native serialized reference types are unsupported")
	}
	if _, err := r.cstring(4096); err != nil {
		return nil, result, err
	}
	if !bytes.Equal(r.data[r.pos:], make([]byte, len(r.data)-r.pos)) {
		return nil, result, errors.New("unrecognized native metadata suffix")
	}
	result.Profile, result.SerializedVersion, result.SourceTarget, result.RuntimeTarget = profile, 23, -2, 24
	var retained bytes.Buffer
	for _, entry := range entries {
		object := input[dataOffset+entry.offset : dataOffset+entry.offset+entry.size]
		reason := ""
		if entry.classID == 1001480554 || profile == "fbx" && entry.classID == 1041 || profile == "wav" && entry.classID == 1020 {
			reason = "editor-only"
		} else if binary.LittleEndian.Uint32(object[:4])&16 != 0 {
			reason = "dont-save-in-build"
		}
		localID := strconv.FormatInt(entry.id, 10)
		if reason != "" {
			result.Removed = append(result.Removed, RemovedObject{localID, entry.classID, reason})
			continue
		}
		typeName := fbxClasses[entry.classID]
		if profile == "wav" {
			typeName = ""
			if entry.classID == 83 {
				typeName = "UnityEngine.AudioClip"
				if typeTrees[83] != "d4440ab8f27cef5c3b79c9fc5a65fbe519405155874da1e123d252cd94dd1fe2" {
					return nil, result, errors.New("native AudioClip type tree differs from the pinned verified runtime layout")
				}
				resources, audio, err := readNativeAudioResource(object)
				if err != nil {
					return nil, result, err
				}
				result.Resources = append(result.Resources, resources...)
				result.Audio = audio
			}
		}
		if typeName == "" {
			return nil, result, fmt.Errorf("native class %d is unsupported in %s runtime profile", entry.classID, profile)
		}
		hash := sha256.Sum256(object)
		result.Objects = append(result.Objects, Object{localID, entry.classID, typeName, hex.EncodeToString(hash[:])})
		retained.Write(entry.row)
	}
	if profile == "fbx" {
		result.ModelGraph, result.ModelGraphUnsupportedReason = readModelGraph(input, dataOffset, entries, result.Objects, typeTrees, result.ExternalReferences)
		if result.ModelGraphUnsupportedReason == "" {
			result.ModelTopologySHA256 = HashModelGraph(result.ModelGraph)
		}
	}
	if len(result.Objects) == 0 || profile == "wav" && len(result.Objects) != 1 {
		return nil, result, errors.New("native runtime profile has no usable objects or unexpected audio objects")
	}
	output := bytes.Clone(input)
	removedBytes := 24 * len(result.Removed)
	newMetadataEnd := int(metadataEnd) - removedBytes
	newTailStart := tableStart + retained.Len()
	copy(output[tableStart:newTailStart], retained.Bytes())
	copy(output[newTailStart:newMetadataEnd], input[tableEnd:metadataEnd])
	clear(output[newMetadataEnd:metadataEnd])
	binary.LittleEndian.PutUint32(output[platformOffset:platformOffset+4], 24)
	binary.LittleEndian.PutUint32(output[countOffset:countOffset+4], uint32(len(result.Objects)))
	binary.BigEndian.PutUint32(output[20:24], uint32(metadataSize)-uint32(removedBytes))
	before, after := sha256.Sum256(input), sha256.Sum256(output)
	result.InputSHA256, result.OutputSHA256 = hex.EncodeToString(before[:]), hex.EncodeToString(after[:])
	return output, result, nil
}

type reader struct {
	data []byte
	pos  int
}

func (r *reader) take(size int) ([]byte, error) {
	if size < 0 || r.pos < 0 || r.pos > len(r.data) || size > len(r.data)-r.pos {
		return nil, errors.New("native serialized data exceeds its bounds")
	}
	result := r.data[r.pos : r.pos+size]
	r.pos += size
	return result, nil
}

func (r *reader) u32() (uint32, error) {
	data, err := r.take(4)
	if err != nil {
		return 0, err
	}
	return binary.LittleEndian.Uint32(data), nil
}

func (r *reader) cstring(maximum int) (string, error) {
	if r.pos < 0 || r.pos > len(r.data) {
		return "", errors.New("native string offset exceeds data bounds")
	}
	end := bytes.IndexByte(r.data[r.pos:], 0)
	if end < 0 || end > maximum {
		return "", errors.New("native string is missing or exceeds bounds")
	}
	value := string(r.data[r.pos : r.pos+end])
	r.pos += end + 1
	return value, nil
}

func (r *reader) string() (string, error) {
	size, err := r.u32()
	if err != nil || size > 4096 {
		return "", errors.New("native object string length exceeds bounds")
	}
	data, err := r.take(int(size))
	if err != nil {
		return "", err
	}
	r.pos = (r.pos + 3) &^ 3
	if r.pos > len(r.data) {
		return "", errors.New("native object string alignment exceeds bounds")
	}
	return string(data), nil
}

func readNativeAudioResource(object []byte) ([]Resource, *AudioMetadata, error) {
	r := reader{data: object, pos: 40} // Object flags + three Editor-extension PPtrs.
	if _, err := r.string(); err != nil {
		return nil, nil, err
	}
	fields, err := r.take(24)
	if err != nil || binary.LittleEndian.Uint32(fields[0:4]) > 1 || binary.LittleEndian.Uint32(fields[12:16]) != 16 {
		return nil, nil, errors.New("native WAV runtime profile requires non-streaming PCM16 or preloaded Vorbis")
	}
	loadType := binary.LittleEndian.Uint32(fields[0:4])
	channels := binary.LittleEndian.Uint32(fields[4:8])
	frequency := binary.LittleEndian.Uint32(fields[8:12])
	length := math.Float32frombits(binary.LittleEndian.Uint32(fields[16:20]))
	if channels == 0 || channels > 8 || frequency < 8000 || frequency > 384000 || !(length > 0) || math.IsInf(float64(length), 0) || fields[20] != 0 || fields[21] != 0 {
		return nil, nil, errors.New("native WAV channel/rate/duration or tracker/ambisonic layout is unsupported")
	}
	options, err := r.take(8) // Subsound index + preload/background/legacy3D flags + alignment.
	if err != nil || binary.LittleEndian.Uint32(options[:4]) != 0 || options[4] != 1 || options[5] != 0 {
		return nil, nil, errors.New("native WAV runtime profile requires preloaded, synchronous single-subsound audio")
	}
	path, err := r.string()
	if err != nil || !strings.HasPrefix(path, "VirtualArtifacts/Primary/") || !strings.HasSuffix(path, ".resource") {
		return nil, nil, errors.New("native WAV resource path is outside the primary artifact namespace")
	}
	guid := strings.TrimSuffix(strings.TrimPrefix(path, "VirtualArtifacts/Primary/"), ".resource")
	if decoded, err := hex.DecodeString(guid); err != nil || len(decoded) != 16 || guid != strings.ToLower(guid) {
		return nil, nil, errors.New("native WAV resource GUID is invalid")
	}
	stream, err := r.take(16)
	if err != nil {
		return nil, nil, err
	}
	offset, size := binary.LittleEndian.Uint64(stream[:8]), binary.LittleEndian.Uint64(stream[8:])
	compression, err := r.u32()
	if err != nil || !(loadType == 0 && compression == 0 || loadType == 1 && compression == 1) || offset != 0 || size == 0 || size > maximumBytes {
		return nil, nil, errors.New("native WAV profile requires verified preloaded PCM/Vorbis combination and a bounded zero resource offset")
	}
	return []Resource{{path, offset, size}}, &AudioMetadata{loadType, compression, channels, frequency, length}, nil
}
