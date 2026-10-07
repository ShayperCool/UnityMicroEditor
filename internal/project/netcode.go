package goproject

import (
	"encoding/binary"
	"fmt"
	"math/bits"
	"path/filepath"
	"strconv"
	"strings"
)

// NetworkObject.OnValidate stores XXHash32(GlobalObjectId.ToString()) in
// serialized content. Source prefabs can legitimately still contain zero before
// Editor validation. Reproduce that derived field in the published descriptor,
// leaving authored YAML untouched and preserving existing nonzero hashes.
// GlobalObjectId format: UnityCsReference/Editor/Mono/GlobalObjectId.bindings.cs.
func derivedNetworkData(record *expandedObject, typeName string, source map[string]any) (map[string]any, error) {
	if typeName != "Unity.Netcode.NetworkObject" || number(source["GlobalObjectIdHash"]) != 0 {
		return source, nil
	}
	if filepath.Ext(record.SourcePath) != ".prefab" {
		return nil, fmt.Errorf("NetworkObject has no validated hash and is not an authored source prefab: %s/%s", record.SourcePath, record.FileID)
	}
	local := record.FileID
	if separator := strings.LastIndex(local, ":"); separator >= 0 {
		local = local[separator+1:]
	}
	if !record.Targets[targetKey(record.GUID, local)] {
		return nil, fmt.Errorf("NetworkObject lacks an exact source prefab identity: %s/%s", record.SourcePath, record.FileID)
	}
	identifier, err := strconv.ParseInt(local, 10, 64)
	if err != nil {
		return nil, fmt.Errorf("NetworkObject source file ID is not int64: %s", local)
	}
	global := fmt.Sprintf("GlobalObjectId_V1-3-%s-%d-0", record.GUID, uint64(identifier))
	hash := xxHash32([]byte(global))
	if hash == 0 {
		return nil, fmt.Errorf("NetworkObject derived hash is zero: %s", global)
	}
	result := make(map[string]any, len(source))
	for key, value := range source {
		result[key] = value
	}
	result["GlobalObjectIdHash"] = int64(hash)
	return result, nil
}

func xxHash32(input []byte) uint32 {
	const p1 uint32 = 2654435761
	const p2 uint32 = 2246822519
	const p3 uint32 = 3266489917
	const p4 uint32 = 668265263
	const p5 uint32 = 374761393
	length := len(input)
	hash := p5
	if length >= 16 {
		v0, v1, v2, v3 := p1, p2, uint32(0), uint32(0)
		v0 += p2
		v3 -= p1
		for len(input) >= 16 {
			v0 = bits.RotateLeft32(v0+binary.LittleEndian.Uint32(input)*p2, 13) * p1
			v1 = bits.RotateLeft32(v1+binary.LittleEndian.Uint32(input[4:])*p2, 13) * p1
			v2 = bits.RotateLeft32(v2+binary.LittleEndian.Uint32(input[8:])*p2, 13) * p1
			v3 = bits.RotateLeft32(v3+binary.LittleEndian.Uint32(input[12:])*p2, 13) * p1
			input = input[16:]
		}
		hash = bits.RotateLeft32(v0, 1) + bits.RotateLeft32(v1, 7) + bits.RotateLeft32(v2, 12) + bits.RotateLeft32(v3, 18)
	}
	hash += uint32(length)
	for len(input) >= 4 {
		hash = bits.RotateLeft32(hash+binary.LittleEndian.Uint32(input)*p3, 17) * p4
		input = input[4:]
	}
	for _, value := range input {
		hash = bits.RotateLeft32(hash+uint32(value)*p5, 11) * p1
	}
	hash ^= hash >> 15
	hash *= p2
	hash ^= hash >> 13
	hash *= p3
	hash ^= hash >> 16
	return hash
}
