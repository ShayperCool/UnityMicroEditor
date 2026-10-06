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

type MeshCPUFlags struct {
	FileID               string `json:"fileId"`
	IsReadableOffset     int    `json:"isReadableOffset"`
	KeepVerticesOffset   int    `json:"keepVerticesOffset"`
	KeepIndicesOffset    int    `json:"keepIndicesOffset"`
	OriginalIsReadable   bool   `json:"originalIsReadable"`
	OriginalKeepVertices bool   `json:"originalKeepVertices"`
	OriginalKeepIndices  bool   `json:"originalKeepIndices"`
	PayloadSHA256        string `json:"payloadSha256"`
	TypeTreeSHA256       string `json:"typeTreeSha256"`
}

type MeshCPUProjection struct {
	SchemaVersion int            `json:"schemaVersion"`
	Mode          string         `json:"mode"`
	InputSHA256   string         `json:"inputSha256"`
	OutputSHA256  string         `json:"outputSha256"`
	Meshes        []MeshCPUFlags `json:"meshes"`
}

// RetainModelMeshCPU prepares an already projected native model for public
// Physics.BakeMesh. Only its three CPU-retention booleans may change; all
// vertices, indices, submeshes, physics blobs, bounds and other objects retain
// their imported bytes. The loader restores authored unreadability after bake.
func RetainModelMeshCPU(input []byte) ([]byte, MeshCPUProjection, error) {
	proof := MeshCPUProjection{SchemaVersion: 1, Mode: "native-model-mesh-cpu-v1", InputSHA256: meshCPUHash(input), Meshes: []MeshCPUFlags{}}
	if len(input) < 64 || len(input) > maximumBytes || binary.BigEndian.Uint32(input[8:12]) != 23 || input[16] != 0 ||
		binary.BigEndian.Uint64(input[24:32]) != uint64(len(input)) {
		return nil, proof, errors.New("Mesh CPU projection requires bounded little-endian SerializedFile v23")
	}
	dataOffset := binary.BigEndian.Uint64(input[32:40])
	metadataEnd := uint64(48) + uint64(binary.BigEndian.Uint32(input[20:24]))
	if metadataEnd > dataOffset || dataOffset > uint64(len(input)) {
		return nil, proof, errors.New("Mesh CPU projection header bounds are invalid")
	}
	r := reader{data: input[48:metadataEnd]}
	version, err := r.cstring(64)
	if err != nil || version != "0.0.0" && version != "6000.6.3f1" {
		return nil, proof, errors.New("Mesh CPU projection requires pinned Unity 6000.6.3f1")
	}
	target, err := r.u32()
	if err != nil || target != 24 {
		return nil, proof, errors.New("Mesh CPU projection requires an already projected Linux Player file")
	}
	inline, err := r.take(1)
	if err != nil || inline[0] != 1 {
		return nil, proof, errors.New("Mesh CPU projection requires inline native type trees")
	}
	typeCount, err := r.u32()
	if err != nil || typeCount == 0 || typeCount > 256 {
		return nil, proof, errors.New("Mesh CPU projection type count is invalid")
	}
	classes := make([]int32, typeCount)
	trees := make([][]byte, typeCount)
	for index := range classes {
		class, err := r.u32()
		if err != nil {
			return nil, proof, err
		}
		classes[index] = int32(class)
		flags, err := r.take(3)
		if err != nil || flags[0] != 0 || int16(binary.LittleEndian.Uint16(flags[1:])) != -1 || int32(class) < 0 || class == 114 {
			return nil, proof, errors.New("Mesh CPU projection contains an unsupported model type")
		}
		if _, err := r.take(32); err != nil {
			return nil, proof, err
		}
		size, err := r.u32()
		if err != nil || size < 16 || size > maximumBytes {
			return nil, proof, errors.New("Mesh CPU projection type tree is missing")
		}
		trees[index], err = r.take(int(size))
		if err != nil {
			return nil, proof, err
		}
		dependencies, err := r.u32()
		if err != nil || dependencies != 0 {
			return nil, proof, errors.New("Mesh CPU projection has unsupported serialized dependencies")
		}
	}
	count, err := r.u32()
	if err != nil || count == 0 || count > 100000 {
		return nil, proof, errors.New("Mesh CPU projection object count is invalid")
	}
	r.pos = (r.pos + 3) &^ 3
	output := bytes.Clone(input)
	seen := map[int64]bool{}
	for index := uint32(0); index < count; index++ {
		row, err := r.take(24)
		if err != nil {
			return nil, proof, err
		}
		id := int64(binary.LittleEndian.Uint64(row[:8]))
		offset, size, typeIndex := binary.LittleEndian.Uint64(row[8:16]), uint64(binary.LittleEndian.Uint32(row[16:20])), binary.LittleEndian.Uint32(row[20:24])
		if id == 0 || seen[id] || typeIndex >= typeCount || offset > uint64(len(input))-dataOffset || size > uint64(len(input))-dataOffset-offset {
			return nil, proof, errors.New("Mesh CPU projection object identity or bounds are invalid")
		}
		seen[id] = true
		if fbxClasses[classes[typeIndex]] == "" {
			return nil, proof, errors.New("Mesh CPU projection retained an unsupported native model object")
		}
		if classes[typeIndex] != 43 {
			continue
		}
		start := int(dataOffset + offset)
		payload := input[start : start+int(size)]
		flags, err := readMeshCPUFlags(payload, trees[typeIndex])
		if err != nil {
			return nil, proof, fmt.Errorf("Mesh %d CPU retention layout: %w", id, err)
		}
		if flags.OriginalIsReadable {
			continue
		}
		flags.FileID, flags.PayloadSHA256, flags.TypeTreeSHA256 = strconv.FormatInt(id, 10), meshCPUHash(payload), meshCPUHash(trees[typeIndex])
		flags.IsReadableOffset += start
		flags.KeepVerticesOffset += start
		flags.KeepIndicesOffset += start
		output[flags.IsReadableOffset], output[flags.KeepVerticesOffset], output[flags.KeepIndicesOffset] = 1, 1, 1
		proof.Meshes = append(proof.Meshes, flags)
	}
	masked := bytes.Clone(output)
	for _, flags := range proof.Meshes {
		for offset, original := range map[int]bool{flags.IsReadableOffset: flags.OriginalIsReadable, flags.KeepVerticesOffset: flags.OriginalKeepVertices, flags.KeepIndicesOffset: flags.OriginalKeepIndices} {
			masked[offset] = 0
			if original {
				masked[offset] = 1
			}
		}
	}
	if !bytes.Equal(input, masked) {
		return nil, proof, errors.New("Mesh CPU projection changed bytes outside declared boolean flags")
	}
	proof.OutputSHA256 = meshCPUHash(output)
	return output, proof, nil
}

