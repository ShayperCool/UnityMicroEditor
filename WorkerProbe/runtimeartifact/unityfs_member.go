package runtimeartifact

import (
	"bytes"
	"encoding/binary"
	"errors"
	"fmt"
)

// VerifyUnityFSMember proves that an extracted SerializedFile is the named
// member of the pinned archive. No scene or engine object is instantiated.
// This bounded profile supports uncompressed and LZ4 UnityFS version 7/8.
func VerifyUnityFSMember(archive []byte, name string, member []byte) error {
	const limit = 128 << 20
	if len(archive) > limit || len(member) > limit || name == "" {
		return errors.New("UnityFS member verification exceeds the supported profile")
	}
	r := bundleReader{data: archive}
	signature, err := r.string()
	if err != nil || signature != "UnityFS" {
		return errors.New("compiled Shader origin is not a UnityFS archive")
	}
	version, err := r.u32()
	if err != nil || version != 7 && version != 8 {
		return errors.New("UnityFS archive version is unsupported")
	}
	if _, err := r.string(); err != nil {
		return err
	}
	if _, err := r.string(); err != nil {
		return err
	}
	size, err := r.u64()
	if err != nil || size != uint64(len(archive)) {
		return errors.New("UnityFS archive size is inconsistent")
	}
	compressed, err := r.u32()
	if err != nil {
		return err
	}
	uncompressed, err := r.u32()
	if err != nil || uncompressed > limit {
		return errors.New("UnityFS block directory size exceeds the supported profile")
	}
	flags, err := r.u32()
	if err != nil || flags & ^uint32(0x3ff) != 0 || flags&0x40 == 0 {
		return errors.New("UnityFS archive flags are unsupported")
	}
	r.pos = (r.pos + 15) & ^15
	infoPos := r.pos
	if flags&0x80 != 0 {
		infoPos = len(archive) - int(compressed)
	}
	if infoPos < r.pos || int(compressed) > len(archive)-infoPos {
		return errors.New("UnityFS block directory bounds are invalid")
	}
	info, err := unpackUnityFSBlock(archive[infoPos:infoPos+int(compressed)], int(uncompressed), flags&0x3f)
	if err != nil {
		return err
	}
	directory := bundleReader{data: info}
	if _, err := directory.take(16); err != nil {
		return err
	}
	count, err := directory.u32()
	if err != nil || count > 4096 {
		return errors.New("UnityFS block count exceeds the supported profile")
	}
	type block struct {
		compressed, uncompressed uint32
		flags                    uint16
	}
	blocks := make([]block, 0, count)
	total := uint64(0)
	for i := uint32(0); i < count; i++ {
		u, err := directory.u32()
		if err != nil {
			return err
		}
		c, err := directory.u32()
		if err != nil {
			return err
		}
		f, err := directory.u16()
		if err != nil || f & ^uint16(0x7f) != 0 {
			return errors.New("UnityFS block flags are unsupported")
		}
		total += uint64(u)
		if total > limit {
			return errors.New("UnityFS content exceeds the supported size")
		}
		blocks = append(blocks, block{c, u, f})
	}
	nodes, err := directory.u32()
	if err != nil || nodes > 4096 {
		return errors.New("UnityFS member count exceeds the supported profile")
	}
	var selectedOffset, selectedSize uint64
	found := false
	names := map[string]bool{}
	for i := uint32(0); i < nodes; i++ {
		o, err := directory.u64()
		if err != nil {
			return err
		}
		s, err := directory.u64()
		if err != nil {
			return err
		}
		if _, err := directory.u32(); err != nil {
			return err
		}
		n, err := directory.string()
		if err != nil || n == "" || names[n] || o > total || s > total-o {
			return errors.New("UnityFS member identity/bounds are invalid")
		}
		names[n] = true
		if n == name {
			selectedOffset, selectedSize, found = o, s, true
		}
	}
	if directory.pos != len(info) || !found || selectedSize != uint64(len(member)) {
		return errors.New("UnityFS exact member was not found or differs in size")
	}
	dataPos := r.pos
	dataEnd := len(archive)
	if flags&0x80 == 0 {
		dataPos += int(compressed)
	} else {
		dataEnd = infoPos
	}
	if flags&0x200 != 0 {
		dataPos = (dataPos + 15) &^ 15
	}
	content := make([]byte, 0, int(total))
	for _, b := range blocks {
		if dataPos > dataEnd || int(b.compressed) > dataEnd-dataPos {
			return errors.New("UnityFS content block is truncated")
		}
		decoded, err := unpackUnityFSBlock(archive[dataPos:dataPos+int(b.compressed)], int(b.uncompressed), uint32(b.flags&0x3f))
		if err != nil {
			return err
		}
		content = append(content, decoded...)
		dataPos += int(b.compressed)
	}
	if dataPos != dataEnd {
		return errors.New("UnityFS archive contains unexplained content bytes")
	}
	if !bytes.Equal(content[selectedOffset:selectedOffset+selectedSize], member) {
		return errors.New("compiled Shader member bytes differ from the pinned UnityFS archive")
	}
	return nil
}

