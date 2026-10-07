package runtimeartifact

import (
	"bytes"
	"encoding/binary"
	"encoding/hex"
	"errors"
	"fmt"
	"strconv"
	"strings"
)

// PlayerShaderMetadata identifies the source asset embedded by Unity in a
// compiled Player Shader. The archive-local path ID is not a source file ID.
type PlayerShaderMetadata struct {
	GUID           string   `json:"guid"`
	SourceFileID   string   `json:"sourceFileId"`
	ArchiveFileID  string   `json:"archiveFileId"`
	Name           string   `json:"name"`
	Platforms      []uint32 `json:"platforms"`
	CompiledBytes  int      `json:"compiledBytes"`
	TypeTreeSHA256 string   `json:"typeTreeSha256"`
	PayloadSHA256  string   `json:"payloadSha256"`
	InputSHA256    string   `json:"inputSha256"`
	OutputSHA256   string   `json:"outputSha256"`
	BuiltinGUID    string   `json:"builtinGuid,omitempty"`
	BuiltinFileID  string   `json:"builtinFileId,omitempty"`
}

type playerShaderType struct {
	class int32
	bytes []byte
	tree  []byte
}

// ExtractPlayerShader keeps one exact compiled Shader payload and its verified
// inline tree from a Linux SerializedFile. Every object reference must be null
// before the unrelated external and object tables can be omitted. Scene objects
// and their data never enter the output file. GPU programs are not compiled or
// translated here.
func ExtractPlayerShader(input []byte, guid, name string) ([]byte, PlayerShaderMetadata, error) {
	return extractPlayerShader(input, guid, name, 4800000)
}

// Unity's Linux built-in-assets snapshot contains the exact Panoramic shader
// under its build-pipeline source GUID. Alias only that verified shader to the
// authored unity_builtin_extra identity; retain every compiled payload byte.
func ExtractBuiltinPlayerShader(input []byte, id int64) ([]byte, PlayerShaderMetadata, error) {
	if id != 108 {
		return nil, PlayerShaderMetadata{}, errors.New("unsupported compiled built-in Shader identity")
	}
	output, proof, err := extractPlayerShader(input, "f57c2bc4f17ba48d9b9b6758dcfe812d", "Skybox/Panoramic", id)
	if err == nil {
		proof.BuiltinGUID, proof.BuiltinFileID = "0000000000000000f000000000000000", "108"
	}
	return output, proof, err
}