func meshCPUHash(data []byte) string {
	hash := sha256.Sum256(data)
	return hex.EncodeToString(hash[:])
}

func readMeshCPUFlags(data, tree []byte) (MeshCPUFlags, error) {
	var result MeshCPUFlags
	if len(tree) < 16 || binary.LittleEndian.Uint32(tree[:4]) != 0x7474686d || binary.LittleEndian.Uint32(tree[4:8]) != 23 {
		return result, errors.New("Mesh type tree format is unverified")
	}
	count := int(binary.LittleEndian.Uint32(tree[8:12]))
	charsSize := uint64(binary.LittleEndian.Uint32(tree[12:16]))
	if count <= 0 || count > 10000 || uint64(16+count*32)+charsSize != uint64(len(tree)) {
		return result, errors.New("Mesh type tree bounds are invalid")
	}
	nodes, chars := tree[16:16+count*32], tree[16+count*32:]
	level := func(index int) byte { return nodes[index*32+2] }
	end := func(index int) int {
		next := index + 1
		for next < count && level(next) > level(index) {
			next++
		}
		return next
	}
	label := func(index, field int) string {
		offset := binary.LittleEndian.Uint32(nodes[index*32+field : index*32+field+4])
		if offset&0x80000000 != 0 {
			return playerShaderCommonStrings[offset&0x7fffffff]
		}
		if offset >= uint32(len(chars)) {
			return ""
		}
		value := chars[offset:]
		if last := bytes.IndexByte(value, 0); last >= 0 {
			return string(value[:last])
		}
		return ""
	}
	r := reader{data: data}
	budget, found := 2000000, 0
	seen := map[string]bool{}
	var walk func(int) error
	walk = func(index int) error {
		budget--
		if budget < 0 {
			return errors.New("Mesh prefix exceeds type tree traversal budget")
		}
		node, next := nodes[index*32:(index+1)*32], end(index)
		name, typeName := label(index, 8), label(index, 4)
		position := r.pos
		if name == "m_IsReadable" || name == "m_KeepVertices" || name == "m_KeepIndices" {
			if level(index) != 1 || typeName != "bool" || int32(binary.LittleEndian.Uint32(node[12:16])) != 1 || next != index+1 || seen[name] {
				return errors.New("Mesh CPU flags must be unique top-level bool fields")
			}
			value, err := r.take(1)
			if err != nil || value[0] > 1 {
				return errors.New("Mesh CPU flag is not a serialized bool")
			}
			seen[name], found = true, found+1
			switch name {
			case "m_IsReadable":
				result.IsReadableOffset, result.OriginalIsReadable = position, value[0] == 1
			case "m_KeepVertices":
				result.KeepVerticesOffset, result.OriginalKeepVertices = position, value[0] == 1
			case "m_KeepIndices":
				result.KeepIndicesOffset, result.OriginalKeepIndices = position, value[0] == 1
			}
		} else if typeName == "string" {
			if _, err := r.string(); err != nil {
				return err
			}
		} else if node[3]&1 != 0 {
			length, err := r.u32()
			if err != nil || length > maximumBytes || index+2 >= next || level(index+1) != level(index)+1 || level(index+2) != level(index)+1 {
				return errors.New("Mesh prefix array is oversized or malformed")
			}
			element := index + 2
			stride := int32(binary.LittleEndian.Uint32(nodes[element*32+12 : element*32+16]))
			align := binary.LittleEndian.Uint32(nodes[element*32+20:element*32+24])&0x4000 != 0
			if stride >= 0 && !align {
				bytes := uint64(length) * uint64(stride)
				if bytes > maximumBytes {
					return errors.New("Mesh prefix array exceeds byte bounds")
				}
				if _, err := r.take(int(bytes)); err != nil {
					return err
				}
			} else {
				for item := uint32(0); item < length; item++ {
					if err := walk(element); err != nil {
						return err
					}
				}
			}
		} else if index+1 < next {
			for child := index + 1; child < next; child = end(child) {
				if err := walk(child); err != nil {
					return err
				}
				if found == 3 {
					return nil
				}
			}
		} else if size := int32(binary.LittleEndian.Uint32(node[12:16])); size >= 0 {
			if _, err := r.take(int(size)); err != nil {
				return err
			}
		} else {
			return errors.New("Mesh prefix has an unsupported dynamic leaf")
		}
		if binary.LittleEndian.Uint32(node[20:24])&0x4000 != 0 {
			r.pos = (r.pos + 3) &^ 3
			if r.pos > len(data) {
				return errors.New("Mesh prefix alignment exceeds payload")
			}
		}
		return nil
	}
	if err := walk(0); err != nil {
		return result, err
	}
	if found != 3 {
		return result, errors.New("Mesh inline tree lacks the three CPU-retention flags")
	}
	return result, nil
}
