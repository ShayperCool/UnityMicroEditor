package goproject

import (
	"encoding/hex"
	"errors"
	"fmt"
	"strings"
)

// These are source-authored Unity Mesh bytes, not an interchange model. FBX
// remains imported by the native worker. The Player verifies the actual public
// Mesh buffer/submesh/bounds readback before publishing the restored asset.
func validateAuthoredMesh(data map[string]any) error {
	if number(data["serializedVersion"]) != 12 || number(data["m_MeshCompression"]) != 0 {
		return errors.New("only uncompressed serializedVersion12 Mesh is supported")
	}
	bindposes := array(data["m_BindPose"])
	if len(bindposes) > 1024 || str(object(data["m_VariableBoneCountWeights"])["m_Data"]) != "" {
		return errors.New("variable-count or oversized bone data is unsupported")
	}
	if len(bindposes) == 0 && (str(data["m_BoneNameHashes"]) != "" || number(data["m_RootBoneNameHash"]) != 0 || len(array(data["m_BonesAABB"])) != 0) {
		return errors.New("bone metadata without bindposes is unsupported")
	}
	if len(bindposes) > 0 {
		hashes, err := meshHex(str(data["m_BoneNameHashes"]))
		if err != nil || len(hashes) != 4*len(bindposes) || len(array(data["m_BonesAABB"])) != len(bindposes) {
			return errors.New("source bone metadata does not match bindpose count")
		}
	}
	for _, value := range object(data["m_Shapes"]) {
		if len(array(value)) != 0 {
			return errors.New("blend shapes are unsupported")
		}
	}
	for name, value := range object(data["m_CompressedMesh"]) {
		if name == "m_UVInfo" {
			if number(value) != 0 {
				return errors.New("compressed Mesh UV metadata is unsupported")
			}
			continue
		}
		packed := object(value)
		if number(packed["m_NumItems"]) != 0 || str(packed["m_Data"]) != "" {
			return errors.New("packed Mesh data is unsupported")
		}
	}
	stream := object(data["m_StreamData"])
	if number(stream["offset"]) != 0 || number(stream["size"]) != 0 || str(stream["path"]) != "" {
		return errors.New("streamed Mesh data is unsupported")
	}
	vertex := object(data["m_VertexData"])
	if number(vertex["serializedVersion"]) != 3 || number(vertex["m_VertexCount"]) <= 0 || number(vertex["m_VertexCount"]) > 4*1024*1024 {
		return errors.New("invalid source Mesh vertex metadata")
	}
	vertexBytes, err := meshHex(str(vertex["_typelessdata"]))
	if err != nil {
		return err
	}
	if len(vertexBytes) != int(number(vertex["m_DataSize"])) || len(vertexBytes) > 64<<20 {
		return errors.New("source Mesh vertex bytes do not match m_DataSize")
	}
	indexBytes, err := meshHex(str(data["m_IndexBuffer"]))
	if err != nil {
		return err
	}
	indexFormat := number(data["m_IndexFormat"])
	if indexFormat < 0 || indexFormat > 1 || len(indexBytes) == 0 || len(indexBytes) > 64<<20 || len(indexBytes)%(2+2*int(indexFormat)) != 0 {
		return errors.New("invalid source Mesh index buffer")
	}
	channels := array(vertex["m_Channels"])
	if len(channels) != 14 {
		return errors.New("source Mesh requires the pinned 14 attribute channels")
	}
	for index, value := range channels {
		channel := object(value)
		dimension := number(channel["dimension"])
		if dimension < 0 || dimension > 4 {
			return errors.New("packed channel dimensions are unsupported")
		}
		if dimension == 0 {
			continue
		}
		format := number(channel["format"])
		if number(channel["stream"]) < 0 || number(channel["stream"]) > 3 || number(channel["offset"]) < 0 || !(format == 0 || format == 2 || format == 10) {
			return errors.New("only float32/UNorm8/UInt32 Mesh attributes in up to four streams are currently supported")
		}
		if index >= 12 && (len(bindposes) == 0 || dimension != 4 || index == 12 && format != 0 || index == 13 && format != 10) {
			return errors.New("only fixed four-weight skinning with float32 weights and UInt32 bone indices is supported")
		}
	}
	if number(object(channels[0])["dimension"]) != 3 {
		return errors.New("Mesh needs float32 position3")
	}
	if len(bindposes) != 0 && (number(object(channels[12])["dimension"]) != 4 || number(object(channels[13])["dimension"]) != 4) {
		return errors.New("skinned Mesh lacks fixed four-weight channels")
	}
	if len(array(data["m_SubMeshes"])) == 0 {
		return errors.New("Mesh has no submeshes")
	}
	var references error
	visitReferences(data, "", func(name string, r map[string]any) {
		if fileID(r) != "0" {
			references = fmt.Errorf("native Mesh reference %s requires a supported native importer", name)
		}
	})
	return references
}

func meshHex(value string) ([]byte, error) {
	// Unity's binary YAML scalar marker prevents digit-only buffers from being
	// interpreted as a YAML number. It is a suffix, not part of the byte data.
	value = strings.TrimSuffix(value, "y")
	value = strings.TrimSuffix(value, "u")
	if len(value) > 128<<20 {
		return nil, errors.New("Mesh hex bytes exceed bounds")
	}
	decoded, err := hex.DecodeString(value)
	if err != nil {
		return nil, fmt.Errorf("invalid source Mesh hex bytes: %w", err)
	}
	return decoded, nil
}
