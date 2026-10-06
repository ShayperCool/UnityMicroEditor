package main

import (
	"bytes"
	"context"
	"encoding/binary"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"math"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"time"
)

const maxArtifactBytes = 64 * 1024 * 1024

type UDSArtifactStore struct {
	directory string
	storage   map[string][]byte
	content   map[string][]byte
}

type ArtifactExtraction struct {
	RawHashKey         string `json:"rawHashKey"`
	StorageType        uint32 `json:"storageType"`
	SourcePath         string `json:"sourcePath,omitempty"`
	Offset             uint64 `json:"offset"`
	Bytes              uint64 `json:"bytes"`
	SHA256             string `json:"sha256"`
	ContentKeyVerified bool   `json:"contentKeyVerified"`
	Output             string `json:"output,omitempty"`
}

// OpenUDSArtifactStore reads a stopped, caller-owned DataStore snapshot without
// invoking Unity or taking native database locks. It decodes only the observed
// Linux x64 6000.6.3f1 StorageInfoTable/ContentDataTable layout.
func OpenUDSArtifactStore(directory string) (*UDSArtifactStore, error) {
	directory, err := filepath.Abs(directory)
	if err != nil {
		return nil, err
	}
	storage, err := ReadLMDBTable(filepath.Join(directory, "uds.db"), "StorageInfoTable")
	if err != nil {
		return nil, fmt.Errorf("UDS storage table: %w", err)
	}
	content, err := ReadLMDBTable(filepath.Join(directory, "uds.db"), "ContentDataTable")
	if err != nil {
		return nil, fmt.Errorf("UDS content table: %w", err)
	}
	return artifactStoreFromRecords(directory, storage, content)
}

func artifactStoreFromRecords(directory string, storage, content []LMDBRecord) (*UDSArtifactStore, error) {
	store := &UDSArtifactStore{directory: directory, storage: make(map[string][]byte), content: make(map[string][]byte)}
	for _, record := range storage {
		if len(record.Key) != 16 || len(record.Value) != 32 {
			return nil, errors.New("UDS storage record has unexpected size")
		}
		key := string(record.Key)
		if _, duplicate := store.storage[key]; duplicate {
			return nil, errors.New("duplicate UDS storage hash")
		}
		store.storage[key] = bytes.Clone(record.Value)
	}
	for _, record := range content {
		if len(record.Key) != 16 || len(record.Value) > maxArtifactBytes {
			return nil, errors.New("UDS content record is outside bounds")
		}
		key := string(record.Key)
		if _, duplicate := store.content[key]; duplicate {
			return nil, errors.New("duplicate UDS content hash")
		}
		store.content[key] = bytes.Clone(record.Value)
	}
	return store, nil
}

// Read uses the exact 16 raw bytes from a worker artifact hash, not a guessed
// string/endianness conversion. The native UDS memory hasher uses XXH3-128,
// stored as low/high little-endian words; verify it before returning content.
func (store *UDSArtifactStore) Read(hash []byte) ([]byte, ArtifactExtraction, error) {
	proof := ArtifactExtraction{RawHashKey: hex.EncodeToString(hash)}
	if len(hash) != 16 {
		return nil, proof, errors.New("UDS artifact hash must be 16 bytes")
	}
	info, found := store.storage[string(hash)]
	if !found {
		return nil, proof, errors.New("artifact hash has no UDS storage entry")
	}
	proof.StorageType = binary.LittleEndian.Uint32(info[:4])
	proof.Offset = binary.LittleEndian.Uint64(info[16:24])
	proof.Bytes = binary.LittleEndian.Uint64(info[24:32])
	if proof.Bytes == 0 || proof.Bytes > maxArtifactBytes {
		return nil, proof, errors.New("UDS artifact size is outside probe bounds")
	}
	var content []byte
	switch proof.StorageType {
	case 2:
		fileID := binary.LittleEndian.Uint64(info[8:16])
		proof.SourcePath = filepath.Join(store.directory, fmt.Sprintf("UDSData_%d.bin", fileID))
		file, err := os.Open(proof.SourcePath)
		if err != nil {
			return nil, proof, err
		}
		defer file.Close()
		stat, err := file.Stat()
		if err != nil {
			return nil, proof, err
		}
		if !stat.Mode().IsRegular() || proof.Offset > uint64(stat.Size()) || proof.Bytes > uint64(stat.Size())-proof.Offset {
			return nil, proof, errors.New("UDS artifact slice exceeds its data file")
		}
		content = make([]byte, int(proof.Bytes))
		if _, err = file.ReadAt(content, int64(proof.Offset)); err != nil {
			return nil, proof, err
		}
	case 3:
		value, found := store.content[string(hash)]
		if !found || proof.Offset != 0 || uint64(len(value)) != proof.Bytes {
			return nil, proof, errors.New("inline UDS artifact does not match storage size")
		}
		content = bytes.Clone(value)
	default:
		return nil, proof, fmt.Errorf("UDS storage type %d is not yet supported", proof.StorageType)
	}
	computed := nativeSourceHash(content)
	if !bytes.Equal(computed[:], hash) {
		return nil, proof, errors.New("UDS content bytes do not match their native XXH3-128 key")
	}
	proof.ContentKeyVerified = true
	proof.SHA256 = bytesSHA(content)
	return content, proof, nil
}

