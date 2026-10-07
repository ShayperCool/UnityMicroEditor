package main

import (
	"bytes"
	"errors"
	"fmt"
	"os"
	"path/filepath"

	goproject "unitymicroeditor/internal/project"
	"unitymicroeditor/internal/runtimeartifact"
)

func builtinShaderFile(path string, limit int64) ([]byte, error) {
	info, err := os.Stat(path)
	if err != nil {
		return nil, err
	}
	if !info.Mode().IsRegular() || info.Size() > limit {
		return nil, errors.New("built-in Shader evidence file is oversized or not regular")
	}
	return os.ReadFile(path)
}

func verifyBuiltinShaderSnapshot(snapshot goproject.RetainedBuiltinShader) error {
	if err := snapshot.Validate("6000.6.3f1"); err != nil {
		return err
	}
	member, err := builtinShaderFile(snapshot.MemberPath, 64<<20)
	if err != nil {
		return err
	}
	if bytesSHA(member) != snapshot.MemberSHA256 {
		return errors.New("compiled built-in Shader member changed")
	}
	bundle, err := builtinShaderFile(snapshot.BundlePath, 128<<20)
	if err != nil {
		return err
	}
	if bytesSHA(bundle) != snapshot.BundleSHA256 {
		return errors.New("compiled built-in Shader archive changed")
	}
	if err := runtimeartifact.VerifyUnityFSMember(bundle, snapshot.MemberName, member); err != nil {
		return err
	}
	projected, proof, err := runtimeartifact.ExtractBuiltinPlayerShader(member, 108)
	if err != nil {
		return err
	}
	if proof.GUID != snapshot.SourceGUID || proof.SourceFileID != snapshot.SourceFileID || proof.ArchiveFileID != snapshot.ArchiveFileID || proof.Name != snapshot.ShaderName ||
		proof.PayloadSHA256 != snapshot.PayloadSHA256 || proof.TypeTreeSHA256 != snapshot.TypeTreeSHA256 || proof.OutputSHA256 != snapshot.Content.SHA256 {
		return errors.New("compiled built-in Shader provenance does not match its exact native payload")
	}
	actual, err := builtinShaderFile(snapshot.Content.Path, 64<<20)
	if err != nil {
		return err
	}
	if !bytes.Equal(actual, projected) {
		return errors.New("retained built-in Shader differs from its verified compiled source")
	}
	engine, err := os.Open(snapshot.BuiltinSourcePath)
	if err != nil {
		return err
	}
	defer engine.Close()
	info, err := engine.Stat()
	if err != nil {
		return err
	}
	identity, err := runtimeartifact.InspectBuiltinShader(engine, info.Size(), 108)
	if err != nil {
		return err
	}
	if identity.UnityVersion != snapshot.UnityVersion || identity.Name != snapshot.ShaderName || identity.PayloadSHA256 != snapshot.BuiltinSourceSHA256 {
		return errors.New("native engine built-in Shader identity changed")
	}
	return nil
}

func applyGoBuiltinShaders(session *goproject.Session, opts launchOptions) error {
	if len(opts.BuiltinShaders) > 1 {
		return errors.New("only one verified built-in Shader snapshot is supported")
	}
	for _, path := range opts.BuiltinShaders {
		if !filepath.IsAbs(path) {
			return errors.New("--builtin-shader requires an absolute immutable snapshot path")
		}
		var snapshot goproject.RetainedBuiltinShader
		if err := readJSONFile(path, &snapshot); err != nil {
			return err
		}
		if err := verifyBuiltinShaderSnapshot(snapshot); err != nil {
			return fmt.Errorf("built-in Shader snapshot: %w", err)
		}
		if err := session.RetainBuiltinShader(snapshot); err != nil {
			return err
		}
	}
	return nil
}
