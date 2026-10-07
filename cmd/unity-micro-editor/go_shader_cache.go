package main

import (
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"

	goproject "unitymicroeditor/internal/project"
	"unitymicroeditor/internal/runtimeartifact"
)

// Verify the archived source and deterministic projection when replaying a
// compiled Shader cache entry. The Session then checks the authored source,
// include closure, environment and physical runtime artifact independently.
func verifyCompiledShaderCache(entry goproject.NativeReplacement) error {
	p := entry.CompiledShaderSnapshot
	if p == nil {
		return nil
	}
	if p.SchemaVersion != 1 || p.CompiledSourceRevisionVerified || p.SourcePath != entry.SourcePath || p.MemberName == "" || !filepath.IsAbs(p.BundlePath) || !filepath.IsAbs(p.MemberPath) || !validSHA256(p.BundleSHA256) || !validSHA256(p.MemberSHA256) {
		return errors.New("compiled Shader cache provenance profile is invalid")
	}
	archive, err := boundedShaderCacheFile(p.BundlePath, 128<<20)
	if err != nil {
		return err
	}
	member, err := boundedShaderCacheFile(p.MemberPath, 64<<20)
	if err != nil {
		return err
	}
	if bytesSHA(archive) != p.BundleSHA256 || bytesSHA(member) != p.MemberSHA256 {
		return errors.New("compiled Shader cache archive/member digest differs")
	}
	if err := runtimeartifact.VerifyUnityFSMember(archive, p.MemberName, member); err != nil {
		return err
	}
	_, proof, err := runtimeartifact.ExtractPlayerShader(member, entry.GUID, p.ShaderName)
	if err != nil {
		return err
	}
	if proof.ArchiveFileID != p.ArchiveFileID || proof.SourceFileID != "4800000" || proof.PayloadSHA256 != p.PayloadSHA256 || proof.TypeTreeSHA256 != p.TypeTreeSHA256 || proof.OutputSHA256 != entry.Content.SHA256 {
		return errors.New("compiled Shader cache projection proof differs from the archived object")
	}
	if entry.Content.GUID != entry.GUID || len(entry.Content.Objects) != 1 || entry.Content.Objects[0].FileID != proof.SourceFileID || entry.Content.Objects[0].TypeName != "UnityEngine.Shader" || len(entry.Content.Resources) != 0 || len(entry.Content.Dependencies) != 0 {
		return errors.New("compiled Shader cache runtime identity differs from the archived object")
	}
	return nil
}

func boundedShaderCacheFile(path string, limit int64) ([]byte, error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	data, err := io.ReadAll(io.LimitReader(f, limit+1))
	if err != nil {
		return nil, err
	}
	if int64(len(data)) > limit {
		return nil, fmt.Errorf("compiled Shader cache input exceeds supported size: %s", path)
	}
	return data, nil
}
