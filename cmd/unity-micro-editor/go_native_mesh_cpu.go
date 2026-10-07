package main

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"strings"

	goproject "unitymicroeditor/internal/project"
	"unitymicroeditor/internal/runtimeartifact"
)

// Retained CPU flags are a derived runtime preparation, not a new asset import.
// This upgrades existing verified native cache files without launching Unity.
func (importer *nativeProjectImporter) retainNativeModelMeshCPU(entry goproject.NativeReplacement) (goproject.NativeReplacement, error) {
	if !strings.EqualFold(filepath.Ext(entry.SourcePath), ".fbx") {
		return entry, nil
	}
	sourcePath, sourceSHA := entry.Content.Path, entry.Content.SHA256
	if entry.MeshCPUProjection != nil {
		p := entry.MeshCPUProjection
		if p.SchemaVersion != 1 || p.Mode != "native-model-mesh-cpu-v1" {
			return entry, errors.New("unsupported native Mesh CPU projection")
		}
		sourcePath, sourceSHA = p.SourcePath, p.SourceSHA256
	}
	data, err := readOwnedNativeMeshFile(importer.opts.Project, sourcePath)
	if err != nil || bytesSHA(data) != sourceSHA {
		return entry, fmt.Errorf("native Mesh CPU source payload changed: %s", sourcePath)
	}
	output, proof, err := runtimeartifact.RetainModelMeshCPU(data)
	if err != nil {
		return entry, fmt.Errorf("native Mesh CPU %s: %w", entry.SourcePath, err)
	}
	retained := make([]goproject.NativeMeshCPURetention, 0, len(proof.Meshes))
	for _, mesh := range proof.Meshes {
		retained = append(retained, goproject.NativeMeshCPURetention{FileID: mesh.FileID, OriginalIsReadable: mesh.OriginalIsReadable})
	}
	if entry.MeshCPUProjection != nil {
		actual, err := readOwnedNativeMeshFile(importer.opts.Project, entry.Content.Path)
		if err != nil || bytesSHA(actual) != entry.Content.SHA256 || !reflect.DeepEqual(output, actual) || !reflect.DeepEqual(retained, entry.Content.MeshCPURetention) {
			return entry, errors.New("native Mesh CPU projected payload/retention identities differ from original imported geometry")
		}
		p := entry.MeshCPUProjection
		proofBytes, err := readOwnedNativeMeshFile(importer.opts.Project, p.ProofPath)
		if err != nil || bytesSHA(proofBytes) != p.ProofSHA256 {
			return entry, errors.New("native Mesh CPU projection proof changed")
		}
		var cached runtimeartifact.MeshCPUProjection
		if json.Unmarshal(proofBytes, &cached) != nil || !reflect.DeepEqual(proof, cached) {
			return entry, errors.New("native Mesh CPU projection proof no longer matches exact input/output")
		}
		return entry, nil
	}
	if len(entry.Content.MeshCPURetention) != 0 {
		return entry, errors.New("native Mesh CPU retention has no geometry projection provenance")
	}
	if len(retained) == 0 {
		return entry, nil
	}
	key := bytesSHA([]byte("native-model-mesh-cpu-v1\x00" + entry.GUID + "\x00" + sourceSHA))
	filename := filepath.Join(importer.opts.Output, "native/mesh-cpu", key+".bin")
	if err := os.MkdirAll(filepath.Dir(filename), 0700); err != nil {
		return entry, err
	}
	if err := writeImmutableNativeMeshFile(filename, output); err != nil {
		return entry, err
	}
	proofBytes, err := json.Marshal(proof)
	if err != nil {
		return entry, err
	}
	proofPath := filename + ".projection.json"
	if err := writeImmutableNativeMeshFile(proofPath, proofBytes); err != nil {
		return entry, err
	}
	entry.Content.Path, entry.Content.SHA256, entry.Content.MeshCPURetention = filename, proof.OutputSHA256, retained
	entry.MeshCPUProjection = &goproject.NativeMeshCPUProjection{SchemaVersion: 1, Mode: proof.Mode, SourcePath: sourcePath,
		SourceSHA256: sourceSHA, ProofPath: proofPath, ProofSHA256: bytesSHA(proofBytes)}
	return entry, nil
}

func readOwnedNativeMeshFile(project, path string) ([]byte, error) {
	if !filepath.IsAbs(path) || filepath.Clean(path) != path || !goproject.IsOwnedCachePath(project, path) {
		return nil, errors.New("native Mesh CPU file must be owned beneath project Temp or Library/UnityMicroEditor")
	}
	canonical, err := filepath.EvalSymlinks(path)
	if err != nil || canonical != path {
		return nil, errors.New("native Mesh CPU file cannot contain symlinks")
	}
	info, err := os.Stat(path)
	if err != nil || !info.Mode().IsRegular() || info.Size() > 64<<20 {
		return nil, errors.New("native Mesh CPU file is not a bounded regular file")
	}
	return os.ReadFile(path)
}

func writeImmutableNativeMeshFile(path string, data []byte) error {
	file, err := os.OpenFile(path, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0600)
	if errors.Is(err, os.ErrExist) {
		info, err := os.Lstat(path)
		if err != nil || !info.Mode().IsRegular() {
			return errors.New("native Mesh CPU immutable output is symlinked or not regular")
		}
		actual, err := goFileHash(path)
		if err != nil || actual != bytesSHA(data) {
			return errors.New("native Mesh CPU immutable output has conflicting bytes")
		}
		return nil
	}
	if err != nil {
		return err
	}
	_, writeErr := file.Write(data)
	closeErr := file.Close()
	return errors.Join(writeErr, closeErr)
}