func unpackUnityFSBlock(input []byte, size int, compression uint32) ([]byte, error) {
	if size < 0 || size > 128<<20 {
		return nil, errors.New("UnityFS block exceeds supported size")
	}
	if compression == 0 {
		if len(input) != size {
			return nil, errors.New("UnityFS raw block size is inconsistent")
		}
		return input, nil
	}
	if compression != 2 && compression != 3 {
		return nil, fmt.Errorf("UnityFS compression %d is unsupported", compression)
	}
	output := make([]byte, 0, size)
	pos := 0
	length := func(base int) (int, error) {
		value := base
		if base == 15 {
			for {
				if pos == len(input) {
					return 0, errors.New("truncated UnityFS LZ4 length")
				}
				x := int(input[pos])
				pos++
				value += x
				if value > size {
					return 0, errors.New("UnityFS LZ4 length exceeds output")
				}
				if x != 255 {
					break
				}
			}
		}
		return value, nil
	}
	for pos < len(input) {
		token := input[pos]
		pos++
		literal, err := length(int(token >> 4))
		if err != nil {
			return nil, err
		}
		if literal > len(input)-pos || literal > size-len(output) {
			return nil, errors.New("UnityFS LZ4 literal bounds are invalid")
		}
		output = append(output, input[pos:pos+literal]...)
		pos += literal
		if pos == len(input) {
			break
		}
		if len(input)-pos < 2 {
			return nil, errors.New("truncated UnityFS LZ4 offset")
		}
		offset := int(binary.LittleEndian.Uint16(input[pos : pos+2]))
		pos += 2
		if offset == 0 || offset > len(output) {
			return nil, errors.New("UnityFS LZ4 offset is invalid")
		}
		match, err := length(int(token & 15))
		if err != nil {
			return nil, err
		}
		match += 4
		if match > size-len(output) {
			return nil, errors.New("UnityFS LZ4 match exceeds output")
		}
		for i := 0; i < match; i++ {
			output = append(output, output[len(output)-offset])
		}
	}
	if len(output) != size {
		return nil, errors.New("UnityFS LZ4 output size differs")
	}
	return output, nil
}

type bundleReader struct {
	data []byte
	pos  int
}

func (r *bundleReader) take(n int) ([]byte, error) {
	if n < 0 || r.pos > len(r.data) || n > len(r.data)-r.pos {
		return nil, errors.New("truncated UnityFS metadata")
	}
	b := r.data[r.pos : r.pos+n]
	r.pos += n
	return b, nil
}
func (r *bundleReader) string() (string, error) {
	if r.pos > len(r.data) {
		return "", errors.New("truncated UnityFS string")
	}
	i := bytes.IndexByte(r.data[r.pos:], 0)
	if i < 0 || i > 4096 {
		return "", errors.New("UnityFS string exceeds supported size")
	}
	b, err := r.take(i + 1)
	if err != nil {
		return "", err
	}
	return string(b[:i]), nil
}
func (r *bundleReader) u16() (uint16, error) {
	b, e := r.take(2)
	if e != nil {
		return 0, e
	}
	return binary.BigEndian.Uint16(b), nil
}
func (r *bundleReader) u32() (uint32, error) {
	b, e := r.take(4)
	if e != nil {
		return 0, e
	}
	return binary.BigEndian.Uint32(b), nil
}
func (r *bundleReader) u64() (uint64, error) {
	b, e := r.take(8)
	if e != nil {
		return 0, e
	}
	return binary.BigEndian.Uint64(b), nil
}
