package main

import (
	"bytes"
	"encoding/binary"
	"encoding/hex"
	"io"
	"testing"
)

type oneByteReader struct{ source io.Reader }

func (reader oneByteReader) Read(data []byte) (int, error) { return reader.source.Read(data[:1]) }

type shortWriter struct{ destination bytes.Buffer }

func (writer *shortWriter) Write(data []byte) (int, error) { return writer.destination.Write(data[:1]) }

type headerOnlyReader struct {
	header    []byte
	bodyReads int
}

func (reader *headerOnlyReader) Read(data []byte) (int, error) {
	if len(reader.header) == 0 {
		reader.bodyReads++
		return 0, io.EOF
	}
	n := copy(data, reader.header)
	reader.header = reader.header[n:]
	return n, nil
}

func TestOversizedFrameRejectedBeforeReadingBody(t *testing.T) {
	// A peer cannot make us read/allocate its announced body beyond the
	// coordinator's 16 MiB budget, even when only the header has arrived.
	for _, length := range []uint64{16*1024*1024 + 1, ^uint64(0)} {
		var header [9]byte
		header[0] = 1
		binary.LittleEndian.PutUint64(header[1:], length)
		reader := &headerOnlyReader{header: header[:]}
		if _, err := readFrame(reader); err == nil {
			t.Fatalf("accepted announced payload length %d", length)
		}
		if reader.bodyReads != 0 {
			t.Fatalf("read body for oversized announced payload length %d", length)
		}
	}
}

func TestObservedHandshakeWithFragmentedTransport(t *testing.T) {
	wire, _ := hex.DecodeString("00050000000000000041495701000100")
	frame, err := readFrame(oneByteReader{bytes.NewReader(wire)})
	if err != nil {
		t.Fatal(err)
	}
	if err := validateHandshake(frame); err != nil {
		t.Fatal(err)
	}
	writer := &shortWriter{}
	if err := writeFrame(writer, handshakeFrame()); err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(writer.destination.Bytes(), wire) {
		t.Fatalf("wrong handshake %x", writer.destination.Bytes())
	}
}

func TestObservedWorkerHelloAndPreparedFrames(t *testing.T) {
	// Captured from the installed worker after our own coordinator echoed
	// its handshake. Prepared is a separate one-byte application message.
	wire, _ := hex.DecodeString("011f0000000000000000000000040000004173736574496d706f7274576f726b657250726f62650001010000000000000017")
	reader := bytes.NewReader(wire)
	hello, err := readFrame(reader)
	if err != nil {
		t.Fatal(err)
	}
	name, err := decodeWorkerHello(hello.Payload)
	if err != nil || name != "AssetImportWorkerProbe" {
		t.Fatalf("hello %q, %v", name, err)
	}
	prepared, err := readFrame(reader)
	if err != nil || prepared.Kind != 1 || !bytes.Equal(prepared.Payload, []byte{23}) {
		t.Fatalf("prepared %+v, %v", prepared, err)
	}
}

func TestInvalidFramesAndBlobPointers(t *testing.T) {
	for _, wire := range [][]byte{
		{0, 4, 0, 0, 0, 0, 0, 0, 0},
		{1, 0, 0, 0, 0, 0, 0, 0, 0},
		{1, 1, 0, 0, 8, 0, 0, 0, 0},
		{2, 1, 0, 0, 0, 0, 0, 0, 0},
		{1, 2, 0, 0, 0, 0, 0, 0, 0, 23},
	} {
		if _, err := readFrame(bytes.NewReader(wire)); err == nil {
			t.Errorf("accepted malformed wire %x", wire)
		}
	}
	for _, pointer := range []int32{-5, 100, 4} {
		payload := make([]byte, 8)
		binary.LittleEndian.PutUint32(payload[4:], uint32(pointer))
		if _, err := decodeWorkerHello(payload); err == nil {
			t.Errorf("accepted bad pointer %d", pointer)
		}
	}
}

