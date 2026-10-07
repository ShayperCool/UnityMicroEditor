package main

import (
	"bytes"
	"context"
	"encoding/binary"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func udsStorageRecord(hash []byte, kind uint32, file, offset, size uint64) LMDBRecord {
	value := make([]byte, 32)
	binary.LittleEndian.PutUint32(value[:4], kind)
	binary.LittleEndian.PutUint64(value[8:16], file)
	binary.LittleEndian.PutUint64(value[16:24], offset)
	binary.LittleEndian.PutUint64(value[24:32], size)
	return LMDBRecord{Key: hash, Value: value}
}

func TestUDSReadsExactInlineAndConcatContent(t *testing.T) {
	directory := t.TempDir()
	inline, concat := []byte("inline exact bytes\x00"), []byte("file exact bytes\x01")
	inlineKey, concatKey := nativeSourceHash(inline), nativeSourceHash(concat)
	inlineHash, concatHash := inlineKey[:], concatKey[:]
	data := append([]byte("prefix"), concat...)
	data = append(data, []byte("suffix")...)
	if err := os.WriteFile(filepath.Join(directory, "UDSData_7.bin"), data, 0600); err != nil {
		t.Fatal(err)
	}
	store, err := artifactStoreFromRecords(directory, []LMDBRecord{
		udsStorageRecord(inlineHash, 3, 0, 0, uint64(len(inline))),
		udsStorageRecord(concatHash, 2, 7, 6, uint64(len(concat))),
	}, []LMDBRecord{{Key: inlineHash, Value: inline}})
	if err != nil {
		t.Fatal(err)
	}
	for index, hash := range [][]byte{inlineHash, concatHash} {
		content, proof, err := store.Read(hash)
		expected := [][]byte{inline, concat}[index]
		if err != nil || !bytes.Equal(content, expected) || proof.SHA256 != bytesSHA(expected) || !proof.ContentKeyVerified {
			t.Fatalf("content mismatch: %+v %v", proof, err)
		}
	}
	output := filepath.Join(t.TempDir(), "asset.bin")
	if _, err = store.Extract(concatHash, output); err != nil {
		t.Fatal(err)
	}
	if _, err = store.Extract(concatHash, output); err == nil {
		t.Fatal("existing extraction overwritten")
	}
	if _, err = store.Extract(concatHash, filepath.Join(directory, "must-not-write")); err == nil {
		t.Fatal("extraction wrote into input DataStore")
	}
	store.content[string(inlineHash)][0] ^= 1
	if _, _, err = store.Read(inlineHash); err == nil {
		t.Fatal("corrupted inline content passed native hash verification")
	}
	data[6] ^= 1
	if err = os.WriteFile(filepath.Join(directory, "UDSData_7.bin"), data, 0600); err != nil {
		t.Fatal(err)
	}
	if _, _, err = store.Read(concatHash); err == nil {
		t.Fatal("corrupted concatenated content passed native hash verification")
	}
}

func TestUDSRejectsTruncatedAndUnsupportedStorage(t *testing.T) {
	directory := t.TempDir()
	hash := bytes.Repeat([]byte{3}, 16)
	if err := os.WriteFile(filepath.Join(directory, "UDSData_0.bin"), []byte("short"), 0600); err != nil {
		t.Fatal(err)
	}
	for _, record := range []LMDBRecord{
		udsStorageRecord(hash, 2, 0, 4, 2),
		udsStorageRecord(hash, 2, 0, ^uint64(0), 2),
		udsStorageRecord(hash, 3, 0, 0, 2),
		udsStorageRecord(hash, 1, 0, 0, 2),
		udsStorageRecord(hash, 2, 0, 0, maxArtifactBytes+1),
	} {
		store, err := artifactStoreFromRecords(directory, []LMDBRecord{record}, nil)
		if err != nil {
			t.Fatal(err)
		}
		if _, _, err = store.Read(hash); err == nil {
			t.Fatalf("accepted bad record %+v", record)
		}
	}
	if _, err := artifactStoreFromRecords(directory, []LMDBRecord{{Key: hash, Value: []byte{1}}}, nil); err == nil {
		t.Fatal("accepted truncated storage record")
	}
}

func TestSerializedArtifactHeaderRejectsInvalidLengths(t *testing.T) {
	content := make([]byte, 256)
	binary.BigEndian.PutUint32(content[8:12], 23)
	binary.BigEndian.PutUint32(content[20:24], 100)
	binary.BigEndian.PutUint64(content[24:32], 256)
	binary.BigEndian.PutUint64(content[32:40], 160)
	if _, err := ReadSerializedArtifactHeader(content); err != nil {
		t.Fatal(err)
	}
	for _, mutate := range []func([]byte){
		func(data []byte) { binary.BigEndian.PutUint64(data[24:32], 255) },
		func(data []byte) { binary.BigEndian.PutUint64(data[32:40], 120) },
		func(data []byte) { binary.BigEndian.PutUint64(data[32:40], ^uint64(0)) },
		func(data []byte) { binary.BigEndian.PutUint32(data[8:12], 26) },
		func(data []byte) { data[16] = 2 },
	} {
		invalid := bytes.Clone(content)
		mutate(invalid)
		if _, err := ReadSerializedArtifactHeader(invalid); err == nil {
			t.Fatal("accepted malformed SerializedFile header")
		}
	}
}

// This optional integration test inspects a stopped native reference store. It
// starts only binary2text and provides evidence about reading imported data,
// not evidence that our own coordinator produced that data.
func TestUDSNativeReferenceExtraction(t *testing.T) {
	directory := os.Getenv("UNITY_WORKER_ARTIFACT_REFERENCE")
	output := os.Getenv("UNITY_WORKER_ARTIFACT_OUTPUT")
	editor := os.Getenv("UNITY_WORKER_ARTIFACT_EDITOR")
	if directory == "" || output == "" || editor == "" {
		t.Skip("native reference paths not supplied")
	}
	store, err := OpenUDSArtifactStore(directory)
	if err != nil {
		t.Fatal(err)
	}
	if err = os.MkdirAll(output, 0700); err != nil {
		t.Fatal(err)
	}
	for key := range store.storage {
		data, proof, err := store.Read([]byte(key))
		if err != nil || !bytes.Contains(data, []byte("Shell_Floor")) {
			continue
		}
		if _, err = ReadSerializedArtifactHeader(data); err != nil {
			continue
		}
		extracted := filepath.Join(output, proof.RawHashKey+".serialized")
		proof, err = store.Extract([]byte(key), extracted)
		if err != nil {
			t.Fatal(err)
		}
		inspection, err := InspectSerializedArtifact(context.Background(), editor, extracted, extracted+".txt")
		if err != nil {
			t.Fatal(err)
		}
		dump, err := os.ReadFile(extracted + ".txt")
		if err != nil {
			t.Fatal(err)
		}
		fingerprint, err := FingerprintFixtureMesh(dump)
		if err != nil {
			t.Fatal(err)
		}
		if oraclePath := os.Getenv("UNITY_WORKER_REFERENCE_REPORT"); oraclePath != "" {
			var oracle struct {
				Model struct {
					Meshes []struct {
						VertexCount     int    `json:"vertexCount"`
						VerticesSHA256  string `json:"verticesSha256"`
						NormalsSHA256   string `json:"normalsSha256"`
						TangentsSHA256  string `json:"tangentsSha256"`
						UVSHA256        string `json:"uvSha256"`
						BindposesSHA256 string `json:"bindposesSha256"`
						Submeshes       []struct {
							IndexCount    int    `json:"indexCount"`
							IndicesSHA256 string `json:"indicesSha256"`
						} `json:"submeshes"`
					} `json:"meshes"`
				} `json:"model"`
			}
			encoded, err := os.ReadFile(oraclePath)
			if err != nil {
				t.Fatal(err)
			}
			if err = json.Unmarshal(encoded, &oracle); err != nil {
				t.Fatal(err)
			}
			if len(oracle.Model.Meshes) != 1 || len(oracle.Model.Meshes[0].Submeshes) != 1 {
				t.Fatal("oracle mesh layout differs")
			}
			expected := oracle.Model.Meshes[0]
			if fingerprint.VertexCount != expected.VertexCount || fingerprint.IndexCount != expected.Submeshes[0].IndexCount ||
				fingerprint.VerticesSHA256 != expected.VerticesSHA256 || fingerprint.NormalsSHA256 != expected.NormalsSHA256 ||
				fingerprint.TangentsSHA256 != expected.TangentsSHA256 || fingerprint.UVSHA256 != expected.UVSHA256 ||
				fingerprint.BindposesSHA256 != expected.BindposesSHA256 || fingerprint.IndicesSHA256 != expected.Submeshes[0].IndicesSHA256 {
				t.Fatalf("independent Mesh fingerprint differs from Editor: %+v", fingerprint)
			}
		}
		encoded, err := json.MarshalIndent(struct {
			Extraction ArtifactExtraction     `json:"extraction"`
			Inspection ArtifactTextInspection `json:"inspection"`
			Mesh       FixtureMeshFingerprint `json:"mesh"`
		}{proof, inspection, fingerprint}, "", "  ")
		if err != nil {
			t.Fatal(err)
		}
		if err = os.WriteFile(filepath.Join(output, "reference-extraction.json"), encoded, 0600); err != nil {
			t.Fatal(err)
		}
		t.Logf("reference FBX artifact independently extracted and parsed: %s", proof.RawHashKey)
		return
	}
	t.Fatal("reference store contains no readable SerializedFile with Shell_Floor")
}

func TestUDSLinkedNativeResponseExtraction(t *testing.T) {
	framePath := os.Getenv("UNITY_WORKER_ARTIFACT_RESPONSE")
	directory := os.Getenv("UNITY_WORKER_ARTIFACT_REFERENCE")
	output := os.Getenv("UNITY_WORKER_ARTIFACT_OUTPUT")
	editor := os.Getenv("UNITY_WORKER_ARTIFACT_EDITOR")
	if framePath == "" || directory == "" || output == "" || editor == "" {
		t.Skip("native response paths not supplied")
	}
	frame, err := os.ReadFile(framePath)
	if err != nil {
		t.Fatal(err)
	}
	response, err := decodeImportResponse(frame)
	if err != nil {
		t.Fatal(err)
	}
	if response.ErrorCode != 0 || len(response.ArtifactBlobs) != 3 {
		t.Fatal("response failed or did not contain native metadata")
	}
	meta, err := decodeArtifactMetaInfo(response.ArtifactBlobs[1])
	if err != nil {
		t.Fatal(err)
	}
	store, err := OpenUDSArtifactStore(directory)
	if err != nil {
		t.Fatal(err)
	}
	if err = os.MkdirAll(output, 0700); err != nil {
		t.Fatal(err)
	}
	var proofs []ArtifactExtraction
	for _, file := range meta.Files {
		data, proof, err := store.Read(file.Hash[:])
		if err != nil {
			t.Fatal(err)
		}
		proof, err = store.Extract(file.Hash[:], filepath.Join(output, proof.RawHashKey+".binary"))
		if err != nil {
			t.Fatal(err)
		}
		proofs = append(proofs, proof)
		if _, err = ReadSerializedArtifactHeader(data); err == nil {
			if _, err = InspectSerializedArtifact(context.Background(), editor, proof.Output, proof.Output+".txt"); err != nil {
				t.Fatal(err)
			}
		}
		t.Logf("linked metadata name=%q type=%d hash=%s bytes=%d prefix=%x", file.Name, file.Type, proof.RawHashKey, len(data), data[:min(len(data), 64)])
	}
	encoded, err := json.MarshalIndent(proofs, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err = os.WriteFile(filepath.Join(output, "linked-extractions.json"), encoded, 0600); err != nil {
		t.Fatal(err)
	}
}

func TestFixturePCMRejectsUnsupportedBanks(t *testing.T) {
	bank := make([]byte, 96+4096*4+32)
	copy(bank, "FSB5")
	binary.LittleEndian.PutUint32(bank[4:8], 1)
	binary.LittleEndian.PutUint32(bank[8:12], 1)
	binary.LittleEndian.PutUint32(bank[12:16], 36)
	binary.LittleEndian.PutUint32(bank[20:24], 4096*4+32)
	binary.LittleEndian.PutUint32(bank[24:28], 2)
	binary.LittleEndian.PutUint64(bank[60:68], uint64(4096)<<34|uint64(1)<<5|uint64(5)<<1)
	copy(bank[96:], fixtureWAV()[44:])
	valid, err := FingerprintFixturePCM(bank)
	if err != nil {
		t.Fatal(err)
	}
	if valid.PCMFloatSHA256 != "ad4d61350a59160ef1b86a1c6cf0dfc6f496872863076d68886e61170ec32f44" {
		t.Fatalf("PCM float conversion differs from native golden: %+v", valid)
	}
	for _, mutate := range []func([]byte){
		func(value []byte) { binary.LittleEndian.PutUint32(value[24:28], 15) },
		func(value []byte) { value[60] |= 1 },
		func(value []byte) { value[60] |= 64 },
		func(value []byte) { value[len(value)-1] = 1 },
		func(value []byte) { binary.LittleEndian.PutUint32(value[20:24], 100) },
	} {
		invalid := bytes.Clone(bank)
		mutate(invalid)
		if _, err = FingerprintFixturePCM(invalid); err == nil {
			t.Fatal("accepted malformed/unsupported FSB5 bank")
		}
	}
}

func TestFixtureAudioClipRejectsWrongSettingsAndMissingResource(t *testing.T) {
	dump := `ID: 7ea5e0 (ClassID: 83)
 m_Name "Probe" (string)
 m_LoadType 0 (int)
 m_Channels 2 (int)
 m_Frequency 5622 (int)
 m_BitsPerSample 10 (int)
 m_PreloadAudioData 1 (bool)
 m_Source "VirtualArtifacts/Primary/71705265666978617564696f70726f62.resource" (string)
 m_Size 4080 (UInt64)
 m_CompressionFormat 0 (int)
`
	clip, err := ValidateFixtureAudioClipDump([]byte(dump), 16512)
	if err != nil || clip.LocalFileID != 8300000 || clip.ResourceBytes != 16512 || !clip.PreloadAudioData {
		t.Fatalf("valid native-format AudioClip rejected: %+v %v", clip, err)
	}
	for _, change := range [][2]string{
		{`m_Name "Probe"`, `m_Name ""`},
		{"m_CompressionFormat 0", "m_CompressionFormat 1"},
		{"m_PreloadAudioData 1", "m_PreloadAudioData 0"},
		{"71705265666978617564696f70726f62.resource", ".resource"},
		{"m_Channels 2", "m_Channels 1"},
		{"m_Size 4080", "m_Size 0"},
	} {
		invalid := strings.ReplaceAll(dump, change[0], change[1])
		if _, err = ValidateFixtureAudioClipDump([]byte(invalid), 16512); err == nil {
			t.Fatalf("accepted wrong AudioClip field %s", change[0])
		}
	}
	if _, err = ValidateFixtureAudioClipDump([]byte(dump), 0); err == nil {
		t.Fatal("accepted missing PCM resource")
	}
}

func TestFixtureNativeAudioClipDump(t *testing.T) {
	path := os.Getenv("UNITY_WORKER_AUDIO_CLIP_DUMP")
	if path == "" {
		t.Skip("native AudioClip dump path not supplied")
	}
	dump, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	clip, err := ValidateFixtureAudioClipDump(dump, 16512)
	if err != nil {
		t.Fatal(err)
	}
	t.Logf("native AudioClip importer settings and resource metadata validated: %+v", clip)
}

func TestUDSNativePCMReferenceExtraction(t *testing.T) {
	directory := os.Getenv("UNITY_WORKER_ARTIFACT_REFERENCE")
	output := os.Getenv("UNITY_WORKER_ARTIFACT_OUTPUT")
	oraclePath := os.Getenv("UNITY_WORKER_REFERENCE_REPORT")
	if directory == "" || output == "" || oraclePath == "" {
		t.Skip("native PCM reference paths not supplied")
	}
	store, err := OpenUDSArtifactStore(directory)
	if err != nil {
		t.Fatal(err)
	}
	var oracle struct {
		Audio FixturePCMFingerprint `json:"audio"`
	}
	encoded, err := os.ReadFile(oraclePath)
	if err != nil {
		t.Fatal(err)
	}
	if err = json.Unmarshal(encoded, &oracle); err != nil {
		t.Fatal(err)
	}
	if err = os.MkdirAll(output, 0700); err != nil {
		t.Fatal(err)
	}
	for key := range store.storage {
		data, proof, err := store.Read([]byte(key))
		if err != nil || !bytes.HasPrefix(data, []byte("FSB5")) {
			continue
		}
		pcm, err := FingerprintFixturePCM(data)
		if err != nil {
			t.Fatal(err)
		}
		if pcm.PCMFloatSHA256 != oracle.Audio.PCMFloatSHA256 || pcm.Samples != oracle.Audio.Samples || pcm.Channels != oracle.Audio.Channels || pcm.Frequency != oracle.Audio.Frequency {
			t.Fatalf("decoded native PCM differs from Editor GetData: %+v", pcm)
		}
		proof, err = store.Extract([]byte(key), filepath.Join(output, proof.RawHashKey+".fsb"))
		if err != nil {
			t.Fatal(err)
		}
		encoded, err = json.MarshalIndent(struct {
			Extraction ArtifactExtraction    `json:"extraction"`
			PCM        FixturePCMFingerprint `json:"pcm"`
		}{proof, pcm}, "", "  ")
		if err != nil {
			t.Fatal(err)
		}
		if err = os.WriteFile(filepath.Join(output, "pcm-reference-extraction.json"), encoded, 0600); err != nil {
			t.Fatal(err)
		}
		t.Logf("native PCM16 resource decoded independently; Editor GetData hash matches: %s", pcm.PCMFloatSHA256)
		return
	}
	t.Fatal("no FSB5 PCM resource found in native store")
}
