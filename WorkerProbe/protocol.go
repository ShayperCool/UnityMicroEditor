package main

// Observed Unity 6000.6.3f1 Linux AssetImportWorker protocol. Unity does not
// expose this as a supported API. An accepted Initialize message alone does
// not establish that an import has the required database/environment inputs.

import (
	"bytes"
	"encoding/binary"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"math/bits"
	"strings"
	"unicode/utf8"
)

const (
	workerHelloType        = byte(0)
	shutdownType           = byte(1)
	workerFileLogType      = byte(2)
	workerConsoleLogType   = byte(3)
	initializeType         = byte(16)
	prepareType            = byte(17)
	importRequestType      = byte(18)
	createDefaultAssetType = byte(19)
	importResponseType     = byte(20)
	scheduleImportType     = byte(21)
	importCompleteType     = byte(22)
	workerPreparedType     = byte(23)
	maxFramePayload        = uint64(16 * 1024 * 1024)
)

type Frame struct {
	Kind    byte
	Payload []byte
}

func handshakeFrame() Frame {
	return Frame{Kind: 0, Payload: []byte{'A', 'I', 'W', 1, 0, 1, 0}}
}

func validateHandshake(frame Frame) error {
	if frame.Kind != 0 || !bytes.Equal(frame.Payload, handshakeFrame().Payload) {
		return errors.New("worker protocol name or version does not match")
	}
	return nil
}

func readFrame(reader io.Reader) (Frame, error) {
	var header [9]byte
	if _, err := io.ReadFull(reader, header[:]); err != nil {
		return Frame{}, err
	}
	length := binary.LittleEndian.Uint64(header[1:])
	switch header[0] {
	case 0:
		// The native handshake declares five bytes but writes seven. Its
		// parser reads the messaging version after the declared payload.
		if length != 5 {
			return Frame{}, errors.New("unexpected handshake length")
		}
		length = 7
	case 1:
		if length == 0 || length > maxFramePayload {
			return Frame{}, errors.New("application frame length is invalid")
		}
	default:
		return Frame{}, fmt.Errorf("unimplemented transport frame kind %d", header[0])
	}
	payload := make([]byte, int(length))
	if _, err := io.ReadFull(reader, payload); err != nil {
		return Frame{}, err
	}
	return Frame{Kind: header[0], Payload: payload}, nil
}

func writeFrame(writer io.Writer, frame Frame) error {
	length := uint64(len(frame.Payload))
	switch frame.Kind {
	case 0:
		if err := validateHandshake(frame); err != nil {
			return err
		}
		length = 5
	case 1:
		if length == 0 || length > maxFramePayload {
			return errors.New("application frame length is invalid")
		}
	default:
		return fmt.Errorf("unimplemented transport frame kind %d", frame.Kind)
	}
	var header [9]byte
	header[0] = frame.Kind
	binary.LittleEndian.PutUint64(header[1:], length)
	for _, data := range [][]byte{header[:], frame.Payload} {
		for len(data) != 0 {
			n, err := writer.Write(data)
			if err != nil {
				return err
			}
			if n <= 0 || n > len(data) {
				return io.ErrShortWrite
			}
			data = data[n:]
		}
	}
	return nil
}

func blobString(payload []byte, field int) (string, error) {
	if field < 0 || field > len(payload)-4 {
		return "", errors.New("blob string field lies outside payload")
	}
	relative := int64(int32(binary.LittleEndian.Uint32(payload[field : field+4])))
	if relative == 0 {
		return "", nil
	}
	start := int64(field) + relative
	if start < 0 || start >= int64(len(payload)) {
		return "", errors.New("blob string points outside payload")
	}
	data := payload[int(start):]
	end := bytes.IndexByte(data, 0)
	if end < 0 {
		return "", errors.New("blob string is not terminated")
	}
	if !utf8.Valid(data[:end]) {
		return "", errors.New("blob string contains invalid UTF-8")
	}
	return string(data[:end]), nil
}