func (store *UDSArtifactStore) Extract(hash []byte, output string) (ArtifactExtraction, error) {
	content, proof, err := store.Read(hash)
	if err != nil {
		return proof, err
	}
	output, err = filepath.Abs(output)
	if err != nil {
		return proof, err
	}
	canonical, err := canonicalDestination(output)
	if err != nil {
		return proof, err
	}
	directory, err := filepath.EvalSymlinks(store.directory)
	if err != nil {
		return proof, err
	}
	if withinDirectory(canonical, directory) {
		return proof, errors.New("artifact extraction must not write into its input DataStore")
	}
	file, err := os.OpenFile(output, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0600)
	if err != nil {
		return proof, err
	}
	defer file.Close()
	if _, err = file.Write(content); err != nil {
		return proof, err
	}
	if err = file.Close(); err != nil {
		return proof, err
	}
	proof.Output = output
	return proof, nil
}

type SerializedArtifactHeader struct {
	Version       uint32 `json:"version"`
	MetadataBytes uint32 `json:"metadataBytes"`
	FileBytes     uint64 `json:"fileBytes"`
	DataOffset    uint64 `json:"dataOffset"`
	BigEndian     bool   `json:"bigEndian"`
}

// Header validation is a format check, not proof of Mesh/Audio compatibility.
func ReadSerializedArtifactHeader(content []byte) (SerializedArtifactHeader, error) {
	var result SerializedArtifactHeader
	if len(content) < 48 {
		return result, errors.New("SerializedFile header is truncated")
	}
	result.Version = binary.BigEndian.Uint32(content[8:12])
	if result.Version < 22 || result.Version > 23 {
		return result, errors.New("unsupported SerializedFile version for this Unity probe")
	}
	result.MetadataBytes = binary.BigEndian.Uint32(content[20:24])
	result.FileBytes = binary.BigEndian.Uint64(content[24:32])
	result.DataOffset = binary.BigEndian.Uint64(content[32:40])
	result.BigEndian = content[16] != 0
	if content[16] > 1 || result.MetadataBytes == 0 || result.FileBytes != uint64(len(content)) ||
		result.DataOffset < 48 || result.DataOffset > result.FileBytes ||
		uint64(result.MetadataBytes) > result.DataOffset-48 {
		return result, errors.New("SerializedFile header sizes are invalid")
	}
	return result, nil
}

type ArtifactTextInspection struct {
	Input      string                   `json:"input"`
	Output     string                   `json:"output"`
	Header     SerializedArtifactHeader `json:"header"`
	DumpSHA256 string                   `json:"dumpSha256"`
	DumpBytes  int                      `json:"dumpBytes"`
}

