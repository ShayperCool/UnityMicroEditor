package main

import (
	"bytes"
	"encoding/binary"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

func TestLMDBFixtureRoundTripAndMissingTable(t *testing.T) {
	data, err := buildFixtureLMDB(map[string][]LMDBRecord{
		"empty": {}, "records": {{Key: []byte("key\x00"), Value: []byte("value\x00")}},
	})
	if err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(t.TempDir(), "database")
	if err := os.WriteFile(path, data, 0600); err != nil {
		t.Fatal(err)
	}
	records, err := ReadLMDBTable(path, "records")
	if err != nil || len(records) != 1 || !bytes.Equal(records[0].Value, []byte("value\x00")) {
		t.Fatalf("lookup mismatch %+v %v", records, err)
	}
	if _, err := ReadLMDBTable(path, "absent"); err == nil {
		t.Fatal("missing table accepted")
	}
	if binary.LittleEndian.Uint16(data[2*lmdbPageSize+10:2*lmdbPageSize+12]) != 2 {
		t.Fatal("native root must be leaf page")
	}
}

func TestProjectLMDBMultipleBranchLevelsAndOverflow(t *testing.T) {
	// The real source project has thousands of GUIDs. Wide keys force a
	// three-level tree; parent-child GUID lists exercise overflow pages.
	var records []LMDBRecord
	for index := 0; index < 20000; index++ {
		key := []byte(fmt.Sprintf("assets/folder/%06d/%s\x00", index, strings.Repeat("a", 100)))
		records = append(records, LMDBRecord{Key: key, Value: []byte(fmt.Sprintf("value-%06d", index))})
	}
	large := bytes.Repeat([]byte{0xf3, 0x0a, 0x55}, 9000)
	tables := map[string][]LMDBRecord{"records": records, "children": {{Key: []byte("large"), Value: large}}, "empty": {}}
	data, err := buildProjectLMDB(tables)
	if err != nil {
		t.Fatal(err)
	}
	filename := filepath.Join(t.TempDir(), "database")
	if err := os.WriteFile(filename, data, 0600); err != nil {
		t.Fatal(err)
	}
	read, err := ReadLMDBTables(filename)
	if err != nil {
		t.Fatal(err)
	}
	if len(read["records"]) != len(records) || !bytes.Equal(read["children"][0].Value, large) {
		t.Fatal("project tree lost source records or overflow data")
	}
	for index, expected := range records {
		if !bytes.Equal(read["records"][index].Key, expected.Key) || !bytes.Equal(read["records"][index].Value, expected.Value) {
			t.Fatalf("record %d did not survive multiple branches", index)
		}
	}
	image, err := openLMDBImage(data)
	if err != nil {
		t.Fatal(err)
	}
	root, err := image.records(image.root)
	if err != nil {
		t.Fatal(err)
	}
	for _, record := range root {
		if string(record.Key) == "records" && binary.LittleEndian.Uint16(record.Value[6:8]) < 3 {
			t.Fatal("test did not exercise multiple branch levels")
		}
	}
	// Optional independent check against the existing read-only native
	// LMDB cursor diagnostic. The production writer itself uses no Cgo.
	if inspector := os.Getenv("UNITY_WORKER_LMDB_INSPECT"); inspector != "" {
		output, err := exec.Command(inspector, filename, "records", "20001").CombinedOutput()
		if err != nil || strings.Count(string(output), "key[") != len(records) {
			t.Fatalf("native LMDB cursor failed: %v, records=%d", err, strings.Count(string(output), "key["))
		}
		output, err = exec.Command(inspector, filename, "children", "2").CombinedOutput()
		if err != nil || !strings.Contains(string(output), fmt.Sprintf("val[%d]", len(large))) {
			t.Fatalf("native LMDB overflow cursor failed: %v %s", err, output)
		}
	}
}

func TestProjectLMDBRejectsConflictingKeysAndPreservesCallerOrder(t *testing.T) {
	records := []LMDBRecord{{Key: []byte("z"), Value: nil}, {Key: []byte("a"), Value: nil}}
	if _, err := buildProjectLMDB(map[string][]LMDBRecord{"table": records}); err != nil {
		t.Fatal(err)
	}
	if string(records[0].Key) != "z" {
		t.Fatal("LMDB image construction reordered caller records")
	}
	for _, invalid := range [][]LMDBRecord{
		{{Key: []byte("x")}, {Key: []byte("x")}}, {{Key: make([]byte, 512)}}, {{Key: nil}},
	} {
		if _, err := buildProjectLMDB(map[string][]LMDBRecord{"table": invalid}); err == nil {
			t.Fatal("invalid native LMDB key accepted")
		}
	}
}

func TestLMDBRejectsDamagedPagesAndOversizedTables(t *testing.T) {
	data, err := buildFixtureLMDB(map[string][]LMDBRecord{"table": {{Key: []byte("key"), Value: []byte("value")}}})
	if err != nil {
		t.Fatal(err)
	}
	image, err := openLMDBImage(data)
	if err != nil {
		t.Fatal(err)
	}
	// A valid metadata root with a corrupt node pointer must fail safely.
	binary.LittleEndian.PutUint16(data[2*lmdbPageSize+16:2*lmdbPageSize+18], 65535)
	if _, err := image.records(image.root); err == nil {
		t.Fatal("out-of-bounds LMDB node accepted")
	}
	if _, err := buildFixtureLMDB(map[string][]LMDBRecord{"large": {{Key: []byte("key"), Value: make([]byte, 4096)}}}); err == nil {
		t.Fatal("oversized table accepted")
	}
	if _, err := buildFixtureLMDB(map[string][]LMDBRecord{"duplicate": {{Key: []byte("key"), Value: nil}, {Key: []byte("key"), Value: nil}}}); err == nil {
		t.Fatal("duplicate key accepted")
	}
}

func TestLMDBRejectsMissingPageAndUnsupportedMetadata(t *testing.T) {
	data, err := buildFixtureLMDB(map[string][]LMDBRecord{"empty": {}})
	if err != nil {
		t.Fatal(err)
	}
	for index := 0; index < 2; index++ {
		binary.LittleEndian.PutUint64(data[index*lmdbPageSize+136:index*lmdbPageSize+144], 100)
	}
	if _, err := openLMDBImage(data); err == nil {
		t.Fatal("missing last page accepted")
	}
	for index := 0; index < 2; index++ {
		binary.LittleEndian.PutUint32(data[index*lmdbPageSize+20:index*lmdbPageSize+24], 99)
	}
	if _, err := openLMDBImage(data); err == nil {
		t.Fatal("unknown metadata version accepted")
	}
}
