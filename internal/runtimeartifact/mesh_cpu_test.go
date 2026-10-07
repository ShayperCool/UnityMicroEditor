package runtimeartifact

import (
	"bytes"
	"encoding/binary"
	"os"
	"testing"
)

func assertMeshCPUProjection(t *testing.T, input []byte) ([]byte, MeshCPUProjection) {
	t.Helper()
	original := bytes.Clone(input)
	output, proof, err := RetainModelMeshCPU(input)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(input, original) || len(output) != len(input) || proof.InputSHA256 != meshCPUHash(input) || proof.OutputSHA256 != meshCPUHash(output) || len(proof.Meshes) == 0 {
		t.Fatalf("native Mesh CPU projection did not retain immutable input/payload shape: %+v", proof)
	}
	masked := bytes.Clone(output)
	for _, mesh := range proof.Meshes {
		if mesh.OriginalIsReadable || mesh.FileID == "" || mesh.PayloadSHA256 == "" || mesh.TypeTreeSHA256 == "" {
			t.Fatal("Mesh CPU original identity was lost")
		}
		for offset, value := range map[int]bool{mesh.IsReadableOffset: mesh.OriginalIsReadable, mesh.KeepVerticesOffset: mesh.OriginalKeepVertices, mesh.KeepIndicesOffset: mesh.OriginalKeepIndices} {
			if output[offset] != 1 {
				t.Fatal("Mesh CPU flag was not retained")
			}
			masked[offset] = 0
			if value {
				masked[offset] = 1
			}
		}
	}
	if !bytes.Equal(input, masked) {
		t.Fatal("native vertices/indices/physics/bounds/type tree changed outside the three declared bools")
	}
	second, secondProof, err := RetainModelMeshCPU(output)
	if err != nil || !bytes.Equal(second, output) || len(secondProof.Meshes) != 0 {
		t.Fatalf("already readable native Mesh was rewritten again: %+v %v", secondProof, err)
	}
	return output, proof
}

func TestRetainNativeRuntimeMeshCPUActualFile(t *testing.T) {
	path := os.Getenv("UNITY_WORKER_MODEL_CPU_FIXTURE")
	if path == "" {
		t.Skip("set UNITY_WORKER_MODEL_CPU_FIXTURE to an actual projected native FBX ContentFile")
	}
	input, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	_, proof := assertMeshCPUProjection(t, input)
	t.Logf("actual native model=%s meshes=%d inputSHA=%s outputSHA=%s first=%+v", path, len(proof.Meshes), proof.InputSHA256, proof.OutputSHA256, proof.Meshes[0])
}

func TestMeshCPUFlagsUseInlineLayoutAndPreserveVariablePrefix(t *testing.T) {
	tree := meshCPUFixtureTree(t)
	var prefix bytes.Buffer
	binary.Write(&prefix, binary.LittleEndian, uint32(8))
	prefix.WriteString("Variable")
	binary.Write(&prefix, binary.LittleEndian, uint32(3))
	prefix.Write([]byte{8, 4, 2, 0})
	prefix.Write([]byte{0, 0, 1, 255})
	flags, err := readMeshCPUFlags(prefix.Bytes(), tree)
	if err != nil || flags.IsReadableOffset != 20 || flags.KeepVerticesOffset != 21 || flags.KeepIndicesOffset != 22 || flags.OriginalIsReadable || !flags.OriginalKeepIndices {
		t.Fatalf("variable string/array shifted native CPU flag offsets incorrectly: %+v %v", flags, err)
	}
	for _, kind := range []string{"bool-value", "missing-flag", "nested-flag", "bad-size", "truncated"} {
		t.Run(kind, func(t *testing.T) {
			data, badTree := bytes.Clone(prefix.Bytes()), bytes.Clone(tree)
			switch kind {
			case "bool-value":
				data[20] = 2
			case "missing-flag":
				binary.LittleEndian.PutUint32(badTree[16+9*32+8:], 0)
			case "nested-flag":
				badTree[16+9*32+2] = 2
			case "bad-size":
				binary.LittleEndian.PutUint32(badTree[16+9*32+12:], 2)
			case "truncated":
				data = data[:21]
			}
			if _, err := readMeshCPUFlags(data, badTree); err == nil {
				t.Fatal("malformed/native-unproven CPU bool layout was accepted")
			}
		})
	}
}

func meshCPUFixtureTree(t *testing.T) []byte {
	t.Helper()
	strings := []string{"Mesh", "Base", "string", "m_Name", "vector", "prefix", "Array", "int", "size", "UInt8", "data", "bool", "m_IsReadable", "m_KeepVertices", "m_KeepIndices"}
	var chars bytes.Buffer
	offsets := map[string]uint32{}
	for _, str := range strings {
		offsets[str] = uint32(chars.Len())
		chars.WriteString(str)
		chars.WriteByte(0)
	}
	var nodes bytes.Buffer
	add := func(level, array byte, typ, name string, size int32, flags uint32) {
		var node [32]byte
		binary.LittleEndian.PutUint16(node[:2], 1)
		node[2], node[3] = level, array
		binary.LittleEndian.PutUint32(node[4:8], offsets[typ])
		binary.LittleEndian.PutUint32(node[8:12], offsets[name])
		binary.LittleEndian.PutUint32(node[12:16], uint32(size))
		binary.LittleEndian.PutUint32(node[20:24], flags)
		nodes.Write(node[:])
	}
	add(0, 0, "Mesh", "Base", -1, 0)
	add(1, 0, "string", "m_Name", -1, 0)
	add(2, 1, "Array", "Array", -1, 0x4000)
	add(3, 0, "int", "size", 4, 0)
	add(3, 0, "UInt8", "data", 1, 0)
	add(1, 0, "vector", "prefix", -1, 0)
	add(2, 1, "Array", "Array", -1, 0x4000)
	add(3, 0, "int", "size", 4, 0)
	add(3, 0, "UInt8", "data", 1, 0)
	add(1, 0, "bool", "m_IsReadable", 1, 0)
	add(1, 0, "bool", "m_KeepVertices", 1, 0)
	add(1, 0, "bool", "m_KeepIndices", 1, 0)
	var header [16]byte
	binary.LittleEndian.PutUint32(header[:4], 0x7474686d)
	binary.LittleEndian.PutUint32(header[4:8], 23)
	binary.LittleEndian.PutUint32(header[8:12], uint32(nodes.Len()/32))
	binary.LittleEndian.PutUint32(header[12:16], uint32(chars.Len()))
	return append(append(header[:], nodes.Bytes()...), chars.Bytes()...)
}