func TestGUIDNibbleOrderAndAddressHash(t *testing.T) {
	address, err := makeImportAddress("0123456789abcdef0123456789abcdef", 1000)
	if err != nil {
		t.Fatal(err)
	}
	expected, _ := hex.DecodeString("1032547698badcfe1032547698badcfe")
	if !bytes.Equal(address[:16], expected) {
		t.Fatalf("wrong native GUID %x", address[:16])
	}
	if binary.LittleEndian.Uint32(address[16:20]) != 1000 {
		t.Fatal("wrong importer ID")
	}
	for _, invalid := range []string{"", "0000", "zz23456789abcdef0123456789abcdef"} {
		if _, err := makeImportAddress(invalid, 1000); err == nil {
			t.Errorf("accepted GUID %q", invalid)
		}
	}
	// Published xxHash test vectors exercise the native address hash's
	// short-input and 16-byte block paths independently of the builder.
	for _, vector := range []struct {
		input string
		hash  uint32
	}{
		{"", 0x02cc5d05}, {"a", 0x550d7456}, {"abc", 0x32d153ff},
		{"message digest", 0x7c948494}, {"abcdefghijklmnopqrstuvwxyz", 0x63a14d5f},
	} {
		if actual := xxHash32([]byte(vector.input), 0); actual != vector.hash {
			t.Errorf("XXH32 %q = %08x, want %08x", vector.input, actual, vector.hash)
		}
	}
}

func TestShutdownBlobRoundTrip(t *testing.T) {
	payload, err := shutdownMessage("Coordinator finished")
	if err != nil {
		t.Fatal(err)
	}
	decoded, err := blobString(payload, 4)
	if err != nil || decoded != "Coordinator finished" {
		t.Fatalf("shutdown %q, %v", decoded, err)
	}
	if _, err := shutdownMessage("x\x00y"); err == nil {
		t.Fatal("accepted embedded NUL")
	}
}

func TestCapturedUnknownImporterLog(t *testing.T) {
	payload, _ := hex.DecodeString("03000000000000004800000096000000920000008e0000008a00000000000000ffffffff02000000000000000000000000000000000000000000000000000000ffffffff000000000100000000000000547269656420746f2073656c65637420756e6b6e6f776e20696d706f7274657220666f722069642027313030302720273030303030303030303030303030303030303030303030303030303030303030270000")
	message, err := decodeWorkerLog(payload)
	if err != nil || message != "Tried to select unknown importer for id '1000' '00000000000000000000000000000000'" {
		t.Fatalf("captured log %q, %v", message, err)
	}
}

func TestDependencyCompletionKeepsRelativePathsAndState(t *testing.T) {
	address, err := makeImportAddress("0123456789abcdef0123456789abcdef", 1041)
	if err != nil {
		t.Fatal(err)
	}
	request := make([]byte, 56)
	request[0] = scheduleImportType
	copy(request[8:52], address[:])
	decoded, err := decodeScheduleImport(request)
	if err != nil || decoded != address {
		t.Fatalf("schedule address %x, %v", decoded, err)
	}
	if _, err := decodeScheduleImport(request[:55]); err == nil {
		t.Fatal("accepted truncated dependency request")
	}
	resultID := [16]byte{1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16}
	paths := []string{"Library/Artifacts/first", "Library/Artifacts/second"}
	message, err := importCompleteMessage(address, 7, resultID, paths, true)
	if err != nil {
		t.Fatal(err)
	}
	if message[0] != importCompleteType || !bytes.Equal(message[8:52], address[:]) ||
		binary.LittleEndian.Uint32(message[56:60]) != 7 ||
		!bytes.Equal(message[64:80], resultID[:]) || message[88] != 1 {
		t.Fatal("dependency completion changed native fixed fields")
	}
	_, count, err := blobArray(message, 80, 4)
	if err != nil || count != len(paths) {
		t.Fatalf("dependency paths count %d, %v", count, err)
	}
	for i, expected := range paths {
		actual, err := blobString(message, 96+i*4)
		if err != nil || actual != expected {
			t.Fatalf("dependency path %d: %q, %v", i, actual, err)
		}
	}
	if _, err := importCompleteMessage(address, 7, resultID, []string{"bad\x00path"}, false); err == nil {
		t.Fatal("accepted embedded NUL in dependency path")
	}
}