// InspectSerializedArtifact runs the installation's standalone binary2text
// reader in a bounded subprocess. It does not start Editor or a Player.
func InspectSerializedArtifact(ctx context.Context, editor, input, output string) (ArtifactTextInspection, error) {
	result := ArtifactTextInspection{Input: input, Output: output}
	file, err := os.Open(input)
	if err != nil {
		return result, err
	}
	content, err := io.ReadAll(io.LimitReader(file, maxArtifactBytes+1))
	file.Close()
	if err != nil || len(content) > maxArtifactBytes {
		return result, errors.New("SerializedFile input is outside probe bounds")
	}
	result.Header, err = ReadSerializedArtifactHeader(content)
	if err != nil {
		return result, err
	}
	if _, err = os.Lstat(output); !os.IsNotExist(err) {
		return result, errors.New("binary2text output must be new")
	}
	bounded, cancel := context.WithTimeout(ctx, 10*time.Second)
	defer cancel()
	command := exec.CommandContext(bounded, filepath.Join(filepath.Dir(editor), "Data/Tools/binary2text"), input, output, "-hexfloat", "-detailed")
	log, err := command.CombinedOutput()
	if err != nil {
		if len(log) > 4096 {
			log = log[:4096]
		}
		return result, fmt.Errorf("binary2text failed: %w: %s", err, log)
	}
	dump, err := os.ReadFile(output)
	if err != nil {
		return result, err
	}
	result.DumpSHA256, result.DumpBytes = bytesSHA(dump), len(dump)
	return result, nil
}

type FixtureMeshFingerprint struct {
	VertexCount     int    `json:"vertexCount"`
	IndexCount      int    `json:"indexCount"`
	VerticesSHA256  string `json:"verticesSha256"`
	NormalsSHA256   string `json:"normalsSha256"`
	TangentsSHA256  string `json:"tangentsSha256"`
	UVSHA256        string `json:"uvSha256"`
	BindposesSHA256 string `json:"bindposesSha256"`
	IndicesSHA256   string `json:"indicesSha256"`
}

