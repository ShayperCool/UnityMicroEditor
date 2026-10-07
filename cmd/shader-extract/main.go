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
	"regexp"
	"strings"

	goproject "unitymicroeditor/internal/project"
	"unitymicroeditor/internal/runtimeartifact"
)

type options struct {
	input, expected, guid, name, output  string
	project, source, origin, environment string
}

type bundleOrigin struct {
	MemberName string `json:"-"`
	Source     string `json:"source"`
	SHA256     string `json:"sha256"`
	Nodes      []struct {
		Name   string `json:"name"`
		Size   int64  `json:"size"`
		Path   string `json:"path"`
		SHA256 string `json:"sha256"`
	} `json:"nodes"`
}

func run(args []string, stdout io.Writer) error {
	var o options
	flags := flag.NewFlagSet("shader-extract", flag.ContinueOnError)
	flags.StringVar(&o.input, "input", "", "exact extracted Linux SerializedFile member")
	flags.StringVar(&o.expected, "input-sha256", "", "verified source member SHA256")
	flags.StringVar(&o.guid, "guid", "", "authored Shader source GUID embedded in Player object")
	flags.StringVar(&o.name, "shader-name", "", "exact Shader name, verified against the embedded source GUID")
	flags.StringVar(&o.output, "output", "", "immutable shader-only output beneath Temp")
	flags.StringVar(&o.project, "project", "", "source project; required to produce a guarded cache replacement")
	flags.StringVar(&o.source, "source", "", "canonical authored Assets/...shader path")
	flags.StringVar(&o.origin, "origin", "", "extraction origin JSON with exact UnityFS archive/member identities")
	flags.StringVar(&o.environment, "environment-sha256", "", "current native-environment fingerprint for cache replay")
	if err := flags.Parse(args); err != nil {
		return err
	}
	if flags.NArg() != 0 || !filepath.IsAbs(o.input) || !filepath.IsAbs(o.output) || !strings.Contains(filepath.Clean(o.output), string(filepath.Separator)+"Temp"+string(filepath.Separator)) || !exactHash(o.expected) {
		return errors.New("use absolute --input, --output beneath Temp, --input-sha256, --guid and --shader-name")
	}
	guarded := o.project != "" || o.source != "" || o.origin != "" || o.environment != ""
	if guarded && (!filepath.IsAbs(o.project) || !filepath.IsAbs(o.origin) || !exactHash(o.environment) || o.source == "") {
		return errors.New("guarded cache output requires --project, --source, --origin and --environment-sha256")
	}
	data, err := boundedFile(o.input, 64<<20)
	if err != nil {
		return err
	}
	if fileDigest(data) != o.expected {
		return errors.New("compiled Shader archive member SHA256 differs")
	}
	projected, proof, err := runtimeartifact.ExtractPlayerShader(data, o.guid, o.name)
	if err != nil {
		return err
	}
	var replacement *goproject.NativeReplacement
	if guarded {
		project, err := filepath.EvalSymlinks(o.project)
		if err != nil {
			return err
		}
		project, err = filepath.Abs(project)
		if err != nil {
			return err
		}
		o.project = project
		if !within(filepath.Join(project, "Temp"), o.output) {
			return errors.New("guarded Shader output must be inside this project's Temp directory")
		}
		origin, err := verifiedOrigin(o, data)
		if err != nil {
			return err
		}
		requirements, missing, err := goproject.CollectShaderSourceRequirements(project, o.source)
		if err != nil {
			return err
		}
		sourceSHA := ""
		for _, requirement := range requirements {
			if requirement.Path == o.source {
				sourceSHA = requirement.SHA256
			}
		}
		meta, err := boundedFile(filepath.Join(project, filepath.FromSlash(o.source))+".meta", 4<<20)
		if err != nil {
			return err
		}
		identities := regexp.MustCompile(`(?m)^guid: ([0-9a-f]{32})\s*$`).FindAllSubmatch(meta, -1)
		if len(identities) != 1 || string(identities[0][1]) != proof.GUID {
			return errors.New("authored Shader .meta GUID differs from embedded Player identity")
		}
		replacement = &goproject.NativeReplacement{GUID: proof.GUID, SourcePath: o.source, SourceSHA256: sourceSHA, MetaSHA256: fileDigest(meta), EnvironmentSHA256: o.environment,
			Content: goproject.NativeContentFile{GUID: proof.GUID, Path: filepath.Clean(o.output), SHA256: proof.OutputSHA256,
				Objects: []goproject.NativeContentObject{{FileID: proof.SourceFileID, TypeName: "UnityEngine.Shader"}}, Resources: []goproject.NativeContentResource{}, Dependencies: []goproject.NativeContentDependency{}},
			CompiledShaderSnapshot: &goproject.CompiledShaderSnapshot{SchemaVersion: 1, CompiledSourceRevisionVerified: false, ShaderName: proof.Name, SourcePath: o.source, ArchiveFileID: proof.ArchiveFileID,
				BundlePath: origin.Source, BundleSHA256: origin.SHA256, MemberPath: o.input, MemberName: origin.MemberName, MemberSHA256: o.expected, PayloadSHA256: proof.PayloadSHA256, TypeTreeSHA256: proof.TypeTreeSHA256,
				SourceRequirements: requirements, MissingIncludes: missing}}
	}
	if err := os.MkdirAll(filepath.Dir(o.output), 0700); err != nil {
		return err
	}
	parent, err := filepath.EvalSymlinks(filepath.Dir(o.output))
	if err != nil {
		return err
	}
	if parent != filepath.Clean(filepath.Dir(o.output)) || guarded && !within(filepath.Join(o.project, "Temp"), parent) {
		return errors.New("Shader output parent contains a symlink or escapes owned Temp")
	}
	files := map[string][]byte{o.output: projected}
	encoded, err := json.MarshalIndent(proof, "", "  ")
	if err != nil {
		return err
	}
	files[o.output+".proof.json"] = append(encoded, '\n')
	if replacement != nil {
		encoded, err = json.MarshalIndent(replacement, "", "  ")
		if err != nil {
			return err
		}
		files[o.output+".replacement.json"] = append(encoded, '\n')
	}
	for path := range files {
		if _, err := os.Lstat(path); err == nil {
			return errors.New("Shader output already exists; use a new revision path")
		} else if !errors.Is(err, os.ErrNotExist) {
			return err
		}
	}
	written := []string{}
	committed := false
	defer func() {
		if !committed {
			for _, path := range written {
				_ = os.Remove(path)
			}
		}
	}()
	for path, contents := range files {
		f, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0600)
		if err != nil {
			return err
		}
		written = append(written, path)
		_, err = f.Write(contents)
		closeErr := f.Close()
		if err != nil {
			return err
		}
		if closeErr != nil {
			return closeErr
		}
	}
	committed = true
	event := map[string]any{"event": "compiled-shader-extracted", "path": o.output, "sha256": proof.OutputSHA256, "guid": proof.GUID, "fileId": proof.SourceFileID, "proof": o.output + ".proof.json"}
	if replacement != nil {
		event["replacement"] = o.output + ".replacement.json"
		event["compiledSourceRevisionVerified"] = false
		event["sourceRequirements"] = len(replacement.CompiledShaderSnapshot.SourceRequirements)
		event["missingIncludes"] = len(replacement.CompiledShaderSnapshot.MissingIncludes)
	}
	return json.NewEncoder(stdout).Encode(event)
}

