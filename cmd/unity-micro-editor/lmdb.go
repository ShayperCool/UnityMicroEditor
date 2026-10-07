package main

import (
	"bytes"
	"encoding/binary"
	"errors"
	"fmt"
	"os"
	"sort"
)

// This bounded reader/writer covers the Linux x64 LMDB v1 files observed in
// Unity 6000.6.3f1. It is an offline image builder, not an online transaction
// implementation. Native LMDB owns locking and mutations after launch.
const lmdbPageSize = 4096
const lmdbMaxFileBytes = 128 * 1024 * 1024

type LMDBRecord struct {
	Key   []byte
	Value []byte
}

type lmdbImage struct {
	data     []byte
	lastPage uint64
	root     uint64
}

func openLMDBImage(data []byte) (*lmdbImage, error) {
	if len(data) < 3*lmdbPageSize || len(data)%lmdbPageSize != 0 || len(data) > lmdbMaxFileBytes {
		return nil, errors.New("LMDB image size is outside probe bounds")
	}
	var chosen []byte
	var transaction uint64
	for index := 0; index < 2; index++ {
		page := data[index*lmdbPageSize : (index+1)*lmdbPageSize]
		if binary.LittleEndian.Uint64(page[:8]) != uint64(index) || binary.LittleEndian.Uint16(page[10:12]) != 8 ||
			binary.LittleEndian.Uint32(page[16:20]) != 0xbeefc0de || binary.LittleEndian.Uint32(page[20:24]) != 1 ||
			binary.LittleEndian.Uint32(page[40:44]) != lmdbPageSize {
			continue
		}
		txn := binary.LittleEndian.Uint64(page[144:152])
		if chosen == nil || txn > transaction {
			chosen, transaction = page, txn
		}
	}
	if chosen == nil {
		return nil, errors.New("no valid Linux x64 LMDB v1 metadata page")
	}
	last := binary.LittleEndian.Uint64(chosen[136:144])
	if last >= uint64(len(data)/lmdbPageSize) {
		return nil, errors.New("LMDB metadata references missing pages")
	}
	return &lmdbImage{data: data, lastPage: last, root: binary.LittleEndian.Uint64(chosen[128:136])}, nil
}

func (image *lmdbImage) records(root uint64) ([]LMDBRecord, error) {
	if root == ^uint64(0) {
		return nil, nil
	}
	var output []LMDBRecord
	seen := make(map[uint64]bool)
	var visit func(uint64, int) error
	visit = func(number uint64, depth int) error {
		if depth > 32 || number < 2 || number > image.lastPage || seen[number] {
			return errors.New("LMDB tree contains an invalid page or cycle")
		}
		seen[number] = true
		page := image.data[number*lmdbPageSize : (number+1)*lmdbPageSize]
		if binary.LittleEndian.Uint64(page[:8]) != number {
			return errors.New("LMDB page number does not match its location")
		}
		flags := binary.LittleEndian.Uint16(page[10:12])
		if flags != 1 && flags != 2 {
			return fmt.Errorf("unsupported LMDB tree page flags %#x", flags)
		}
		lower, upper := int(binary.LittleEndian.Uint16(page[12:14])), int(binary.LittleEndian.Uint16(page[14:16]))
		if lower < 16 || lower > upper || upper > lmdbPageSize || (lower-16)%2 != 0 {
			return errors.New("LMDB page bounds are invalid")
		}
		for index := 0; index < (lower-16)/2; index++ {
			offset := int(binary.LittleEndian.Uint16(page[16+2*index : 18+2*index]))
			if offset < upper || offset+8 > lmdbPageSize {
				return errors.New("LMDB node points outside page data")
			}
			node := page[offset:]
			keySize := int(binary.LittleEndian.Uint16(node[6:8]))
			if keySize > len(node)-8 {
				return errors.New("LMDB node key exceeds page")
			}
			key := node[8 : 8+keySize]
			nodeFlags := binary.LittleEndian.Uint16(node[4:6])
			if flags == 1 {
				child := uint64(binary.LittleEndian.Uint32(node[:4])) | uint64(nodeFlags)<<32
				if err := visit(child, depth+1); err != nil {
					return err
				}
				continue
			}
			if nodeFlags&4 != 0 {
				return errors.New("duplicate-value LMDB records are not supported by probe")
			}
			size := uint64(binary.LittleEndian.Uint32(node[:4]))
			var value []byte
			if nodeFlags&1 != 0 {
				if len(node)-8-keySize < 8 {
					return errors.New("LMDB overflow pointer is missing")
				}
				overflow := binary.LittleEndian.Uint64(node[8+keySize : 16+keySize])
				if overflow < 2 || overflow > image.lastPage {
					return errors.New("LMDB overflow page is invalid")
				}
				start := overflow * lmdbPageSize
				header := image.data[start : start+16]
				pages := uint64(binary.LittleEndian.Uint32(header[12:16]))
				if binary.LittleEndian.Uint16(header[10:12]) != 4 || pages == 0 ||
					overflow+pages-1 > image.lastPage || size > pages*lmdbPageSize-16 {
					return errors.New("LMDB overflow record exceeds allocated pages")
				}
				value = image.data[start+16 : start+16+size]
			} else {
				if size > uint64(len(node)-8-keySize) {
					return errors.New("LMDB leaf value exceeds page")
				}
				value = node[8+keySize : 8+keySize+int(size)]
			}
			output = append(output, LMDBRecord{Key: bytes.Clone(key), Value: bytes.Clone(value)})
		}
		return nil
	}
	if err := visit(root, 0); err != nil {
		return nil, err
	}
	return output, nil
}