func decodeWorkerHello(payload []byte) (string, error) {
	if len(payload) < 8 || payload[0] != workerHelloType {
		return "", errors.New("expected WorkerHello message")
	}
	return blobString(payload, 4)
}

func decodeWorkerLog(payload []byte) (string, error) {
	if len(payload) < 12 || (payload[0] != workerFileLogType && payload[0] != workerConsoleLogType) {
		return "", errors.New("expected worker log message")
	}
	if payload[0] == workerConsoleLogType && len(payload) < 80 {
		return "", errors.New("console log fixed part is incomplete")
	}
	return blobString(payload, 8)
}

func shutdownMessage(reason string) ([]byte, error) {
	if strings.ContainsRune(reason, 0) || !utf8.ValidString(reason) {
		return nil, errors.New("shutdown reason is invalid")
	}
	payload := make([]byte, 8, 9+len(reason))
	payload[0] = shutdownType
	binary.LittleEndian.PutUint32(payload[4:8], 4)
	payload = append(payload, reason...)
	return append(payload, 0), nil
}

func initializeMessage(buildTarget int32) []byte {
	// Initialize and Prepare share a 96-byte fixed part. Zero-length blob
	// arrays omit custom dependencies, package summaries, diagnostic switches.
	payload := make([]byte, 96)
	payload[0] = initializeType
	binary.LittleEndian.PutUint32(payload[4:8], uint32(buildTarget))
	return payload
}

func prepareMessage(buildTarget int32) []byte {
	payload := initializeMessage(buildTarget)
	payload[0] = prepareType
	return payload
}

func importAddress(guidText string, importerType int32, importerHash [16]byte) ([44]byte, error) {
	var address [44]byte
	if len(guidText) != 32 {
		return address, errors.New("asset GUID must have 32 hexadecimal characters")
	}
	guid, err := hex.DecodeString(guidText)
	if err != nil {
		return address, fmt.Errorf("asset GUID: %w", err)
	}
	// UnityGUID text stores each low nibble first. This differs from UUIDs
	// and from Hash128's text representation.
	for i, value := range guid {
		address[i] = value<<4 | value>>4
	}
	binary.LittleEndian.PutUint32(address[16:20], uint32(importerType))
	copy(address[24:40], importerHash[:])
	hash := xxHash32(address[:16], uint32(importerType))
	hash ^= binary.LittleEndian.Uint32(importerHash[:4]) + (hash << 6) + (hash >> 2) + uint32(0x9e3779b9)
	binary.LittleEndian.PutUint32(address[40:44], hash)
	return address, nil
}

func makeImportAddress(guidText string, importerType int32) ([44]byte, error) {
	return importAddress(guidText, importerType, [16]byte{})
}

func importRequestMessage(address [44]byte, flags uint32) []byte {
	payload := make([]byte, 64)
	payload[0] = importRequestType
	copy(payload[8:52], address[:])
	binary.LittleEndian.PutUint32(payload[56:60], flags)
	return payload
}

func createDefaultAssetMessage(address [44]byte) ([]byte, error) {
	if int32(binary.LittleEndian.Uint32(address[16:20])) != -1 {
		return nil, errors.New("DefaultAsset requires importer ID -1")
	}
	payload := make([]byte, 56)
	payload[0] = createDefaultAssetType
	copy(payload[8:52], address[:])
	return payload, nil
}

func decodeScheduleImport(payload []byte) ([44]byte, error) {
	var address [44]byte
	if len(payload) != 56 || payload[0] != scheduleImportType {
		return address, errors.New("expected ScheduleImport message")
	}
	copy(address[:], payload[8:52])
	return address, nil
}

