package main

import (
	"bytes"
	"encoding/binary"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func workerLogFixture(kind byte, message string) []byte {
	size := 12
	if kind == workerConsoleLogType {
		size = 80
	}
	payload := make([]byte, size)
	payload[0] = kind
	binary.LittleEndian.PutUint32(payload[8:12], uint32(size-8))
	payload = append(payload, message...)
	return append(payload, 0)
}

func workerWireFixture(t *testing.T, payloads ...[]byte) *bytes.Reader {
	t.Helper()
	var wire bytes.Buffer
	for _, payload := range payloads {
		if err := writeFrame(&wire, Frame{Kind: 1, Payload: payload}); err != nil {
			t.Fatal(err)
		}
	}
	return bytes.NewReader(wire.Bytes())
}

func TestReceiveMessageDrainsBothNativeLogKinds(t *testing.T) {
	reader := workerWireFixture(t,
		workerLogFixture(workerFileLogType, "Refreshing native importer environment"),
		workerLogFixture(workerConsoleLogType, "Worker is ready"),
		[]byte{workerPreparedType},
	)
	report := &probeReport{}
	frame, err := receiveMessage(reader, t.TempDir(), report, workerPreparedType)
	if err != nil || frame.Kind != 1 || !bytes.Equal(frame.Payload, []byte{workerPreparedType}) {
		t.Fatalf("expected Prepared after interleaved logs, got %+v, %v", frame, err)
	}
	if reader.Len() != 0 || len(report.Frames) != 3 {
		t.Fatalf("did not consume/capture both logs and reply: remaining=%d frames=%d", reader.Len(), len(report.Frames))
	}
	for _, evidence := range report.Frames {
		data, err := os.ReadFile(evidence.Path)
		if err != nil || len(data) != evidence.Size || data[0] != evidence.Type {
			t.Fatalf("captured evidence is invalid: %+v, %v", evidence, err)
		}
	}
}

func TestReceiveMessageRejectsNativeFatalDiagnostics(t *testing.T) {
	for _, diagnostic := range []string{
		"InvalidOperationException: importer settings failed",
		"Assertion failed on expression: 'IsPersistent()'",
	} {
		t.Run(diagnostic, func(t *testing.T) {
			reader := workerWireFixture(t, workerLogFixture(workerConsoleLogType, diagnostic), []byte{workerPreparedType})
			report := &probeReport{}
			if _, err := receiveMessage(reader, t.TempDir(), report, workerPreparedType); err == nil || !strings.Contains(err.Error(), diagnostic) {
				t.Fatalf("fatal diagnostic was not preserved: %v", err)
			}
			if reader.Len() == 0 || len(report.Frames) != 1 {
				t.Fatal("accepted/consumed Prepared after a fatal native diagnostic")
			}
		})
	}
}

func TestWorkerVersionMustMatchSupportedInstallation(t *testing.T) {
	for _, sample := range []struct {
		name, log string
		valid     bool
	}{
		{"supported", "Unity Editor version:    6000.6.3f1 (45d8eee7de74)\n", true},
		{"mismatched", "Unity Editor version:    6000.6.4f1 (different)\n", false},
		{"missing", "Worker has connected\n", false},
	} {
		t.Run(sample.name, func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "worker.log")
			if err := os.WriteFile(path, []byte(sample.log), 0600); err != nil {
				t.Fatal(err)
			}
			if err := validateWorkerVersion(path); (err == nil) != sample.valid {
				t.Fatalf("version acceptance = %v, expected valid=%v", err, sample.valid)
			}
		})
	}
}

func TestReferenceRequiresSuccessfulMatchingImportAndContent(t *testing.T) {
	const reference = `{"success":true,"unityVersion":"6000.6.3f1","model":{"guid":"0123456789abcdef0123456789abcdef","meshes":[{"vertexCount":3,"verticesSha256":"vertices","normalsSha256":"normals","tangentsSha256":"tangents","uvSha256":"uv","bindposesSha256":"bindposes","submeshes":[{"indexCount":3,"indicesSha256":"indices"}]}]},"audio":{"guid":"71705265666978617564696f70726f62","samples":8,"channels":2,"frequency":22050,"pcmFloatSha256":"pcm"}}`
	fixture := Fixture{ModelGUID: "0123456789abcdef0123456789abcdef", AudioGUID: audioGUID}
	content := contentValidation{
		Mesh: FixtureMeshFingerprint{VertexCount: 3, IndexCount: 3, VerticesSHA256: "vertices", NormalsSHA256: "normals", TangentsSHA256: "tangents", UVSHA256: "uv", BindposesSHA256: "bindposes", IndicesSHA256: "indices"},
		PCM:  FixturePCMFingerprint{Samples: 8, Channels: 2, Frequency: 22050, PCMFloatSHA256: "pcm"},
	}
	for _, sample := range []struct {
		name, before, after string
	}{
		{"matching", "", ""},
		{"failed-oracle", `"success":true`, `"success":false`},
		{"different-version", "6000.6.3f1", "6000.6.4f1"},
		{"different-asset", "0123456789abcdef0123456789abcdef", "1123456789abcdef0123456789abcdef"},
		{"changed-mesh-channel", `"verticesSha256":"vertices"`, `"verticesSha256":"changed"`},
		{"changed-pcm", `"pcmFloatSha256":"pcm"`, `"pcmFloatSha256":"changed"`},
		{"changed-audio-shape", `"channels":2`, `"channels":1`},
	} {
		t.Run(sample.name, func(t *testing.T) {
			data := reference
			if sample.before != "" {
				data = strings.Replace(reference, sample.before, sample.after, 1)
			}
			path := filepath.Join(t.TempDir(), "reference.json")
			if err := os.WriteFile(path, []byte(data), 0600); err != nil {
				t.Fatal(err)
			}
			err := compareReference(path, fixture, content)
			if sample.name == "matching" && err != nil {
				t.Fatalf("rejected matching reference: %v", err)
			}
			if sample.name != "matching" && err == nil {
				t.Fatal("accepted failed/incompatible Editor reference")
			}
		})
	}
}