// ReadLMDBTables reads named tables without opening LMDB locks or changing any
// database files. Duplicate-value tables are rejected rather than misdecoded.
func readLMDBFile(path string) (*lmdbImage, error) {
	file, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer file.Close()
	info, err := file.Stat()
	if err != nil {
		return nil, err
	}
	if !info.Mode().IsRegular() || info.Size() > lmdbMaxFileBytes {
		return nil, errors.New("LMDB input is outside probe bounds")
	}
	data := make([]byte, int(info.Size()))
	if _, err := file.ReadAt(data, 0); err != nil {
		return nil, err
	}
	image, err := openLMDBImage(data)
	if err != nil {
		return nil, err
	}
	return image, nil
}

// ReadLMDBTable selects one named table so unrelated duplicate-value tables
// do not prevent extracting storage metadata from Unity.DataStore.
func ReadLMDBTable(path, name string) ([]LMDBRecord, error) {
	image, err := readLMDBFile(path)
	if err != nil {
		return nil, err
	}
	root, err := image.records(image.root)
	if err != nil {
		return nil, err
	}
	for _, record := range root {
		if string(record.Key) != name {
			continue
		}
		if len(record.Value) != 48 {
			return nil, errors.New("LMDB named descriptor is invalid")
		}
		return image.records(binary.LittleEndian.Uint64(record.Value[40:48]))
	}
	return nil, fmt.Errorf("LMDB table %q is missing", name)
}

func stringsIndexNUL(value string) bool { return bytes.IndexByte([]byte(value), 0) >= 0 }

type lmdbTreeStats struct {
	root, branchPages, leafPages, overflowPages uint64
	depth                                       uint16
	entries                                     uint64
}

func (tree lmdbTreeStats) descriptor() []byte {
	data := make([]byte, 48)
	binary.LittleEndian.PutUint16(data[6:8], tree.depth)
	binary.LittleEndian.PutUint64(data[8:16], tree.branchPages)
	binary.LittleEndian.PutUint64(data[16:24], tree.leafPages)
	binary.LittleEndian.PutUint64(data[24:32], tree.overflowPages)
	binary.LittleEndian.PutUint64(data[32:40], tree.entries)
	binary.LittleEndian.PutUint64(data[40:48], tree.root)
	return data
}

type lmdbImageBuilder struct{ pages [][]byte }
type lmdbChild struct {
	page uint64
	key  []byte
}

func (builder *lmdbImageBuilder) appendPage(page []byte) (uint64, error) {
	if len(page) != lmdbPageSize || (len(builder.pages)+1)*lmdbPageSize > lmdbMaxFileBytes {
		return 0, errors.New("LMDB image exceeds bounded size")
	}
	number := uint64(len(builder.pages))
	binary.LittleEndian.PutUint64(page[:8], number)
	builder.pages = append(builder.pages, page)
	return number, nil
}