// FingerprintFixtureMesh independently reconstructs the tested R_Floor Mesh
// channels from binary2text's raw byte dump. Unsupported stream/format/rig
// layouts fail closed. This is a fixture validator, not a general Mesh importer.
func FingerprintFixtureMesh(dump []byte) (FixtureMeshFingerprint, error) {
	var result FixtureMeshFingerprint
	text := string(dump)
	headers := regexp.MustCompile(`(?m)^ID: .*\(ClassID: 43\)`).FindAllStringIndex(text, -1)
	if len(headers) != 1 {
		return result, errors.New("fixture must contain exactly one Mesh")
	}
	mesh := text[headers[0][0]:]
	if end := strings.Index(mesh[1:], "\nID:"); end >= 0 {
		mesh = mesh[:end+1]
	}
	indexStart, vertexStart, compressedStart := strings.Index(mesh, "m_IndexBuffer"), strings.Index(mesh, "m_VertexData"), strings.Index(mesh, "m_CompressedMesh")
	if indexStart < 0 || vertexStart <= indexStart || compressedStart <= vertexStart || !regexp.MustCompile(`m_IndexFormat 0 \(int\)`).MatchString(mesh) {
		return result, errors.New("unsupported fixture Mesh index/vertex layout")
	}
	indices, err := detailedByteArray(mesh[indexStart:vertexStart])
	if err != nil || len(indices) == 0 || len(indices)%2 != 0 {
		return result, errors.New("invalid UInt16 fixture index buffer")
	}
	vertex := mesh[vertexStart:compressedStart]
	values, err := detailedByteArray(vertex)
	if err != nil {
		return result, err
	}
	count := regexp.MustCompile(`m_VertexCount ([0-9a-f]+) \(unsigned int\)`).FindStringSubmatch(vertex)
	if count == nil {
		return result, errors.New("Mesh vertex count missing")
	}
	parsed, err := strconv.ParseUint(count[1], 16, 32)
	if err != nil || parsed == 0 || parsed > 10000 {
		return result, errors.New("Mesh vertex count outside fixture bounds")
	}
	result.VertexCount = int(parsed)
	streams := regexp.MustCompile(`\bstream ([0-9a-f]+) \(UInt8\)`).FindAllStringSubmatch(vertex, -1)
	offsets := regexp.MustCompile(`\boffset ([0-9a-f]+) \(UInt8\)`).FindAllStringSubmatch(vertex, -1)
	formats := regexp.MustCompile(`\bformat ([0-9a-f]+) \(UInt8\)`).FindAllStringSubmatch(vertex, -1)
	dimensions := regexp.MustCompile(`\bdimension ([0-9a-f]+) \(UInt8\)`).FindAllStringSubmatch(vertex, -1)
	if len(streams) != 14 || len(offsets) != 14 || len(formats) != 14 || len(dimensions) != 14 {
		return result, errors.New("fixture vertex channels must use the observed 14-channel layout")
	}
	stride := 0
	channelOffset, channelDimension := make([]int, 14), make([]int, 14)
	for index := range streams {
		dimension, err := strconv.ParseUint(dimensions[index][1], 16, 8)
		if err != nil || dimension > 4 {
			return result, errors.New("invalid Mesh channel dimension")
		}
		offset, err := strconv.ParseUint(offsets[index][1], 16, 8)
		if err != nil {
			return result, err
		}
		channelOffset[index], channelDimension[index] = int(offset), int(dimension)
		if dimension == 0 {
			continue
		}
		if streams[index][1] != "0" || formats[index][1] != "0" {
			return result, errors.New("fixture Mesh requires one Float32 stream")
		}
		end := int(offset) + int(dimension)*4
		if end > stride {
			stride = end
		}
	}
	if stride == 0 || len(values) != result.VertexCount*stride {
		return result, errors.New("Mesh vertex bytes do not match channel stride/count")
	}
	channelHash := func(channel, dimension int) (string, error) {
		if channelDimension[channel] != dimension {
			return "", errors.New("fixture Mesh has unexpected channel dimensions")
		}
		var output bytes.Buffer
		binary.Write(&output, binary.LittleEndian, int32(result.VertexCount))
		for index := 0; index < result.VertexCount; index++ {
			start := index*stride + channelOffset[channel]
			output.Write(values[start : start+dimension*4])
		}
		return bytesSHA(output.Bytes()), nil
	}
	if result.VerticesSHA256, err = channelHash(0, 3); err != nil {
		return result, err
	}
	if result.NormalsSHA256, err = channelHash(1, 3); err != nil {
		return result, err
	}
	if result.TangentsSHA256, err = channelHash(2, 4); err != nil {
		return result, err
	}
	if result.UVSHA256, err = channelHash(4, 2); err != nil {
		return result, err
	}
	if !regexp.MustCompile(`(?s)m_BindPose\s+\(vector\).*?size 0 \(int\)`).MatchString(mesh[:indexStart]) {
		return result, errors.New("fixture validator only supports an empty bindpose array")
	}
	result.BindposesSHA256 = bytesSHA([]byte{0, 0, 0, 0})
	result.IndexCount = len(indices) / 2
	var indexValues bytes.Buffer
	binary.Write(&indexValues, binary.LittleEndian, int32(result.IndexCount))
	for offset := 0; offset < len(indices); offset += 2 {
		binary.Write(&indexValues, binary.LittleEndian, int32(binary.LittleEndian.Uint16(indices[offset:offset+2])))
	}
	result.IndicesSHA256 = bytesSHA(indexValues.Bytes())
	return result, nil
}

func detailedByteArray(text string) ([]byte, error) {
	lines := regexp.MustCompile(`(?m)^\s*data \(UInt8\) #([0-9a-f]+): ([0-9a-f ]+)$`).FindAllStringSubmatch(text, -1)
	var result []byte
	for _, line := range lines {
		offset, err := strconv.ParseUint(line[1], 16, 64)
		if err != nil || offset != uint64(len(result)) {
			return nil, errors.New("raw byte dump offsets are not contiguous")
		}
		for _, value := range strings.Fields(line[2]) {
			parsed, err := strconv.ParseUint(value, 16, 8)
			if err != nil {
				return nil, errors.New("raw byte dump contains an invalid byte")
			}
			result = append(result, byte(parsed))
		}
	}
	return result, nil
}

