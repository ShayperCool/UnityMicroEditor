package main

import (
	"bytes"
	"encoding/binary"
	"errors"
	"fmt"
	"sort"
)

func fixtureAudioMeta() []byte {
	return []byte(fmt.Sprintf(`fileFormatVersion: 2
guid: %s
AudioImporter:
  externalObjects: {}
  serializedVersion: 8
  defaultSettings:
    serializedVersion: 2
    loadType: 0
    sampleRateSetting: 0
    sampleRateOverride: 22050
    compressionFormat: 0
    quality: 1
    conversionMode: 0
    preloadAudioData: 1
  platformSettingOverrides: {}
  forceToMono: 0
  normalize: 0
  loadInBackground: 0
  ambisonic: 0
  3D: 1
  userData:
  assetBundleName:
  assetBundleVariant:
`, audioGUID))
}

func fixtureWAV() []byte {
	const frames = 4096
	const channels = 2
	const rate = 22050
	const dataSize = frames * channels * 2
	var output bytes.Buffer
	output.WriteString("RIFF")
	binary.Write(&output, binary.LittleEndian, uint32(36+dataSize))
	output.WriteString("WAVEfmt ")
	binary.Write(&output, binary.LittleEndian, uint32(16))
	binary.Write(&output, binary.LittleEndian, uint16(1))
	binary.Write(&output, binary.LittleEndian, uint16(channels))
	binary.Write(&output, binary.LittleEndian, uint32(rate))
	binary.Write(&output, binary.LittleEndian, uint32(rate*channels*2))
	binary.Write(&output, binary.LittleEndian, uint16(channels*2))
	binary.Write(&output, binary.LittleEndian, uint16(16))
	output.WriteString("data")
	binary.Write(&output, binary.LittleEndian, uint32(dataSize))
	for index := 0; index < frames; index++ {
		left := int16((index*173+7919)%49152 - 24576)
		right := int16((index*307+3571)%40960 - 20480)
		binary.Write(&output, binary.LittleEndian, left)
		binary.Write(&output, binary.LittleEndian, right)
	}
	return output.Bytes()
}

func buildFixtureLMDB(tables map[string][]LMDBRecord) ([]byte, error) {
	if len(tables) == 0 || len(tables) > 64 {
		return nil, errors.New("LMDB fixture table count is outside bounds")
	}
	names := make([]string, 0, len(tables))
	for name := range tables {
		if name == "" || stringsIndexNUL(name) {
			return nil, errors.New("LMDB table name is invalid")
		}
		names = append(names, name)
	}
	sort.Strings(names)
	pages := [][]byte{make([]byte, lmdbPageSize), make([]byte, lmdbPageSize), nil}
	var rootRecords []LMDBRecord
	for _, name := range names {
		number := uint64(len(pages))
		if len(tables[name]) > 0 {
			page, err := lmdbSingleLeaf(number, tables[name], false)
			if err != nil {
				return nil, fmt.Errorf("LMDB table %s: %w", name, err)
			}
			pages = append(pages, page)
		}
		rootRecords = append(rootRecords, LMDBRecord{Key: []byte(name), Value: lmdbDescriptor(number, len(tables[name]))})
	}
	root, err := lmdbSingleLeaf(2, rootRecords, true)
	if err != nil {
		return nil, err
	}
	pages[2] = root
	for index := 0; index < 2; index++ {
		meta := pages[index]
		binary.LittleEndian.PutUint64(meta[:8], uint64(index))
		binary.LittleEndian.PutUint16(meta[10:12], 8)
		binary.LittleEndian.PutUint32(meta[16:20], 0xbeefc0de)
		binary.LittleEndian.PutUint32(meta[20:24], 1)
		binary.LittleEndian.PutUint64(meta[32:40], 64*1024*1024)
		free := lmdbDescriptor(0, 0)
		binary.LittleEndian.PutUint32(free[:4], lmdbPageSize)
		binary.LittleEndian.PutUint16(free[4:6], 8)
		copy(meta[40:88], free)
		copy(meta[88:136], lmdbDescriptor(2, len(rootRecords)))
		binary.LittleEndian.PutUint64(meta[136:144], uint64(len(pages)-1))
		binary.LittleEndian.PutUint64(meta[144:152], uint64(1-index))
	}
	return bytes.Join(pages, nil), nil
}

func lmdbDescriptor(root uint64, count int) []byte {
	data := make([]byte, 48)
	if count > 0 {
		binary.LittleEndian.PutUint16(data[6:8], 1)
		binary.LittleEndian.PutUint64(data[16:24], 1)
		binary.LittleEndian.PutUint64(data[32:40], uint64(count))
	} else {
		root = ^uint64(0)
	}
	binary.LittleEndian.PutUint64(data[40:48], root)
	return data
}

func lmdbSingleLeaf(number uint64, records []LMDBRecord, subdatabases bool) ([]byte, error) {
	page := make([]byte, lmdbPageSize)
	binary.LittleEndian.PutUint64(page[:8], number)
	binary.LittleEndian.PutUint16(page[10:12], 2)
	sort.Slice(records, func(i, j int) bool { return bytes.Compare(records[i].Key, records[j].Key) < 0 })
	lower, upper := 16, lmdbPageSize
	for index, record := range records {
		if len(record.Key) == 0 || len(record.Key) > 511 || len(record.Value) > 0xffff ||
			(index > 0 && bytes.Equal(record.Key, records[index-1].Key)) {
			return nil, errors.New("invalid or duplicate LMDB fixture key")
		}
		nodeSize := (8 + len(record.Key) + len(record.Value) + 1) &^ 1
		upper -= nodeSize
		if upper < lower+2 {
			return nil, errors.New("LMDB fixture table exceeds one leaf page")
		}
		binary.LittleEndian.PutUint16(page[lower:lower+2], uint16(upper))
		lower += 2
		node := page[upper:]
		binary.LittleEndian.PutUint32(node[:4], uint32(len(record.Value)))
		if subdatabases {
			binary.LittleEndian.PutUint16(node[4:6], 2)
		}
		binary.LittleEndian.PutUint16(node[6:8], uint16(len(record.Key)))
		copy(node[8:], record.Key)
		copy(node[8+len(record.Key):], record.Value)
	}
	binary.LittleEndian.PutUint16(page[12:14], uint16(lower))
	binary.LittleEndian.PutUint16(page[14:16], uint16(upper))
	return page, nil
}

func ReadLMDBTables(path string) (map[string][]LMDBRecord, error) {
	image, err := readLMDBFile(path)
	if err != nil {
		return nil, err
	}
	root, err := image.records(image.root)
	if err != nil {
		return nil, err
	}
	tables := make(map[string][]LMDBRecord, len(root))
	for _, record := range root {
		if len(record.Value) != 48 || bytes.IndexByte(record.Key, 0) >= 0 {
			return nil, errors.New("LMDB root must contain only named database descriptors")
		}
		entries := binary.LittleEndian.Uint64(record.Value[32:40])
		table, err := image.records(binary.LittleEndian.Uint64(record.Value[40:48]))
		if err != nil {
			return nil, fmt.Errorf("LMDB table %q: %w", record.Key, err)
		}
		if uint64(len(table)) != entries {
			return nil, errors.New("LMDB table record count mismatch")
		}
		tables[string(record.Key)] = table
	}
	return tables, nil
}

const modelSource = "Assets/Models/Floor.fbx"
const audioGUID = "71705265666978617564696f70726f62"