func (builder *lmdbImageBuilder) overflow(value []byte) (uint64, uint64, error) {
	count := (16 + len(value) + lmdbPageSize - 1) / lmdbPageSize
	if count < 1 || (len(builder.pages)+count)*lmdbPageSize > lmdbMaxFileBytes {
		return 0, 0, errors.New("LMDB overflow exceeds bounded size")
	}
	number := uint64(len(builder.pages))
	data := make([]byte, count*lmdbPageSize)
	binary.LittleEndian.PutUint64(data[:8], number)
	binary.LittleEndian.PutUint16(data[10:12], 4)
	binary.LittleEndian.PutUint32(data[12:16], uint32(count))
	copy(data[16:], value)
	for offset := 0; offset < len(data); offset += lmdbPageSize {
		builder.pages = append(builder.pages, data[offset:offset+lmdbPageSize])
	}
	return number, uint64(count), nil
}

func lmdbNodeBytes(key, value []byte, flags uint16, size uint32) []byte {
	node := make([]byte, (8+len(key)+len(value)+1)&^1)
	binary.LittleEndian.PutUint32(node[:4], size)
	binary.LittleEndian.PutUint16(node[4:6], flags)
	binary.LittleEndian.PutUint16(node[6:8], uint16(len(key)))
	copy(node[8:], key)
	copy(node[8+len(key):], value)
	return node
}

func lmdbPackNodes(nodes [][]byte, flags uint16) ([]byte, error) {
	page := make([]byte, lmdbPageSize)
	binary.LittleEndian.PutUint16(page[10:12], flags)
	lower, upper := 16, lmdbPageSize
	for _, node := range nodes {
		upper -= len(node)
		if upper < lower+2 {
			return nil, errors.New("LMDB page nodes exceed its bounds")
		}
		binary.LittleEndian.PutUint16(page[lower:lower+2], uint16(upper))
		lower += 2
		copy(page[upper:], node)
	}
	binary.LittleEndian.PutUint16(page[12:14], uint16(lower))
	binary.LittleEndian.PutUint16(page[14:16], uint16(upper))
	return page, nil
}

