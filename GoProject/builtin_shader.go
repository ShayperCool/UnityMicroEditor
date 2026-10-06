package goproject

import (
	"encoding/json"
	"errors"
	"path/filepath"
)

const BuiltinResourceGUID = "0000000000000000f000000000000000"

// RetainedBuiltinShader records an immutable engine shader snapshot separately
// from authored project imports. Its source object is verified in native engine
// resources; its GPU payload comes from an exact compiled Player archive.
type RetainedBuiltinShader struct {
	SchemaVersion                  int               `json:"schemaVersion"`
	UnityVersion                   string            `json:"unityVersion"`
	GUID                           string            `json:"guid"`
	FileID                         string            `json:"fileId"`
	ShaderName                     string            `json:"shaderName"`
	SourceGUID                     string            `json:"sourceGuid"`
	SourceFileID                   string            `json:"sourceFileId"`
	ArchiveFileID                  string            `json:"archiveFileId"`
	BundlePath                     string            `json:"bundlePath"`
	BundleSHA256                   string            `json:"bundleSha256"`
	MemberPath                     string            `json:"memberPath"`
	MemberName                     string            `json:"memberName"`
	MemberSHA256                   string            `json:"memberSha256"`
	PayloadSHA256                  string            `json:"payloadSha256"`
	TypeTreeSHA256                 string            `json:"typeTreeSha256"`
	BuiltinSourcePath              string            `json:"builtinSourcePath"`
	BuiltinSourceSHA256            string            `json:"builtinSourcePayloadSha256"`
	CompiledSourceRevisionVerified bool              `json:"compiledSourceRevisionVerified"`
	Content                        NativeContentFile `json:"content"`
}

func (value RetainedBuiltinShader) Validate(engine string) error {
	if value.SchemaVersion != 1 || value.CompiledSourceRevisionVerified || engine != "6000.6.3f1" || value.UnityVersion != engine || value.GUID != BuiltinResourceGUID || value.FileID != "108" || value.ShaderName != "Skybox/Panoramic" ||
		value.SourceGUID != "f57c2bc4f17ba48d9b9b6758dcfe812d" || value.SourceFileID != "4800000" || !validNativeID(value.ArchiveFileID) || value.MemberName == "" {
		return errors.New("unsupported retained built-in Shader identity/version")
	}
	for _, file := range []struct{ path, sha string }{{value.BundlePath, value.BundleSHA256}, {value.MemberPath, value.MemberSHA256},
		{value.BuiltinSourcePath, value.BuiltinSourceSHA256}, {value.Content.Path, value.Content.SHA256}} {
		path, sha := file.path, file.sha
		if !filepath.IsAbs(path) || filepath.Clean(path) != path || !validNativeHex(sha, 64) {
			return errors.New("retained built-in Shader requires exact native paths/hashes")
		}
	}
	if !validNativeHex(value.PayloadSHA256, 64) || !validNativeHex(value.TypeTreeSHA256, 64) || value.Content.GUID != value.GUID || len(value.Content.Objects) != 1 ||
		value.Content.Objects[0].FileID != value.FileID || value.Content.Objects[0].TypeName != "UnityEngine.Shader" || len(value.Content.Resources) != 0 || len(value.Content.Dependencies) != 0 {
		return errors.New("retained built-in Shader content must contain only its exact Shader object")
	}
	return nil
}

func (s *Session) validateRetainedBuiltinShaders() error {
	if s.baseline["builtinShaderSnapshots"] == nil {
		return nil
	}
	encoded, err := json.Marshal(s.baseline["builtinShaderSnapshots"])
	if err != nil {
		return err
	}
	var snapshots []RetainedBuiltinShader
	if err := json.Unmarshal(encoded, &snapshots); err != nil {
		return err
	}
	if len(snapshots) != 1 {
		return errors.New("unsupported retained built-in Shader snapshot count")
	}
	for _, value := range snapshots {
		if err := value.Validate(str(s.baseline["unityVersion"])); err != nil {
			return err
		}
		if err := s.verifyNativeArtifacts(value.Content); err != nil {
			return err
		}
	}
	return nil
}

// RetainBuiltinShader accepts the verified descriptor produced by the CLI.
// Hashes are checked again before publication; authored content stays untouched.
func (s *Session) RetainBuiltinShader(value RetainedBuiltinShader) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if err := value.Validate(str(s.baseline["unityVersion"])); err != nil {
		return err
	}
	if err := s.verifyNativeArtifacts(value.Content); err != nil {
		return err
	}
	var existing []NativeContentFile
	if data := s.baseline["nativeContentFiles"]; data != nil {
		encoded, err := json.Marshal(data)
		if err != nil {
			return err
		}
		if err := json.Unmarshal(encoded, &existing); err != nil {
			return err
		}
	}
	var files []NativeContentFile
	for _, file := range existing {
		if file.GUID != value.GUID {
			files = append(files, file)
			continue
		}
		if len(file.Objects) != 1 || file.Objects[0].FileID != value.FileID || file.Objects[0].TypeName != "UnityEngine.Shader" {
			return errors.New("retained built-in Shader conflicts with another built-in content descriptor")
		}
	}
	files = append(files, value.Content)
	s.baseline["nativeContentFiles"] = files
	s.baseline["builtinShaderSnapshots"] = []RetainedBuiltinShader{value}
	return nil
}