type FixturePCMFingerprint struct {
	Samples        int    `json:"samples"`
	Channels       int    `json:"channels"`
	Frequency      int    `json:"frequency"`
	BitsPerSample  int    `json:"bitsPerSample"`
	PCMFloatSHA256 string `json:"pcmFloatSha256"`
	PCM16SHA256    string `json:"pcm16Sha256"`
}

// FingerprintFixturePCM supports the observed Unity FSB5 v1 single-sample
// PCM16 bank only. It validates the native header/sample count/channel/rate and
// converts exactly the interleaved PCM samples to the Editor GetData hash form.
// Other codecs, extra sample-header chunks and streamed offsets are rejected.
func FingerprintFixturePCM(bank []byte) (FixturePCMFingerprint, error) {
	var result FixturePCMFingerprint
	if len(bank) < 68 || !bytes.Equal(bank[:4], []byte("FSB5")) || binary.LittleEndian.Uint32(bank[4:8]) != 1 ||
		binary.LittleEndian.Uint32(bank[8:12]) != 1 || binary.LittleEndian.Uint32(bank[24:28]) != 2 {
		return result, errors.New("fixture audio requires an FSB5 v1 single-sample PCM16 bank")
	}
	headerBytes := uint64(binary.LittleEndian.Uint32(bank[12:16]))
	nameBytes := uint64(binary.LittleEndian.Uint32(bank[16:20]))
	dataBytes := uint64(binary.LittleEndian.Uint32(bank[20:24]))
	dataStart := uint64(60) + headerBytes + nameBytes
	if headerBytes < 8 || dataStart > uint64(len(bank)) || dataBytes != uint64(len(bank))-dataStart {
		return result, errors.New("FSB5 header/data sizes do not match file")
	}
	sample := binary.LittleEndian.Uint64(bank[60:68])
	if sample&1 != 0 || (sample>>6)&0xfffffff != 0 || ((sample>>1)&15) != 5 {
		return result, errors.New("fixture audio requires unchunked data at offset zero and 22050 Hz")
	}
	result.Samples, result.Channels = int(sample>>34), int((sample>>5)&1)+1
	result.Frequency, result.BitsPerSample = 22050, 16
	if result.Samples != 4096 || result.Channels != 2 {
		return result, errors.New("FSB5 sample/channel count differs from fixture")
	}
	pcmBytes := result.Samples * result.Channels * 2
	if uint64(pcmBytes) > dataBytes || dataBytes-uint64(pcmBytes) > 4096 {
		return result, errors.New("FSB5 PCM data size is outside fixture bounds")
	}
	pcm := bank[dataStart : dataStart+uint64(pcmBytes)]
	for _, padding := range bank[dataStart+uint64(pcmBytes):] {
		if padding != 0 {
			return result, errors.New("FSB5 trailing PCM padding is not zero")
		}
	}
	var floats bytes.Buffer
	binary.Write(&floats, binary.LittleEndian, int32(result.Samples*result.Channels))
	for offset := 0; offset < len(pcm); offset += 2 {
		value := float32(int16(binary.LittleEndian.Uint16(pcm[offset:offset+2]))) / 32768
		binary.Write(&floats, binary.LittleEndian, math.Float32bits(value))
	}
	result.PCMFloatSHA256, result.PCM16SHA256 = bytesSHA(floats.Bytes()), bytesSHA(pcm)
	return result, nil
}

type FixtureAudioClipMetadata struct {
	LocalFileID       int64  `json:"localFileId"`
	Name              string `json:"name"`
	LoadType          int    `json:"loadType"`
	Channels          int    `json:"channels"`
	Frequency         int    `json:"frequency"`
	BitsPerSample     int    `json:"bitsPerSample"`
	PreloadAudioData  bool   `json:"preloadAudioData"`
	CompressionFormat int    `json:"compressionFormat"`
	ResourceSource    string `json:"resourceSource"`
	ResourceBytes     uint64 `json:"resourceBytes"`
}

