package main

import (
	"bytes"
	"encoding/binary"
	"fmt"

	"github.com/zeebo/xxh3"
)

type SourceRegistryReport struct {
	Database            string            `json:"database"`
	SHA256              string            `json:"sha256"`
	UnityVersion        string            `json:"unityVersion"`
	GUIDCount           int               `json:"guidCount"`
	TableCounts         map[string]int    `json:"tableCounts"`
	SourceHashAlgorithm string            `json:"sourceHashAlgorithm"`
	FileHashes          map[string]string `json:"fileHashes"`
	GUIDPaths           map[string]string `json:"guidPaths,omitempty"`
	SourceBytes         int64             `json:"sourceBytes,omitempty"`
	SourceFileCount     int               `json:"sourceFileCount,omitempty"`
}

func nativeSourceHash(data []byte) [16]byte {
	hash := xxh3.Hash128(data)
	var output [16]byte
	binary.LittleEndian.PutUint64(output[:8], hash.Lo)
	binary.LittleEndian.PutUint64(output[8:], hash.Hi)
	return output
}

func sourceRegistryTypes() []LMDBRecord {
	types := map[string]string{
		"AssetBundleIndex": "i32", "ImportLogEntriesCount": "pair<i32,i32>",
		"MainObjectLocalIdentifierInFile": "i64", "MonoImporter.defaultReferences": "pair<str,pptr>[]",
		"MonoImporter.executionOrder": "i32", "MonoImporter.icon": "pptr", "MonoImporter.userData": "str",
		"assetBundleName": "str", "assetBundleVariant": "str", "assetPath": "str",
		"assetReqImport": "bool", "coreAssetReqImport": "bool", "importLogFile": "hash128",
		"importerErrorFile": "hash128", "importerOverride": "imp", "labels": "str[]",
		"managedPlugin": "bool", "nativePluginPath": "str", "packageName": "str", "packageVersion": "str",
		"productId": "i32", "scriptCompilationAssetPath": "str", "uploadId": "i32",
	}
	for index := 0; index < 32; index++ {
		types[fmt.Sprintf("ArtifactFilePropDef%d", index)] = "hash128"
	}
	var records []LMDBRecord
	for name, kind := range types {
		records = append(records, LMDBRecord{Key: []byte(name + "\x00"), Value: []byte(kind + "\x00")})
	}
	return records
}

func sourceRegistryGUID(text string) ([]byte, error) {
	address, err := importAddress(text, 0, [16]byte{})
	if err != nil {
		return nil, err
	}
	return bytes.Clone(address[:16]), nil
}
