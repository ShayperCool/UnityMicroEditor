package runtimeartifact

import (
	"encoding/binary"
	"errors"
	"fmt"
	"io"
)

type BuiltinLightingIdentity struct {
	UnityVersion    string `json:"unityVersion"`
	FileID          int64  `json:"fileId"`
	ClassID         int32  `json:"classId"`
	TypeTreeSHA256  string `json:"typeTreeSha256"`
	PayloadSHA256   string `json:"payloadSha256"`
	LightmapCount   int    `json:"lightmapCount"`
	LightProbeCount int    `json:"lightProbeCount"`
}

// InspectBuiltinLighting reads the exact native resource object. Its accepted
// tree/payload fingerprints are independently checked against binary2text and
// the public Editor observations for the pinned engine. A same-ID resource is
// insufficient: changed layouts or data fail closed.
func InspectBuiltinLighting(input io.ReaderAt, size int64) (BuiltinLightingIdentity, error) {
	object, err := readBuiltinResourceObject(input, size, 20201)
	if err != nil {
		return BuiltinLightingIdentity{}, err
	}
	proof := BuiltinLightingIdentity{UnityVersion: object.version, FileID: 20201, ClassID: object.class,
		TypeTreeSHA256: uiDigest(object.tree), PayloadSHA256: uiDigest(object.payload)}
	if object.class != 1120 {
		return proof, errors.New("native builtin20201 is not the verified LightingDataAsset")
	}
	if proof.TypeTreeSHA256 != builtinLightingTreeSHA || proof.PayloadSHA256 != builtinLightingPayloadSHA {
		return proof, errors.New("native builtin lighting data differs from its independently verified empty-lightmaps/probes profile")
	}
	return proof, nil
}

// Filled from the immutable native engine resource and checked independently
// against the public Editor and standalone binary2text readbacks.
const builtinLightingTreeSHA = "ff2620f5eb07441a22bc1862441790fcbc524bfb5f0c5713cfa135b29c581818"
const builtinLightingPayloadSHA = "c4102142e2fc2b9e09e5e541122478e2883bbeb7d75c6c42ff2e5027bb919f4a"

type builtinResourceObject struct {
	version       string
	class         int32
	tree, payload []byte
}

func readBuiltinResourceObject(input io.ReaderAt, size, wanted int64) (builtinResourceObject, error) {
	var result builtinResourceObject
	var header [48]byte
	if _, err := input.ReadAt(header[:], 0); err != nil {
		return result, err
	}
	if size < 64 || size > 256<<20 || binary.BigEndian.Uint32(header[8:12]) != 23 || header[16] != 0 || int64(binary.BigEndian.Uint64(header[24:32])) != size {
		return result, errors.New("builtin resource requires bounded little-endian native SerializedFile v23")
	}
	metadataSize := int64(binary.BigEndian.Uint32(header[20:24]))
	dataOffset := int64(binary.BigEndian.Uint64(header[32:40]))
	if metadataSize < 16 || metadataSize > 4<<20 || 48+metadataSize > dataOffset || dataOffset > size {
		return result, errors.New("builtin resource metadata bounds are invalid")
	}
	metadata := make([]byte, metadataSize)
	if _, err := input.ReadAt(metadata, 48); err != nil {
		return result, err
	}
	r := reader{data: metadata}
	version, err := r.cstring(64)
	if err != nil || version != "6000.6.3f1" {
		return result, errors.New("builtin resource engine version differs from the pinned profile")
	}
	platform, err := r.u32()
	if err != nil || int32(platform) != -2 {
		return result, errors.New("builtin resource requires native Editor resource metadata")
	}
	inline, err := r.take(1)
	if err != nil || inline[0] != 1 {
		return result, errors.New("builtin resource type trees are absent")
	}
	count, err := r.u32()
	if err != nil || count == 0 || count > 1024 {
		return result, errors.New("builtin resource type count is invalid")
	}
	types := make([]playerShaderType, count)
	for index := range types {
		class, err := r.u32()
		if err != nil {
			return result, err
		}
		flags, err := r.take(3)
		if err != nil || flags[0] != 0 {
			return result, errors.New("stripped native builtin types are unsupported")
		}
		if int16(binary.LittleEndian.Uint16(flags[1:])) >= 0 || class == 114 || int32(class) < 0 {
			if _, err := r.take(16); err != nil {
				return result, err
			}
		}
		if _, err := r.take(32); err != nil {
			return result, err
		}
		length, err := r.u32()
		if err != nil || length < 16 || length > 4<<20 {
			return result, errors.New("builtin type tree bounds are invalid")
		}
		tree, err := r.take(int(length))
		if err != nil || binary.LittleEndian.Uint32(tree[:4]) != 0x7474686d || binary.LittleEndian.Uint32(tree[4:8]) != 23 {
			return result, errors.New("builtin type tree format is unsupported")
		}
		nodes, chars := uint64(binary.LittleEndian.Uint32(tree[8:12])), uint64(binary.LittleEndian.Uint32(tree[12:16]))
		if nodes == 0 || nodes > 65536 || 16+nodes*32+chars != uint64(len(tree)) {
			return result, errors.New("builtin type tree layout is invalid")
		}
		dependencies, err := r.u32()
		if err != nil || dependencies > 1024 {
			return result, errors.New("builtin type dependencies are invalid")
		}
		if _, err := r.take(int(dependencies) * 4); err != nil {
			return result, err
		}
		types[index] = playerShaderType{class: int32(class), tree: tree}
	}
	objects, err := r.u32()
	if err != nil || objects == 0 || objects > 100000 {
		return result, errors.New("builtin object count is invalid")
	}
	r.pos = (r.pos + 3) &^ 3
	seen := map[int64]bool{}
	for index := uint32(0); index < objects; index++ {
		row, err := r.take(24)
		if err != nil {
			return result, err
		}
		id := int64(binary.LittleEndian.Uint64(row[:8]))
		offset, length := int64(binary.LittleEndian.Uint64(row[8:16])), int64(binary.LittleEndian.Uint32(row[16:20]))
		typeIndex := binary.LittleEndian.Uint32(row[20:24])
		if id == 0 || seen[id] || typeIndex >= count || offset < 0 || length <= 0 || offset > size-dataOffset || length > size-dataOffset-offset {
			return result, errors.New("builtin object identities or payload bounds are invalid")
		}
		seen[id] = true
		if id != wanted {
			continue
		}
		if length > 4<<20 {
			return result, errors.New("selected builtin resource object exceeds its bound")
		}
		payload := make([]byte, length)
		if _, err := input.ReadAt(payload, dataOffset+offset); err != nil {
			return result, err
		}
		result = builtinResourceObject{version: version, class: types[typeIndex].class, tree: types[typeIndex].tree, payload: payload}
	}
	if result.payload == nil {
		return result, fmt.Errorf("builtin resource object %d is absent", wanted)
	}
	return result, nil
}