func ValidateFixtureAudioClipDump(dump []byte, resourceBytes uint64) (FixtureAudioClipMetadata, error) {
	var result FixtureAudioClipMetadata
	text := string(dump)
	headers := regexp.MustCompile(`(?m)^ID: ([-0-9a-f]+) \(ClassID: 83\)`).FindAllStringSubmatchIndex(text, -1)
	if len(headers) != 1 {
		return result, errors.New("fixture requires exactly one AudioClip")
	}
	match := headers[0]
	clip := text[match[0]:]
	if end := strings.Index(clip[1:], "\nID:"); end >= 0 {
		clip = clip[:end+1]
	}
	// binary2text retains std::hex after printing numeric arrays. The known
	// PCM16 bit depth identifies which radix this object's fields use.
	depth := regexp.MustCompile(`m_BitsPerSample ([0-9a-f]+) \(int\)`).FindStringSubmatch(clip)
	if depth == nil {
		return result, errors.New("AudioClip bit depth missing")
	}
	radix := 10
	if depth[1] == "10" {
		radix = 16
	} else if depth[1] != "16" {
		return result, errors.New("fixture AudioClip must be PCM16")
	}
	var err error
	result.LocalFileID, err = strconv.ParseInt(text[match[2]:match[3]], radix, 64)
	if err != nil || result.LocalFileID != 8300000 {
		return result, errors.New("AudioClip local file ID differs from native reference")
	}
	stringField := func(name string) (string, error) {
		value := regexp.MustCompile(regexp.QuoteMeta(name) + ` ("[^"\n]*") \(string\)`).FindStringSubmatch(clip)
		if value == nil {
			return "", fmt.Errorf("AudioClip %s missing", name)
		}
		return strconv.Unquote(value[1])
	}
	intField := func(name string) (int, error) {
		value := regexp.MustCompile(regexp.QuoteMeta(name) + ` ([0-9a-f]+) \((?:int|bool)\)`).FindStringSubmatch(clip)
		if value == nil {
			return 0, fmt.Errorf("AudioClip %s missing", name)
		}
		parsed, err := strconv.ParseInt(value[1], radix, 32)
		return int(parsed), err
	}
	if result.Name, err = stringField("m_Name"); err != nil {
		return result, err
	}
	if result.LoadType, err = intField("m_LoadType"); err != nil {
		return result, err
	}
	if result.Channels, err = intField("m_Channels"); err != nil {
		return result, err
	}
	if result.Frequency, err = intField("m_Frequency"); err != nil {
		return result, err
	}
	if result.BitsPerSample, err = intField("m_BitsPerSample"); err != nil {
		return result, err
	}
	if result.CompressionFormat, err = intField("m_CompressionFormat"); err != nil {
		return result, err
	}
	preload, err := intField("m_PreloadAudioData")
	if err != nil {
		return result, err
	}
	result.PreloadAudioData = preload == 1
	if result.ResourceSource, err = stringField("m_Source"); err != nil {
		return result, err
	}
	size := regexp.MustCompile(`m_Size ([0-9a-f]+) \(UInt64\)`).FindStringSubmatch(clip)
	if size == nil {
		return result, errors.New("AudioClip resource size missing")
	}
	result.ResourceBytes, err = strconv.ParseUint(size[1], radix, 64)
	if err != nil {
		return result, err
	}
	if result.Name != "Probe" || result.LoadType != 0 || result.Channels != 2 || result.Frequency != 22050 || result.BitsPerSample != 16 ||
		result.CompressionFormat != 0 || !result.PreloadAudioData || result.ResourceBytes != resourceBytes ||
		!strings.HasSuffix(result.ResourceSource, "/"+audioGUID+".resource") {
		return result, errors.New("AudioClip fields/resources differ from fixture importer settings")
	}
	return result, nil
}