func extractPlayerShader(input []byte, guid, name string, outputID int64) ([]byte, PlayerShaderMetadata, error) {
	var proof PlayerShaderMetadata
	if decoded, err := hex.DecodeString(guid); err != nil || len(decoded) != 16 || guid != strings.ToLower(guid) || name == "" {
		return nil, proof, errors.New("compiled Shader requires exact lowercase source GUID and Shader name")
	}
	if len(input) < 64 || len(input) > maximumBytes || binary.BigEndian.Uint32(input[8:12]) != 23 || input[16] != 0 {
		return nil, proof, errors.New("compiled Shader requires little-endian SerializedFile v23")
	}
	fileSize, dataOffset := binary.BigEndian.Uint64(input[24:32]), binary.BigEndian.Uint64(input[32:40])
	metadataEnd := uint64(48) + uint64(binary.BigEndian.Uint32(input[20:24]))
	if fileSize != uint64(len(input)) || metadataEnd > dataOffset || dataOffset > fileSize {
		return nil, proof, errors.New("compiled Shader SerializedFile header bounds are invalid")
	}
	r := reader{data: input[48:metadataEnd]}
	version, err := r.cstring(64)
	if err != nil || version != "6000.6.3f1" {
		return nil, proof, errors.New("compiled Shader uses an unverified Unity version")
	}
	platform, err := r.u32()
	if err != nil || platform != 24 {
		return nil, proof, errors.New("compiled Shader must come from Linux Player target24")
	}
	inline, err := r.take(1)
	if err != nil || inline[0] != 1 {
		return nil, proof, errors.New("compiled Shader requires inline type trees")
	}
	typeCount, err := r.u32()
	if err != nil || typeCount == 0 || typeCount > 1024 {
		return nil, proof, errors.New("compiled Shader type count is invalid")
	}
	types := make([]playerShaderType, typeCount)
	for index := range types {
		start := r.pos
		class, err := r.u32()
		if err != nil {
			return nil, proof, err
		}
		flags, err := r.take(3)
		if err != nil {
			return nil, proof, err
		}
		if int16(binary.LittleEndian.Uint16(flags[1:])) >= 0 || int32(class) == 114 || int32(class) < 0 {
			if _, err := r.take(16); err != nil {
				return nil, proof, err
			}
		}
		if _, err := r.take(32); err != nil {
			return nil, proof, err
		}
		size, err := r.u32()
		if err != nil || size < 16 || size > maximumBytes {
			return nil, proof, errors.New("compiled Shader inline tree size is invalid")
		}
		tree, err := r.take(int(size))
		if err != nil {
			return nil, proof, err
		}
		if binary.LittleEndian.Uint32(tree[:4]) != 0x7474686d || binary.LittleEndian.Uint32(tree[4:8]) != 23 {
			return nil, proof, errors.New("compiled Shader inline tree format is invalid")
		}
		count, chars := uint64(binary.LittleEndian.Uint32(tree[8:12])), uint64(binary.LittleEndian.Uint32(tree[12:16]))
		if count == 0 || 16+count*32+chars != uint64(len(tree)) {
			return nil, proof, errors.New("compiled Shader inline tree bounds are invalid")
		}
		deps, err := r.u32()
		if err != nil || deps > 1024 {
			return nil, proof, errors.New("compiled Shader type dependency count is invalid")
		}
		if _, err := r.take(int(deps) * 4); err != nil {
			return nil, proof, err
		}
		if class == 48 && (flags[0] != 0 || int16(binary.LittleEndian.Uint16(flags[1:])) != -1 || deps != 0) {
			return nil, proof, errors.New("compiled Shader type has unsupported stripped/script dependencies")
		}
		types[index] = playerShaderType{int32(class), r.data[start:r.pos], tree}
	}
	objects, err := r.u32()
	if err != nil || objects == 0 || objects > 100000 {
		return nil, proof, errors.New("compiled Shader object count is invalid")
	}
	r.pos = (r.pos + 3) &^ 3
	var chosenType playerShaderType
	var chosenPayload []byte
	var chosenID int64
	seen := map[int64]bool{}
	for index := uint32(0); index < objects; index++ {
		row, err := r.take(24)
		if err != nil {
			return nil, proof, err
		}
		id := int64(binary.LittleEndian.Uint64(row[:8]))
		offset, size := binary.LittleEndian.Uint64(row[8:16]), uint64(binary.LittleEndian.Uint32(row[16:20]))
		typeIndex := binary.LittleEndian.Uint32(row[20:24])
		if id == 0 || seen[id] || typeIndex >= typeCount || offset > fileSize-dataOffset || size > fileSize-dataOffset-offset {
			return nil, proof, errors.New("compiled Shader object identity/bounds are invalid")
		}
		seen[id] = true
		if types[typeIndex].class != 48 {
			continue
		}
		payload := input[dataOffset+offset : dataOffset+offset+size]
		candidate, err := readPlayerShader(payload, types[typeIndex].tree)
		if err != nil {
			return nil, proof, fmt.Errorf("compiled Shader %d: %w", id, err)
		}
		if candidate.GUID != guid {
			continue
		}
		if candidate.Name != name || candidate.SourceFileID != "4800000" {
			return nil, proof, errors.New("compiled Shader embedded source GUID disagrees with name/local identity")
		}
		if chosenPayload != nil {
			return nil, proof, errors.New("compiled Shader source GUID is duplicated in archive")
		}
		proof, chosenType, chosenPayload, chosenID = candidate, types[typeIndex], payload, id
	}
	if chosenPayload == nil {
		return nil, proof, errors.New("compiled Shader source GUID is absent from archive")
	}
	// The original tail is irrelevant to this proven reference-free Shader.
	// Preserve only its type record and payload; construct canonical empty tables.
	var metadata bytes.Buffer
	metadata.WriteString(version)
	metadata.WriteByte(0)
	writeShaderU32(&metadata, 24)
	metadata.WriteByte(1)
	writeShaderU32(&metadata, 1)
	metadata.Write(chosenType.bytes)
	writeShaderU32(&metadata, 1)
	for metadata.Len()%4 != 0 {
		metadata.WriteByte(0)
	}
	var row [24]byte
	binary.LittleEndian.PutUint64(row[:8], uint64(outputID))
	binary.LittleEndian.PutUint32(row[16:20], uint32(len(chosenPayload)))
	metadata.Write(row[:])
	writeShaderU32(&metadata, 0) // Script references.
	writeShaderU32(&metadata, 0) // External references, all Shader PPtrs proven null.
	writeShaderU32(&metadata, 0) // Serialized reference types.
	metadata.WriteByte(0)
	outputData := (48 + metadata.Len() + 15) &^ 15
	output := make([]byte, outputData+len(chosenPayload))
	binary.BigEndian.PutUint32(output[8:12], 23)
	binary.BigEndian.PutUint32(output[20:24], uint32(metadata.Len()))
	binary.BigEndian.PutUint64(output[24:32], uint64(len(output)))
	binary.BigEndian.PutUint64(output[32:40], uint64(outputData))
	copy(output[48:], metadata.Bytes())
	copy(output[outputData:], chosenPayload)
	proof.ArchiveFileID, proof.TypeTreeSHA256 = strconv.FormatInt(chosenID, 10), uiDigest(chosenType.tree)
	proof.PayloadSHA256, proof.InputSHA256, proof.OutputSHA256 = uiDigest(chosenPayload), uiDigest(input), uiDigest(output)
	return output, proof, nil
}

