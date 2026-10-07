package runtimeartifact

import (
	"encoding/binary"
	"errors"
	"fmt"
	"io"
)

type BuiltinShaderIdentity struct {
	UnityVersion  string `json:"unityVersion"`
	FileID        int64  `json:"fileId"`
	Name          string `json:"name"`
	PayloadSHA256 string `json:"payloadSha256"`
}

// InspectBuiltinShader reads only the native resource metadata and one shader
// object. The Editor resource is identity evidence and is never loaded by Unity.
func InspectBuiltinShader(input io.ReaderAt, size int64, id int64) (BuiltinShaderIdentity, error) {
	var proof BuiltinShaderIdentity
	var header [48]byte
	if _, err := input.ReadAt(header[:], 0); err != nil {
		return proof, err
	}
	if binary.BigEndian.Uint32(header[8:12]) != 23 || header[16] != 0 || size < 64 || size > 256<<20 || int64(binary.BigEndian.Uint64(header[24:32])) != size {
		return proof, errors.New("built-in Shader source requires bounded native SerializedFile v23")
	}
	metadataSize := int64(binary.BigEndian.Uint32(header[20:24]))
	dataOffset := int64(binary.BigEndian.Uint64(header[32:40]))
	if metadataSize > 4<<20 || metadataSize < 16 || 48+metadataSize > dataOffset || dataOffset > size {
		return proof, errors.New("built-in Shader native metadata bounds are invalid")
	}
	metadata := make([]byte, metadataSize)
	if _, err := input.ReadAt(metadata, 48); err != nil {
		return proof, err
	}
	r := reader{data: metadata}
	version, err := r.cstring(64)
	if err != nil || version != "6000.6.3f1" {
		return proof, errors.New("built-in Shader source requires pinned engine resources")
	}
	platform, err := r.u32()
	if err != nil || int32(platform) != -2 {
		return proof, errors.New("built-in Shader identity must use native Editor resources")
	}
	inline, err := r.take(1)
	if err != nil || inline[0] != 1 {
		return proof, errors.New("built-in Shader source requires inline native trees")
	}
	count, err := r.u32()
	if err != nil || count == 0 || count > 1024 {
		return proof, errors.New("built-in Shader type count is invalid")
	}
	types := make([]playerShaderType, count)
	for index := range types {
		class, err := r.u32()
		if err != nil {
			return proof, err
		}
		flags, err := r.take(3)
		if err != nil {
			return proof, err
		}
		if int16(binary.LittleEndian.Uint16(flags[1:])) >= 0 || class == 114 || int32(class) < 0 {
			if _, err := r.take(16); err != nil {
				return proof, err
			}
		}
		if _, err := r.take(32); err != nil {
			return proof, err
		}
		length, err := r.u32()
		if err != nil || length < 16 || length > 4<<20 {
			return proof, errors.New("built-in Shader tree bounds are invalid")
		}
		tree, err := r.take(int(length))
		if err != nil {
			return proof, err
		}
		if binary.LittleEndian.Uint32(tree[:4]) != 0x7474686d || binary.LittleEndian.Uint32(tree[4:8]) != 23 {
			return proof, errors.New("built-in Shader tree format is unsupported")
		}
		nodes, chars := uint64(binary.LittleEndian.Uint32(tree[8:12])), uint64(binary.LittleEndian.Uint32(tree[12:16]))
		if nodes == 0 || 16+nodes*32+chars != uint64(len(tree)) {
			return proof, errors.New("built-in Shader tree layout is invalid")
		}
		dependencies, err := r.u32()
		if err != nil || dependencies > 1024 {
			return proof, errors.New("built-in Shader type dependencies are invalid")
		}
		if _, err := r.take(int(dependencies) * 4); err != nil {
			return proof, err
		}
		types[index] = playerShaderType{class: int32(class), tree: tree}
	}
	objects, err := r.u32()
	if err != nil || objects == 0 || objects > 100000 {
		return proof, errors.New("built-in Shader object count is invalid")
	}
	r.pos = (r.pos + 3) &^ 3
	seen := make(map[int64]bool)
	for index := uint32(0); index < objects; index++ {
		row, err := r.take(24)
		if err != nil {
			return proof, err
		}
		localID := int64(binary.LittleEndian.Uint64(row[:8]))
		offset, length := int64(binary.LittleEndian.Uint64(row[8:16])), int64(binary.LittleEndian.Uint32(row[16:20]))
		typeIndex := binary.LittleEndian.Uint32(row[20:24])
		if localID == 0 || seen[localID] || typeIndex >= count || offset < 0 || length <= 0 || offset > size-dataOffset || length > size-dataOffset-offset {
			return proof, errors.New("built-in Shader object table is invalid")
		}
		seen[localID] = true
		if localID != id {
			continue
		}
		if types[typeIndex].class != 48 || length > 4<<20 {
			return proof, errors.New("built-in identity is not a bounded native Shader")
		}
		payload := make([]byte, length)
		if _, err := input.ReadAt(payload, dataOffset+offset); err != nil {
			return proof, err
		}
		name, err := builtinShaderName(payload, types[typeIndex].tree)
		if err == nil && name == "" {
			parsed, parseErr := readShaderIdentity(payload, types[typeIndex].tree, false)
			name, err = parsed.Name, parseErr
		}
		if err != nil {
			return proof, err
		}
		proof = BuiltinShaderIdentity{UnityVersion: version, FileID: id, Name: name, PayloadSHA256: uiDigest(payload)}
	}
	if proof.FileID == id && proof.Name == "" {
		return proof, errors.New("native built-in Shader has an empty top-level name")
	}
	if proof.Name == "" {
		return proof, errors.New("built-in Shader source identity is absent")
	}
	return proof, nil
}

func builtinShaderName(payload, tree []byte) (string, error) {
	count := int(binary.LittleEndian.Uint32(tree[8:12]))
	nodes := tree[16 : 16+count*32]
	chars := tree[16+count*32:]
	data := reader{data: payload}
	for i := 1; i < count; i++ {
		node := nodes[i*32 : (i+1)*32]
		if node[2] != 1 {
			continue
		}
		offset := binary.LittleEndian.Uint32(node[8:12])
		name := ""
		if offset&0x80000000 != 0 {
			name = playerShaderCommonStrings[offset&0x7fffffff]
		} else if offset < uint32(len(chars)) {
			r := reader{data: chars[offset:]}
			name, _ = r.cstring(256)
		}
		if name == "m_Name" {
			return data.string()
		}
		length := int32(binary.LittleEndian.Uint32(node[12:16]))
		if length < 0 {
			return "", fmt.Errorf("unverified built-in Shader identity prefix %s", name)
		}
		if _, err := data.take(int(length)); err != nil {
			return "", err
		}
		if binary.LittleEndian.Uint32(node[20:24])&0x4000 != 0 {
			data.pos = (data.pos + 3) &^ 3
		}
	}
	return "", errors.New("built-in Shader source has no native name")
}