func verifiedOrigin(o options, member []byte) (bundleOrigin, error) {
	var selected bundleOrigin
	data, err := boundedFile(o.origin, 4<<20)
	if err != nil {
		return selected, err
	}
	var origins []bundleOrigin
	if err := json.Unmarshal(data, &origins); err != nil {
		return selected, err
	}
	found := ""
	for _, origin := range origins {
		for _, node := range origin.Nodes {
			if filepath.Clean(node.Path) != filepath.Clean(o.input) {
				continue
			}
			if found != "" || node.SHA256 != o.expected || node.Size != int64(len(member)) || node.Name == "" || !filepath.IsAbs(origin.Source) || !exactHash(origin.SHA256) {
				return selected, errors.New("compiled Shader extraction origin is ambiguous or inconsistent")
			}
			selected, found = origin, node.Name
		}
	}
	if found == "" {
		return selected, errors.New("compiled Shader extracted member is absent from origin manifest")
	}
	selected.MemberName = found
	archive, err := boundedFile(selected.Source, 128<<20)
	if err != nil {
		return selected, err
	}
	if fileDigest(archive) != selected.SHA256 {
		return selected, errors.New("compiled Shader original UnityFS archive SHA256 differs")
	}
	if err := runtimeartifact.VerifyUnityFSMember(archive, found, member); err != nil {
		return selected, err
	}
	return selected, nil
}

func within(root, path string) bool {
	relative, err := filepath.Rel(root, filepath.Clean(path))
	return err == nil && relative != ".." && !strings.HasPrefix(relative, ".."+string(filepath.Separator))
}
func exactHash(value string) bool {
	if len(value) != 64 {
		return false
	}
	_, err := hex.DecodeString(value)
	return err == nil && strings.ToLower(value) == value
}
func fileDigest(data []byte) string { sum := sha256.Sum256(data); return hex.EncodeToString(sum[:]) }
func boundedFile(path string, limit int64) ([]byte, error) {
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
		return nil, fmt.Errorf("input exceeds supported size: %s", path)
	}
	return data, nil
}

func main() {
	if err := run(os.Args[1:], os.Stdout); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