func writeShaderU32(buffer *bytes.Buffer, value uint32) {
	var data [4]byte
	binary.LittleEndian.PutUint32(data[:], value)
	buffer.Write(data[:])
}

// readPlayerShader traverses every inline-tree field, including arrays, so
// retaining opaque GPU blobs cannot conceal a live serialized Object reference.
func readPlayerShader(data, tree []byte) (PlayerShaderMetadata, error) {
	return readShaderIdentity(data, tree, true)
}

var shaderSourceNameFound = errors.New("native Shader source name found")

func readShaderIdentity(data, tree []byte, compiled bool) (PlayerShaderMetadata, error) {
	var proof PlayerShaderMetadata
	count := int(binary.LittleEndian.Uint32(tree[8:12]))
	if count > 10000 {
		return proof, errors.New("compiled Shader tree is oversized")
	}
	nodes, chars := tree[16:16+count*32], tree[16+count*32:]
	level := func(i int) byte { return nodes[i*32+2] }
	end := func(i int) int {
		j := i + 1
		for j < count && level(j) > level(i) {
			j++
		}
		return j
	}
	label := func(i, field int) (string, error) {
		offset := binary.LittleEndian.Uint32(nodes[i*32+field : i*32+field+4])
		if offset&0x80000000 != 0 {
			value, ok := playerShaderCommonStrings[offset&0x7fffffff]
			if !ok {
				return "", fmt.Errorf("compiled Shader uses an unverified common string offset %d", offset&0x7fffffff)
			}
			return value, nil
		}
		if offset >= uint32(len(chars)) {
			return "", fmt.Errorf("compiled Shader tree string offset is invalid: node %d field %d offset %x string bytes %d", i, field, offset, len(chars))
		}
		value := chars[offset:]
		last := bytes.IndexByte(value, 0)
		if last < 0 {
			return "", errors.New("compiled Shader tree string is unterminated")
		}
		return string(value[:last]), nil
	}
	r := reader{data: data}
	budget := 2000000
	var walk func(int, string) error
	walk = func(i int, parent string) error {
		budget--
		if budget < 0 {
			return errors.New("compiled Shader tree exceeds traversal budget")
		}
		node := nodes[i*32 : (i+1)*32]
		next := end(i)
		typeName, err := label(i, 4)
		if err != nil {
			return err
		}
		name, err := label(i, 8)
		if err != nil {
			return err
		}
		path := name
		if parent != "" {
			path = parent + "." + name
		}
		start := r.pos
		if strings.HasPrefix(typeName, "PPtr<") {
			if int32(binary.LittleEndian.Uint32(node[12:16])) != 12 {
				return errors.New("compiled Shader PPtr layout is unsupported")
			}
			value, err := r.take(12)
			if err != nil {
				return err
			}
			if compiled && !bytes.Equal(value, make([]byte, 12)) {
				return fmt.Errorf("compiled Shader has a live dependency at %s", path)
			}
		} else if typeName == "string" {
			value, err := r.string()
			if err != nil {
				return err
			}
			if path == "Base.m_ParsedForm.m_Name" || path == "Shader.m_ParsedForm.m_Name" {
				proof.Name = value
				if !compiled {
					return shaderSourceNameFound
				}
			}
		} else if node[3]&1 != 0 {
			length, err := r.u32()
			if err != nil || length > maximumBytes {
				return errors.New("compiled Shader array size is invalid")
			}
			if i+2 >= next || level(i+1) != level(i)+1 || level(i+2) != level(i)+1 {
				return errors.New("compiled Shader array tree is malformed")
			}
			element := i + 2
			elementType, err := label(element, 4)
			if err != nil {
				return err
			}
			byteSize := int32(binary.LittleEndian.Uint32(nodes[element*32+12 : element*32+16]))
			if byteSize == 1 && end(element) == element+1 && binary.LittleEndian.Uint32(nodes[element*32+20:element*32+24])&0x4000 == 0 {
				if _, err := r.take(int(length)); err != nil {
					return err
				}
				if name == "Array" && (parent == "Base.compressedBlob" || parent == "Shader.compressedBlob") {
					proof.CompiledBytes = int(length)
				}
			} else {
				if length > 200000 {
					return errors.New("compiled Shader structured array is oversized")
				}
				for item := uint32(0); item < length; item++ {
					position := r.pos
					if err := walk(element, path); err != nil {
						return err
					}
					if (parent == "Base.platforms" || parent == "Shader.platforms") && byteSize == 4 && (elementType == "unsigned int" || elementType == "UInt32") {
						proof.Platforms = append(proof.Platforms, binary.LittleEndian.Uint32(data[position:position+4]))
					}
				}
			}
		} else if name == "m_AssetGUID" && typeName == "GUID" {
			value, err := r.take(16)
			if err != nil {
				return err
			}
			ordinary := bytes.Clone(value)
			for j, v := range ordinary {
				ordinary[j] = v<<4 | v>>4
			}
			proof.GUID = hex.EncodeToString(ordinary)
		} else if name == "m_AssetLocalIdentifierInFile" && typeName == "SInt64" {
			value, err := r.take(8)
			if err != nil {
				return err
			}
			proof.SourceFileID = strconv.FormatInt(int64(binary.LittleEndian.Uint64(value)), 10)
		} else if i+1 < next {
			for child := i + 1; child < next; child = end(child) {
				if err := walk(child, path); err != nil {
					return err
				}
			}
		} else if size := int32(binary.LittleEndian.Uint32(node[12:16])); size >= 0 {
			if _, err := r.take(int(size)); err != nil {
				return err
			}
			if name == "m_ShaderIsBaked" && (size != 1 || data[start] != 1) {
				return errors.New("Shader object is not Player-baked")
			}
		} else {
			return fmt.Errorf("compiled Shader has unsupported dynamic leaf %s", path)
		}
		if binary.LittleEndian.Uint32(node[20:24])&0x4000 != 0 {
			r.pos = (r.pos + 3) &^ 3
			if r.pos > len(data) {
				return errors.New("compiled Shader alignment exceeds payload")
			}
		}
		return nil
	}
	if err := walk(0, ""); err != nil {
		if !compiled && errors.Is(err, shaderSourceNameFound) {
			return proof, nil
		}
		return proof, err
	}
	if r.pos != len(data) {
		return proof, errors.New("compiled Shader payload does not match its entire inline tree")
	}
	if proof.GUID == "" || proof.SourceFileID == "" || proof.Name == "" || proof.CompiledBytes == 0 || len(proof.Platforms) != 1 || proof.Platforms[0] != 15 {
		return proof, errors.New("compiled Shader requires embedded source identity and GLCore GPU program data")
	}
	return proof, nil
}