func importCompleteMessage(address [44]byte, state uint32, resultID [16]byte, paths []string, flag bool) ([]byte, error) {
	// This resumes a worker blocked on a ScheduleImport dependency. It is
	// not an acknowledgement of the worker's ordinary ImportResponse.
	// The state is a native ImportResultState value supplied by the caller;
	// no conversion from ImportResponse.ErrorCode is implied.
	if uint64(len(paths)) > (maxFramePayload-96)/4 {
		return nil, errors.New("too many dependency artifact paths")
	}
	payload := make([]byte, 96+len(paths)*4)
	payload[0] = importCompleteType
	copy(payload[8:52], address[:])
	binary.LittleEndian.PutUint32(payload[56:60], state)
	copy(payload[64:80], resultID[:])
	if flag {
		payload[88] = 1
	}
	if len(paths) > 0 {
		binary.LittleEndian.PutUint32(payload[80:84], 16)
		binary.LittleEndian.PutUint32(payload[84:88], uint32(len(paths)))
	}
	for i, path := range paths {
		if strings.ContainsRune(path, 0) || !utf8.ValidString(path) {
			return nil, errors.New("dependency artifact path is invalid")
		}
		if uint64(len(payload))+uint64(len(path))+1 > maxFramePayload {
			return nil, errors.New("dependency artifact paths exceed frame limit")
		}
		field := 96 + i*4
		binary.LittleEndian.PutUint32(payload[field:field+4], uint32(len(payload)-field))
		payload = append(payload, path...)
		payload = append(payload, 0)
	}
	return payload, nil
}

type ImportResponse struct {
	Address                 [44]byte
	ResultID                string
	ErrorCode               byte
	Flag                    bool
	ErrorMessage            string
	Paths                   []string
	ArtifactBlobs           [][]byte
	Dependencies            [][44]byte
	ConsistencyFailureCount int32
}

type ArtifactFile struct {
	Type       uint32
	Name       string
	Hash       [16]byte
	InlineData []byte
}

type ArtifactMetaInfo struct {
	OutputID string
	Address  [44]byte
	Files    []ArtifactFile
}

func decodeArtifactMetaInfo(payload []byte) (ArtifactMetaInfo, error) {
	var metadata ArtifactMetaInfo
	if len(payload) < 112 {
		return metadata, errors.New("ArtifactMetaInfo fixed part is incomplete")
	}
	metadata.OutputID = hex.EncodeToString(payload[:16])
	// BlobImportAddress contains GUID + ImporterID but omits the cached
	// address hash. Reconstruct it as ImportAddressFromBlobImportAddress does.
	copy(metadata.Address[:20], payload[16:36])
	copy(metadata.Address[24:40], payload[40:56])
	hash := xxHash32(metadata.Address[:16], binary.LittleEndian.Uint32(metadata.Address[16:20]))
	hash ^= binary.LittleEndian.Uint32(metadata.Address[24:28]) + (hash << 6) + (hash >> 2) + uint32(0x9e3779b9)
	binary.LittleEndian.PutUint32(metadata.Address[40:44], hash)
	_, count, err := blobArray(payload, 88, 32)
	if err != nil {
		return metadata, err
	}
	start := 88 + int(int32(binary.LittleEndian.Uint32(payload[88:92])))
	for i := 0; i < count; i++ {
		field := start + i*32
		name, err := blobString(payload, field+4)
		if err != nil {
			return metadata, err
		}
		inline, _, err := blobArray(payload, field+24, 1)
		if err != nil {
			return metadata, err
		}
		file := ArtifactFile{Type: binary.LittleEndian.Uint32(payload[field : field+4]), Name: name, InlineData: append([]byte(nil), inline...)}
		copy(file.Hash[:], payload[field+8:field+24])
		metadata.Files = append(metadata.Files, file)
	}
	return metadata, nil
}

