// A byte-preserving, uncompressed UnityFS container for the delivery experiment.
// This does not serialize assets or create an AssetBundle object/registry.
package main

import (
	"bytes"
	"crypto/sha256"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"path/filepath"
)

const maximumArchiveBytes = 64 << 20

type archiveMember struct {
	Name   string `json:"name"`
	Flags  uint32 `json:"flags"`
	Bytes  int    `json:"bytes"`
	SHA256 string `json:"sha256"`
	data   []byte
}

func main() {
	guid := flag.String("guid", "", "native source GUID, exactly 32 hexadecimal characters")
	serialized := flag.String("serialized", "", "native SerializedFile from the worker")
	resource := flag.String("resource", "", "optional native .resource output from the same import")
	output := flag.String("output", "", "new uncompressed archive file")
	flag.Parse()
	if err := run(*guid, *serialized, *resource, *output); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func run(guid, serialized, resource, output string) error {
	decoded, err := hex.DecodeString(guid)
	if err != nil || len(decoded) != 16 || guid != hex.EncodeToString(decoded) {
		return errors.New("guid must contain exactly 32 lowercase hexadecimal characters")
	}
	if !filepath.IsAbs(output) || serialized == "" {
		return errors.New("serialized input and absolute new output path are required")
	}
	member, err := readMember(serialized, "VirtualArtifacts/Primary/"+guid, 4)
	if err != nil {
		return err
	}
	// Only the versioned binary artifact form tested with this Unity installation.
	if len(member.data) < 48 || binary.BigEndian.Uint32(member.data[8:12]) < 22 || binary.BigEndian.Uint32(member.data[8:12]) > 23 {
		return errors.New("serialized input is not a version 22/23 SerializedFile")
	}
	members := []archiveMember{member}
	if resource != "" {
		member, err := readMember(resource, "VirtualArtifacts/Primary/"+guid+".resource", 0)
		if err != nil {
			return err
		}
		members = append(members, member)
	}
	archive, err := encodeArchive(members)
	if err != nil {
		return err
	}
	file, err := os.OpenFile(output, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0600)
	if err != nil {
		return err
	}
	_, writeErr := file.Write(archive)
	closeErr := file.Close()
	if err := errors.Join(writeErr, closeErr); err != nil {
		return err
	}
	hash := sha256.Sum256(archive)
	return json.NewEncoder(os.Stdout).Encode(struct {
		Output  string          `json:"output"`
		Bytes   int             `json:"bytes"`
		SHA256  string          `json:"sha256"`
		Members []archiveMember `json:"members"`
	}{output, len(archive), hex.EncodeToString(hash[:]), members})
}

func readMember(path, name string, flags uint32) (archiveMember, error) {
	file, err := os.Open(path)
	if err != nil {
		return archiveMember{}, err
	}
	defer file.Close()
	data, err := io.ReadAll(io.LimitReader(file, maximumArchiveBytes+1))
	if err != nil {
		return archiveMember{}, err
	}
	if len(data) == 0 || len(data) > maximumArchiveBytes {
		return archiveMember{}, errors.New("native input must contain 1..64 MiB")
	}
	hash := sha256.Sum256(data)
	return archiveMember{name, flags, len(data), hex.EncodeToString(hash[:]), data}, nil
}

func encodeArchive(members []archiveMember) ([]byte, error) {
	dataBytes := 0
	for _, member := range members {
		if len(member.data) > maximumArchiveBytes-dataBytes {
			return nil, errors.New("archive payload exceeds 64 MiB")
		}
		dataBytes += len(member.data)
	}
	// UnityFS version 6 uses big-endian tables and an uncompressed combined
	// blocks/nodes info section. One block stores the exact concatenated bytes.
	var info bytes.Buffer
	info.Write(make([]byte, 16)) // Content hash is optional for this container.
	put(&info, uint32(1))
	put(&info, uint32(dataBytes))
	put(&info, uint32(dataBytes))
	put(&info, uint16(0))
	put(&info, uint32(len(members)))
	var offset uint64
	for _, member := range members {
		put(&info, offset)
		put(&info, uint64(len(member.data)))
		put(&info, member.Flags)
		info.WriteString(member.Name)
		info.WriteByte(0)
		offset += uint64(len(member.data))
	}
	var header bytes.Buffer
	header.WriteString("UnityFS\x00")
	put(&header, uint32(6))
	header.WriteString("6000.6.3f1\x00")
	header.WriteString("6000.6.3f1\x00")
	fileSize := header.Len() + 8 + 4 + 4 + 4 + info.Len() + dataBytes
	put(&header, uint64(fileSize))
	put(&header, uint32(info.Len()))
	put(&header, uint32(info.Len()))
	put(&header, uint32(0x40))
	var archive bytes.Buffer
	archive.Grow(fileSize)
	archive.Write(header.Bytes())
	archive.Write(info.Bytes())
	for _, member := range members {
		archive.Write(member.data)
	}
	return archive.Bytes(), nil
}

func put(buffer *bytes.Buffer, value any) {
	// bytes.Buffer writes cannot fail; supported values are fixed-size integers.
	if err := binary.Write(buffer, binary.BigEndian, value); err != nil {
		panic(err)
	}
}
