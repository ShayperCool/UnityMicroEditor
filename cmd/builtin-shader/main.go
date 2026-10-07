// builtin-shader extracts a verified engine Shader from an existing compiled
// Linux archive. It never compiles content, loads scenes, or starts Unity.
package main

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"

	goproject "unitymicroeditor/internal/project"
	"unitymicroeditor/internal/runtimeartifact"
)

type options struct{ project, input, inputSHA, bundle, bundleSHA, member, builtin, output string }

func main() {
	var opts options
	flag.StringVar(&opts.project, "project", "", "source project root; output stays in its Temp")
	flag.StringVar(&opts.input, "input", "", "exact extracted native SerializedFile member")
	flag.StringVar(&opts.inputSHA, "input-sha256", "", "verified native member SHA256")
	flag.StringVar(&opts.bundle, "bundle", "", "original compiled Linux UnityFS archive")
	flag.StringVar(&opts.bundleSHA, "bundle-sha256", "", "verified original archive SHA256")
	flag.StringVar(&opts.member, "member-name", "", "exact archive member name")
	flag.StringVar(&opts.builtin, "builtin-source", "", "matching installation Data/Resources/unity_builtin_extra")
	flag.StringVar(&opts.output, "output", "", "new immutable shader-only runtime file beneath project Temp")
	flag.Parse()
	if err := extract(opts, os.Stdout); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func digest(data []byte) string { sum := sha256.Sum256(data); return hex.EncodeToString(sum[:]) }
func readBounded(path string, limit int64) ([]byte, error) {
	file, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer file.Close()
	info, err := file.Stat()
	if err != nil {
		return nil, err
	}
	if !info.Mode().IsRegular() || info.Size() > limit {
		return nil, errors.New("built-in Shader input is oversized or not a regular file")
	}
	return io.ReadAll(io.LimitReader(file, limit+1))
}

func extract(opts options, output io.Writer) error {
	for _, path := range []string{opts.project, opts.input, opts.bundle, opts.builtin, opts.output} {
		if !filepath.IsAbs(path) || filepath.Clean(path) != path {
			return errors.New("built-in Shader extraction requires canonical absolute paths")
		}
	}
	project, err := filepath.EvalSymlinks(opts.project)
	if err != nil || project != opts.project {
		return errors.New("built-in Shader project cannot contain symlink parents")
	}
	if relative, err := filepath.Rel(filepath.Join(project, "Temp"), opts.output); err != nil || relative == "." || relative == ".." || strings.HasPrefix(relative, ".."+string(filepath.Separator)) {
		return errors.New("built-in Shader output must stay beneath project Temp")
	}
	member, err := readBounded(opts.input, 64<<20)
	if err != nil {
		return err
	}
	if digest(member) != opts.inputSHA {
		return errors.New("built-in Shader member SHA256 differs")
	}
	bundle, err := readBounded(opts.bundle, 128<<20)
	if err != nil {
		return err
	}
	if digest(bundle) != opts.bundleSHA {
		return errors.New("built-in Shader archive SHA256 differs")
	}
	if err := runtimeartifact.VerifyUnityFSMember(bundle, opts.member, member); err != nil {
		return err
	}
	data, proof, err := runtimeartifact.ExtractBuiltinPlayerShader(member, 108)
	if err != nil {
		return err
	}
	engine, err := os.Open(opts.builtin)
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
	if identity.Name != proof.Name {
		return errors.New("native builtin108 name differs from compiled Shader")
	}
	snapshot := goproject.RetainedBuiltinShader{SchemaVersion: 1, UnityVersion: identity.UnityVersion, GUID: proof.BuiltinGUID, FileID: proof.BuiltinFileID,
		ShaderName: proof.Name, SourceGUID: proof.GUID, SourceFileID: proof.SourceFileID, ArchiveFileID: proof.ArchiveFileID,
		BundlePath: opts.bundle, BundleSHA256: opts.bundleSHA, MemberPath: opts.input, MemberName: opts.member, MemberSHA256: opts.inputSHA,
		PayloadSHA256: proof.PayloadSHA256, TypeTreeSHA256: proof.TypeTreeSHA256, BuiltinSourcePath: opts.builtin, BuiltinSourceSHA256: identity.PayloadSHA256,
		Content: goproject.NativeContentFile{GUID: proof.BuiltinGUID, Path: opts.output, SHA256: proof.OutputSHA256,
			Objects: []goproject.NativeContentObject{{FileID: proof.BuiltinFileID, TypeName: "UnityEngine.Shader"}}, Resources: []goproject.NativeContentResource{}, Dependencies: []goproject.NativeContentDependency{}}}
	if err := snapshot.Validate(identity.UnityVersion); err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(opts.output), 0700); err != nil {
		return err
	}
	parent, err := filepath.EvalSymlinks(filepath.Dir(opts.output))
	if err != nil || parent != filepath.Dir(opts.output) {
		return errors.New("built-in Shader output parent is symlinked")
	}
	snapshotJSON, err := json.MarshalIndent(snapshot, "", "  ")
	if err != nil {
		return err
	}
	proofJSON, err := json.MarshalIndent(struct {
		Compiled     runtimeartifact.PlayerShaderMetadata
		NativeSource runtimeartifact.BuiltinShaderIdentity
	}{proof, identity}, "", "  ")
	if err != nil {
		return err
	}
	files := map[string][]byte{opts.output: data, opts.output + ".snapshot.json": append(snapshotJSON, '\n'), opts.output + ".proof.json": append(proofJSON, '\n')}
	var written []string
	committed := false
	defer func() {
		if !committed {
			for _, path := range written {
				os.Remove(path)
			}
		}
	}()
	for path, bytes := range files {
		file, err := os.OpenFile(path, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0600)
		if err != nil {
			return err
		}
		written = append(written, path)
		_, writeErr := file.Write(bytes)
		closeErr := file.Close()
		if err := errors.Join(writeErr, closeErr); err != nil {
			return err
		}
	}
	committed = true
	return json.NewEncoder(output).Encode(map[string]any{"event": "builtin-shader-extracted", "guid": proof.BuiltinGUID, "fileId": proof.BuiltinFileID,
		"shaderName": proof.Name, "bytes": len(data), "snapshot": opts.output + ".snapshot.json", "sha256": proof.OutputSHA256})
}