func blobArray(payload []byte, field, elementSize int) ([]byte, int, error) {
	if field < 0 || field > len(payload)-8 || elementSize <= 0 {
		return nil, 0, errors.New("blob array field is invalid")
	}
	relative := int64(int32(binary.LittleEndian.Uint32(payload[field : field+4])))
	count := uint64(binary.LittleEndian.Uint32(payload[field+4 : field+8]))
	if count == 0 {
		return nil, 0, nil
	}
	start := int64(field) + relative
	length := count * uint64(elementSize)
	if relative == 0 || start < 0 || uint64(start) > uint64(len(payload)) || length > uint64(len(payload))-uint64(start) {
		return nil, 0, errors.New("blob array points outside payload")
	}
	return payload[int(start):int(uint64(start)+length)], int(count), nil
}

func decodeImportResponse(payload []byte) (ImportResponse, error) {
	var response ImportResponse
	if len(payload) < 128 || payload[0] != importResponseType {
		return response, errors.New("expected complete ImportResponse fixed part")
	}
	copy(response.Address[:], payload[8:52])
	response.ResultID = hex.EncodeToString(payload[56:72])
	response.ErrorCode = payload[72]
	response.Flag = payload[73] != 0
	message, err := blobString(payload, 76)
	if err != nil {
		return response, err
	}
	response.ErrorMessage = message
	_, pathCount, err := blobArray(payload, 80, 4)
	if err != nil {
		return response, err
	}
	if pathCount > 0 {
		start := 80 + int(int32(binary.LittleEndian.Uint32(payload[80:84])))
		for i := 0; i < pathCount; i++ {
			path, err := blobString(payload, start+i*4)
			if err != nil {
				return response, err
			}
			response.Paths = append(response.Paths, path)
		}
	}
	for _, field := range []int{88, 96, 104} {
		blob, _, err := blobArray(payload, field, 1)
		if err != nil {
			return response, err
		}
		response.ArtifactBlobs = append(response.ArtifactBlobs, append([]byte(nil), blob...))
	}
	// C++ ImportAddress has 44 meaningful bytes and four bytes of trailing
	// padding to satisfy its 8-byte alignment. Blob arrays retain that stride.
	dependencies, dependencyCount, err := blobArray(payload, 112, 48)
	if err != nil {
		return response, err
	}
	response.Dependencies = make([][44]byte, dependencyCount)
	for i := range response.Dependencies {
		copy(response.Dependencies[i][:], dependencies[i*48:i*48+44])
	}
	response.ConsistencyFailureCount = int32(binary.LittleEndian.Uint32(payload[120:124]))
	return response, nil
}

func xxHash32(data []byte, seed uint32) uint32 {
	const p1 uint32 = 2654435761
	const p2 uint32 = 2246822519
	const p3 uint32 = 3266489917
	const p4 uint32 = 668265263
	const p5 uint32 = 374761393
	round := func(value, input uint32) uint32 { return bits.RotateLeft32(value+input*p2, 13) * p1 }
	length := len(data)
	var hash uint32
	if length >= 16 {
		v1, v2, v3, v4 := seed+p1+p2, seed+p2, seed, seed-p1
		for len(data) >= 16 {
			v1 = round(v1, binary.LittleEndian.Uint32(data[0:4]))
			v2 = round(v2, binary.LittleEndian.Uint32(data[4:8]))
			v3 = round(v3, binary.LittleEndian.Uint32(data[8:12]))
			v4 = round(v4, binary.LittleEndian.Uint32(data[12:16]))
			data = data[16:]
		}
		hash = bits.RotateLeft32(v1, 1) + bits.RotateLeft32(v2, 7) + bits.RotateLeft32(v3, 12) + bits.RotateLeft32(v4, 18)
	} else {
		hash = seed + p5
	}
	hash += uint32(length)
	for len(data) >= 4 {
		hash = bits.RotateLeft32(hash+binary.LittleEndian.Uint32(data[:4])*p3, 17) * p4
		data = data[4:]
	}
	for _, value := range data {
		hash = bits.RotateLeft32(hash+uint32(value)*p5, 11) * p1
	}
	hash ^= hash >> 15
	hash *= p2
	hash ^= hash >> 13
	hash *= p3
	return hash ^ hash>>16
}