// Unity's common type-tree string offsets are part of the serialized format.
// This narrow subset is independently readable with the installation's
// binary2text tool; unknown names are rejected rather than guessed.
var playerShaderCommonStrings = map[uint32]string{
	1093: "m_CorrespondingSourceObject", 1121: "m_PrefabInstance", 1138: "m_PrefabAsset",
	646: "PPtr<Prefab>",
	519: "m_Type", 526: "m_Version", 288: "m_ByteSize", 349: "m_Enabled", 387: "m_Index", 395: "m_IsArray", 416: "m_MetaFlag",
	49: "Array", 55: "Base", 76: "bool", 81: "char", 86: "ColorRGBA", 106: "data", 117: "double", 155: "first", 161: "float", 208: "GUID", 222: "int", 231: "long long", 241: "map", 245: "Matrix4x4f", 427: "m_Name", 434: "m_ObjectHideFlags", 536: "Object", 543: "pair", 633: "PPtr<Object>", 688: "PPtr<Texture>", 702: "PPtr<Texture2D>", 778: "second", 789: "short", 795: "size", 800: "SInt16", 807: "SInt32", 814: "SInt64", 821: "SInt8", 840: "string", 894: "TypelessData", 907: "UInt16", 914: "UInt32", 921: "UInt64", 928: "UInt8", 934: "unsigned int", 947: "unsigned long long", 966: "unsigned short", 981: "vector", 988: "Vector2f", 997: "Vector3f", 1006: "Vector4f", 1161: "Hash128",
}