func TestRejectMalformedImportResponseArrays(t *testing.T) {
	payload := make([]byte, 128)
	payload[0] = importResponseType
	if _, err := decodeImportResponse(payload); err != nil {
		t.Fatal(err)
	}
	// A native pointer is relative to its own field. A positive count with
	// a zero pointer, or an array beyond the received bytes, is corrupt.
	binary.LittleEndian.PutUint32(payload[84:88], 1)
	if _, err := decodeImportResponse(payload); err == nil {
		t.Fatal("accepted nonempty null path array")
	}
	binary.LittleEndian.PutUint32(payload[80:84], 1000)
	if _, err := decodeImportResponse(payload); err == nil {
		t.Fatal("accepted path array outside payload")
	}
}

func TestImportResponseDependencyStrideAndBounds(t *testing.T) {
	first, _ := makeImportAddress("0123456789abcdef0123456789abcdef", 1041)
	second, _ := makeImportAddress("fedcba9876543210fedcba9876543210", 1020)
	payload := make([]byte, 128+2*48)
	payload[0] = importResponseType
	binary.LittleEndian.PutUint32(payload[112:116], 16)
	binary.LittleEndian.PutUint32(payload[116:120], 2)
	binary.LittleEndian.PutUint32(payload[120:124], 3)
	copy(payload[128:172], first[:])
	copy(payload[176:220], second[:])
	response, err := decodeImportResponse(payload)
	if err != nil {
		t.Fatal(err)
	}
	if len(response.Dependencies) != 2 || response.Dependencies[0] != first || response.Dependencies[1] != second || response.ConsistencyFailureCount != 3 {
		t.Fatalf("wrong dependency stride or consistency count: %+v", response)
	}
	if _, err := decodeImportResponse(payload[:len(payload)-1]); err == nil {
		t.Fatal("accepted truncated aligned dependency array")
	}
}

func TestCapturedArtifactFileTableLinksContentHash(t *testing.T) {
	// ArtifactMetaInfo fixed header, file table and empty primary-file name,
	// captured from a successful standalone FBX import in Unity6000.6.3f1.
	payload, _ := hex.DecodeString("f2ea8044e695c3e1ad7a114fc05bb18f1b1d4525ba076b6a969e8b3e0c9b21151104000000000000000000000000000000000000000000001104000000000000000000000000000000000000000000000100000001000000180000000100000034000000010000006000000001000000000000001c0000005b56b146b94ebabb3511413bb5772f5e000000000000000000")
	metadata, err := decodeArtifactMetaInfo(payload)
	if err != nil {
		t.Fatal(err)
	}
	address, _ := makeImportAddress("b1d15452ab70b6a669e9b8e3c0b91251", 1041)
	if metadata.OutputID != "f2ea8044e695c3e1ad7a114fc05bb18f" || metadata.Address != address || len(metadata.Files) != 1 {
		t.Fatalf("captured metadata %+v", metadata)
	}
	file := metadata.Files[0]
	if file.Type != 0 || file.Name != "" || hex.EncodeToString(file.Hash[:]) != "5b56b146b94ebabb3511413bb5772f5e" || len(file.InlineData) != 0 {
		t.Fatalf("captured file %+v", file)
	}
	if _, err := decodeArtifactMetaInfo(payload[:143]); err == nil {
		t.Fatal("accepted truncated artifact file table")
	}
	binary.LittleEndian.PutUint32(payload[116:120], 1000)
	if _, err := decodeArtifactMetaInfo(payload); err == nil {
		t.Fatal("accepted artifact file name outside metadata")
	}
}
