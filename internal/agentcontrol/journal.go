package agentcontrol

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
)

const requestJournalLimit = 256 * 1024

func (c *Controller) journalRoot(create bool) (*os.Root, error) {
	directory := filepath.Join(c.options.Output, ".agent-requests")
	if err := canonicalDestination(directory); err != nil {
		return nil, err
	}
	if create {
		if err := os.MkdirAll(directory, 0700); err != nil {
			return nil, err
		}
	}
	return openOwnedDirectory(c.options.Output, directory)
}

func (c *Controller) loadRequestJournal() error {
	root, err := c.journalRoot(false)
	if errors.Is(err, os.ErrNotExist) {
		return nil
	}
	if err != nil {
		return err
	}
	defer root.Close()
	dir, err := root.Open(".")
	if err != nil {
		return err
	}
	defer dir.Close()
	for {
		entries, err := dir.ReadDir(256)
		for _, entry := range entries {
			if !strings.HasSuffix(entry.Name(), ".request-record.json") {
				continue
			}
			if len(c.requests) >= 4096 {
				return errors.New("persistent runtime request journal exceeds 4096 entries")
			}
			data, readErr := readRootBounded(root, entry.Name(), requestJournalLimit)
			if readErr != nil {
				return readErr
			}
			var record requestRecord
			if json.Unmarshal(data, &record) != nil || entry.Name() != record.ID+".request-record.json" {
				return errors.New("persistent runtime request journal has an invalid record")
			}
			if err := c.validateRequestRecord(&record); err != nil {
				return err
			}
			c.requests[record.ID] = &record
		}
		if errors.Is(err, io.EOF) {
			return nil
		}
		if err != nil {
			return err
		}
	}
}

func (c *Controller) validateRequestRecord(record *requestRecord) error {
	if !validID(record.ID) || !validID(record.SessionID) || !validID(record.Revision) || record.PID < 1 ||
		!allowedOperation(record.Operation) || !ownedPath(c.options.Output, record.Directory) || len(record.Envelope) > 65536 {
		return errors.New("persistent runtime request record has an invalid identity or ownership")
	}
	if err := canonicalDestination(record.Directory); err != nil {
		return err
	}
	digest := sha256.Sum256(record.Envelope)
	if record.Hash != hex.EncodeToString(digest[:]) {
		return errors.New("persistent runtime request envelope hash differs")
	}
	var envelope struct {
		Schema    int             `json:"schemaVersion"`
		ID        string          `json:"requestId"`
		PID       int             `json:"expectedPid"`
		Revision  string          `json:"expectedRevision"`
		Operation string          `json:"operation"`
		Arguments json.RawMessage `json:"arguments"`
	}
	if json.Unmarshal(record.Envelope, &envelope) != nil || envelope.Schema != 1 || envelope.ID != record.ID ||
		envelope.PID != record.PID || envelope.Revision != record.Revision || envelope.Operation != record.Operation ||
		!bytes.Equal(envelope.Arguments, record.Arguments) {
		return errors.New("persistent runtime request envelope disagrees with its record")
	}
	return nil
}

func (c *Controller) persistRequest(record *requestRecord) error {
	if err := c.validateRequestRecord(record); err != nil {
		return err
	}
	root, err := c.journalRoot(true)
	if err != nil {
		return err
	}
	defer root.Close()
	data, err := json.Marshal(record)
	if err != nil {
		return err
	}
	if err := publishRootBoundedImmutable(root, record.ID+".request-record.json", data, requestJournalLimit); err != nil {
		return err
	}
	return syncJournal(root)
}

func syncJournal(root *os.Root) error {
	dir, err := root.Open(".")
	if err != nil {
		return err
	}
	defer dir.Close()
	return dir.Sync()
}

func (c *Controller) readJournalReceipt(record *requestRecord) ([]byte, error) {
	root, err := c.journalRoot(false)
	if err != nil {
		return nil, err
	}
	defer root.Close()
	return readRootBounded(root, record.ID+".receipt.json", 4*1024*1024)
}

func (c *Controller) persistReceipt(record *requestRecord, data []byte) error {
	root, err := c.journalRoot(true)
	if err != nil {
		return err
	}
	defer root.Close()
	if err := publishRootBoundedImmutable(root, record.ID+".receipt.json", data, 4*1024*1024); err != nil {
		return err
	}
	return syncJournal(root)
}

func decodeRequestReceipt(data []byte, record *requestRecord, operation string) (any, error) {
	var result struct {
		Schema   int    `json:"schemaVersion"`
		ID       string `json:"requestId"`
		Hash     string `json:"requestSha256"`
		PID      int    `json:"pid"`
		Success  *bool  `json:"success"`
		Revision string `json:"revision"`
		Result   any    `json:"result"`
		Error    any    `json:"error"`
	}
	if json.Unmarshal(data, &result) != nil || result.Schema != 1 || result.ID != record.ID || result.Hash != record.Hash ||
		result.PID != record.PID || result.Success == nil || !validID(result.Revision) ||
		*result.Success && operation != "status" && result.Revision != record.Revision || !*result.Success && result.Error == nil {
		return nil, fmt.Errorf("Player returned an incompatible response identity for %s", record.ID)
	}
	return map[string]any{"requestId": record.ID, "pid": result.PID, "revision": result.Revision, "success": *result.Success, "result": result.Result, "error": result.Error}, nil
}
