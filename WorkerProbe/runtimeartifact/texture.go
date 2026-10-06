package runtimeartifact

import (
	"bytes"
	"encoding/binary"
	"errors"
	"strings"
)

type TextureMetadata struct {
	Width             int32  `json:"width"`
	Height            int32  `json:"height"`
	Format            int32  `json:"format"`
	MipCount          int32  `json:"mipCount"`
	CompleteImageSize uint32 `json:"completeImageSize"`
}

// readTextureResources walks the imported inline tree rather than assuming
// offsets into Texture2D. Pixel and mip bytes are skipped in constant time.
func readTextureResources(data, tree []byte) ([]Resource, *TextureMetadata, error) {
	var metadata TextureMetadata
	if len(tree) < 16 {
		return nil, nil, errors.New("native Texture2D type tree is missing")
	}
	count := int(binary.LittleEndian.Uint32(tree[8:12]))
	if count <= 0 || count > 10000 || 16+count*32 > len(tree) {
		return nil, nil, errors.New("native Texture2D type tree nodes are invalid")
	}
	nodes, chars := tree[16:16+count*32], tree[16+count*32:]
	level := func(index int) byte { return nodes[index*32+2] }
	end := func(index int) int {
		next := index + 1
		for next < count && level(next) > level(index) {
			next++
		}
		return next
	}
	name := func(index int) string {
		offset := binary.LittleEndian.Uint32(nodes[index*32+8 : index*32+12])
		if offset >= uint32(len(chars)) {
			return ""
		}
		value := chars[offset:]
		last := bytes.IndexByte(value, 0)
		if last < 0 {
			return ""
		}
		return string(value[:last])
	}
	r := reader{data: data}
	var resources []Resource
	var walk func(int) error
	walk = func(index int) error {
		node := nodes[index*32 : (index+1)*32]
		next := end(index)
		if name(index) == "m_StreamData" {
			offset, err := r.take(8)
			if err != nil {
				return err
			}
			size, err := r.u32()
			if err != nil {
				return err
			}
			path, err := r.string()
			if err != nil {
				return err
			}
			start := binary.LittleEndian.Uint64(offset)
			if path != "" || size != 0 || start != 0 {
				if !strings.HasPrefix(path, "VirtualArtifacts/Primary/") || !strings.HasSuffix(path, ".resource") {
					return errors.New("native texture resource has an unsupported virtual path")
				}
				resources = append(resources, Resource{VirtualPath: path, Offset: start, Size: uint64(size)})
			}
		} else if node[3]&1 != 0 {
			length, err := r.u32()
			if err != nil || length > maximumBytes {
				return errors.New("native texture array size is invalid")
			}
			if index+2 >= next || level(index+1) != level(index)+1 || level(index+2) != level(index)+1 {
				return errors.New("native texture array element tree is invalid")
			}
			element := index + 2
			byteSize := int32(binary.LittleEndian.Uint32(nodes[element*32+12 : element*32+16]))
			align := binary.LittleEndian.Uint32(nodes[element*32+20:element*32+24])&0x4000 != 0
			if byteSize >= 0 && !align {
				bytes := uint64(length) * uint64(byteSize)
				if bytes > maximumBytes {
					return errors.New("native texture array exceeds bounds")
				}
				if _, err := r.take(int(bytes)); err != nil {
					return err
				}
			} else {
				for item := uint32(0); item < length; item++ {
					if err := walk(element); err != nil {
						return err
					}
				}
			}
		} else if size := int32(binary.LittleEndian.Uint32(node[12:16])); size >= 0 {
			if size == 4 && r.pos <= len(data)-4 {
				value := binary.LittleEndian.Uint32(data[r.pos : r.pos+4])
				switch name(index) {
				case "m_Width":
					metadata.Width = int32(value)
				case "m_Height":
					metadata.Height = int32(value)
				case "m_TextureFormat":
					metadata.Format = int32(value)
				case "m_MipCount":
					metadata.MipCount = int32(value)
				case "m_CompleteImageSize":
					metadata.CompleteImageSize = value
				}
			}
			if _, err := r.take(int(size)); err != nil {
				return err
			}
		} else if index+1 < next {
			for child := index + 1; child < next; child = end(child) {
				if err := walk(child); err != nil {
					return err
				}
			}
		} else {
			return errors.New("native texture type has an unsupported dynamic leaf")
		}
		if binary.LittleEndian.Uint32(node[20:24])&0x4000 != 0 {
			r.pos = (r.pos + 3) &^ 3
			if r.pos > len(data) {
				return errors.New("native texture alignment exceeds object bounds")
			}
		}
		return nil
	}
	if err := walk(0); err != nil {
		return nil, nil, err
	}
	if r.pos != len(data) {
		return nil, nil, errors.New("native Texture2D payload does not match its inline type tree")
	}
	if metadata.Width <= 0 || metadata.Height <= 0 || metadata.Width > 32768 || metadata.Height > 32768 || metadata.Format <= 0 || metadata.MipCount <= 0 || metadata.MipCount > 16 {
		return nil, nil, errors.New("native Texture2D dimensions, format or mip count are invalid")
	}
	return resources, &metadata, nil
}
