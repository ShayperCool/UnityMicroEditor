package main

import (
	"errors"
	"fmt"
	"io"
	"os"
	"strings"
)

func receiveMessage(reader io.Reader, output string, report *probeReport, expected byte) (Frame, error) {
	for {
		frame, err := captureFrame(reader, output, report)
		if err != nil {
			return Frame{}, err
		}
		if frame.Payload[0] == expected {
			return frame, nil
		}
		if frame.Payload[0] != 2 && frame.Payload[0] != 3 {
			return Frame{}, fmt.Errorf("unsupported worker message type %d while awaiting %d", frame.Payload[0], expected)
		}
		message, err := decodeWorkerLog(frame.Payload)
		if err != nil {
			return Frame{}, err
		}
		if strings.Contains(message, "Exception:") || strings.Contains(message, "Assertion failed") {
			return Frame{}, fmt.Errorf("native importer diagnostic: %s", message)
		}
	}
}

func validateWorkerVersion(log string) error {
	file, err := os.Open(log)
	if err != nil {
		return err
	}
	defer file.Close()
	data, err := io.ReadAll(io.LimitReader(file, 128*1024))
	if err != nil {
		return err
	}
	for _, line := range strings.Split(string(data), "\n") {
		if !strings.HasPrefix(line, "Unity Editor version:") {
			continue
		}
		fields := strings.Fields(strings.TrimPrefix(line, "Unity Editor version:"))
		if len(fields) > 0 && fields[0] == fixtureUnityVersion {
			return nil
		}
		return fmt.Errorf("worker Unity version differs from supported %s", fixtureUnityVersion)
	}
	return errors.New("worker did not report its Unity version")
}