func (builder *lmdbImageBuilder) buildTree(input []LMDBRecord, subdatabases bool) (lmdbTreeStats, error) {
	tree := lmdbTreeStats{root: ^uint64(0), entries: uint64(len(input))}
	if len(input) == 0 {
		return tree, nil
	}
	// Sort our own slice; callers may reuse their table snapshots.
	records := append([]LMDBRecord(nil), input...)
	sort.Slice(records, func(i, j int) bool { return bytes.Compare(records[i].Key, records[j].Key) < 0 })
	var nodes [][]byte
	var children []lmdbChild
	used := 16
	var firstKey []byte
	flush := func() error {
		page, err := lmdbPackNodes(nodes, 2)
		if err != nil {
			return err
		}
		number, err := builder.appendPage(page)
		if err != nil {
			return err
		}
		children = append(children, lmdbChild{page: number, key: firstKey})
		tree.leafPages++
		nodes, firstKey, used = nil, nil, 16
		return nil
	}
	for index, record := range records {
		if len(record.Key) == 0 || len(record.Key) > 511 || len(record.Value) > lmdbMaxFileBytes ||
			(index > 0 && bytes.Equal(record.Key, records[index-1].Key)) {
			return tree, errors.New("invalid or duplicate LMDB key")
		}
		flags := uint16(0)
		value := record.Value
		if subdatabases {
			flags = 2
		}
		// Native LMDB limits normal nodes to half a page so a split can
		// always retain at least two entries. Large values use overflow.
		if (8+len(record.Key)+len(value)+1)&^1 > (lmdbPageSize-16)/2 {
			if subdatabases {
				return tree, errors.New("LMDB descriptor exceeds native node bounds")
			}
			page, count, err := builder.overflow(value)
			if err != nil {
				return tree, err
			}
			tree.overflowPages += count
			value = make([]byte, 8)
			binary.LittleEndian.PutUint64(value, page)
			flags |= 1
		}
		node := lmdbNodeBytes(record.Key, value, flags, uint32(len(record.Value)))
		if used+len(node)+2 > lmdbPageSize && len(nodes) > 0 {
			if err := flush(); err != nil {
				return tree, err
			}
		}
		if firstKey == nil {
			firstKey = bytes.Clone(record.Key)
		}
		nodes = append(nodes, node)
		used += len(node) + 2
	}
	if err := flush(); err != nil {
		return tree, err
	}
	tree.depth = 1
	for len(children) > 1 {
		var groups [][]lmdbChild
		start, size := 0, 16
		for index, child := range children {
			nodeSize := 8
			if index != start {
				nodeSize = (8 + len(child.key) + 1) &^ 1
			}
			if size+nodeSize+2 > lmdbPageSize {
				groups = append(groups, children[start:index])
				start, size, nodeSize = index, 16, 8
			}
			size += nodeSize + 2
		}
		groups = append(groups, children[start:])
		// A trailing one-child branch is avoidable: move one child from
		// its preceding sibling while preserving ordered boundaries.
		if len(groups) > 1 && len(groups[len(groups)-1]) == 1 {
			previous := groups[len(groups)-2]
			groups[len(groups)-1] = children[start-1:]
			groups[len(groups)-2] = previous[:len(previous)-1]
		}
		var next []lmdbChild
		for _, group := range groups {
			var branchNodes [][]byte
			for index, child := range group {
				key := child.key
				if index == 0 {
					key = nil // Native LMDB's leftmost branch key is empty.
				}
				branchNodes = append(branchNodes, lmdbNodeBytes(key, nil, uint16(child.page>>32), uint32(child.page)))
			}
			page, err := lmdbPackNodes(branchNodes, 1)
			if err != nil {
				return tree, err
			}
			number, err := builder.appendPage(page)
			if err != nil {
				return tree, err
			}
			next = append(next, lmdbChild{page: number, key: group[0].key})
			tree.branchPages++
		}
		children = next
		tree.depth++
	}
	tree.root = children[0].page
	return tree, nil
}

// buildProjectLMDB writes bounded multi-level trees and overflow values. The
// returned image must be published before any native process opens the file.
func buildProjectLMDB(tables map[string][]LMDBRecord) ([]byte, error) {
	if len(tables) == 0 || len(tables) > 64 {
		return nil, errors.New("LMDB table count is outside bounds")
	}
	var names []string
	for name := range tables {
		if name == "" || stringsIndexNUL(name) {
			return nil, errors.New("LMDB table name is invalid")
		}
		names = append(names, name)
	}
	sort.Strings(names)
	builder := lmdbImageBuilder{pages: [][]byte{make([]byte, lmdbPageSize), make([]byte, lmdbPageSize)}}
	var descriptors []LMDBRecord
	for _, name := range names {
		tree, err := builder.buildTree(tables[name], false)
		if err != nil {
			return nil, fmt.Errorf("LMDB table %s: %w", name, err)
		}
		descriptors = append(descriptors, LMDBRecord{Key: []byte(name), Value: tree.descriptor()})
	}
	root, err := builder.buildTree(descriptors, true)
	if err != nil {
		return nil, err
	}
	for index := 0; index < 2; index++ {
		meta := builder.pages[index]
		binary.LittleEndian.PutUint64(meta[:8], uint64(index))
		binary.LittleEndian.PutUint16(meta[10:12], 8)
		binary.LittleEndian.PutUint32(meta[16:20], 0xbeefc0de)
		binary.LittleEndian.PutUint32(meta[20:24], 1)
		binary.LittleEndian.PutUint64(meta[32:40], lmdbMaxFileBytes)
		free := (lmdbTreeStats{root: ^uint64(0)}).descriptor()
		binary.LittleEndian.PutUint32(free[:4], lmdbPageSize)
		binary.LittleEndian.PutUint16(free[4:6], 8)
		copy(meta[40:88], free)
		copy(meta[88:136], root.descriptor())
		binary.LittleEndian.PutUint64(meta[136:144], uint64(len(builder.pages)-1))
		binary.LittleEndian.PutUint64(meta[144:152], uint64(1-index))
	}
	return bytes.Join(builder.pages, nil), nil
}
