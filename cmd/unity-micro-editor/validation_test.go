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
