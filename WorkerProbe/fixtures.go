package main

import (
	"bytes"
	"crypto/sha256"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strings"
)

const modelSource = "Assets/Dungeon/ShipGen/Art/Custom/Shell/R_Floor.fbx"
const modelAsset = "Assets/Probe/R_Floor.fbx"
const audioAsset = "Assets/Probe/Probe.wav"
const audioGUID = "71705265666978617564696f70726f62"
const fixtureUnityVersion = "6000.6.3f1"

type Fixture struct {
	SchemaVersion     int               `json:"schemaVersion"`
	Project           string            `json:"project"`
	UnityVersion      string            `json:"unityVersion"`
	ModelAsset        string            `json:"modelAsset"`
	AudioAsset        string            `json:"audioAsset"`
	ModelGUID         string            `json:"modelGuid"`
	AudioGUID         string            `json:"audioGuid"`
	ReferenceIncluded bool              `json:"referenceIncluded"`
	InputHashes       map[string]string `json:"inputHashes"`
	SourceHashes      map[string]string `json:"sourceHashes"`
}

func bytesSHA(data []byte) string {
	hash := sha256.Sum256(data)
	return hex.EncodeToString(hash[:])
}

func fixtureWAV() []byte {
	const frames = 4096
	const channels = 2
	const rate = 22050
	const dataSize = frames * channels * 2
	var output bytes.Buffer
	output.WriteString("RIFF")
	binary.Write(&output, binary.LittleEndian, uint32(36+dataSize))
	output.WriteString("WAVEfmt ")
	binary.Write(&output, binary.LittleEndian, uint32(16))
	binary.Write(&output, binary.LittleEndian, uint16(1))
	binary.Write(&output, binary.LittleEndian, uint16(channels))
	binary.Write(&output, binary.LittleEndian, uint32(rate))
	binary.Write(&output, binary.LittleEndian, uint32(rate*channels*2))
	binary.Write(&output, binary.LittleEndian, uint16(channels*2))
	binary.Write(&output, binary.LittleEndian, uint16(16))
	output.WriteString("data")
	binary.Write(&output, binary.LittleEndian, uint32(dataSize))
	for index := 0; index < frames; index++ {
		left := int16((index*173+7919)%49152 - 24576)
		right := int16((index*307+3571)%40960 - 20480)
		binary.Write(&output, binary.LittleEndian, left)
		binary.Write(&output, binary.LittleEndian, right)
	}
	return output.Bytes()
}

func fixtureAudioMeta() []byte {
	return []byte(fmt.Sprintf(`fileFormatVersion: 2
guid: %s
AudioImporter:
  externalObjects: {}
  serializedVersion: 8
  defaultSettings:
    serializedVersion: 2
    loadType: 0
    sampleRateSetting: 0
    sampleRateOverride: 22050
    compressionFormat: 0
    quality: 1
    conversionMode: 0
    preloadAudioData: 1
  platformSettingOverrides: {}
  forceToMono: 0
  normalize: 0
  loadInBackground: 0
  ambisonic: 0
  3D: 1
  userData:
  assetBundleName:
  assetBundleVariant:
`, audioGUID))
}

// canonicalDestination resolves existing ancestors without creating any files.
func canonicalDestination(path string) (string, error) {
	var missing []string
	current := path
	for {
		if _, err := os.Lstat(current); err == nil {
			resolved, err := filepath.EvalSymlinks(current)
			if err != nil {
				return "", err
			}
			for index := len(missing) - 1; index >= 0; index-- {
				resolved = filepath.Join(resolved, missing[index])
			}
			return resolved, nil
		} else if !os.IsNotExist(err) {
			return "", err
		}
		parent := filepath.Dir(current)
		if parent == current {
			return "", fmt.Errorf("no existing ancestor for %s", path)
		}
		missing = append(missing, filepath.Base(current))
		current = parent
	}
}

func withinDirectory(path, parent string) bool {
	relative, err := filepath.Rel(parent, path)
	return err == nil && relative != ".." && !strings.HasPrefix(relative, ".."+string(filepath.Separator)) && !filepath.IsAbs(relative)
}

// CreateFixture creates output/project with exact source FBX settings and a
// deterministic stereo PCM16 WAV. It starts no Unity process and provisions no
// existing project Library. The optional Editor helper is an oracle only.
func CreateFixture(sourceProject, output string, includeReference bool) (Fixture, error) {
	var result Fixture
	sourceProject, err := filepath.Abs(sourceProject)
	if err != nil {
		return result, err
	}
	sourceProject, err = filepath.EvalSymlinks(sourceProject)
	if err != nil {
		return result, err
	}
	output, err = filepath.Abs(output)
	if err != nil {
		return result, err
	}
	temporary := filepath.Join(sourceProject, "Temp")
	canonicalOutput, err := canonicalDestination(output)
	if err != nil {
		return result, err
	}
	canonicalTemporary, err := canonicalDestination(temporary)
	if err != nil {
		return result, err
	}
	if !withinDirectory(output, temporary) || !withinDirectory(canonicalOutput, canonicalTemporary) || !withinDirectory(canonicalTemporary, sourceProject) {
		return result, fmt.Errorf("worker fixture output must stay inside source project Temp")
	}
	if _, err = os.Lstat(filepath.Join(output, "fixture.json")); err == nil {
		return result, fmt.Errorf("worker fixture already exists: %s", output)
	} else if !os.IsNotExist(err) {
		return result, err
	}
	source := filepath.Join(sourceProject, filepath.FromSlash(modelSource))
	model, err := os.ReadFile(source)
	if err != nil {
		return result, err
	}
	meta, err := os.ReadFile(source + ".meta")
	if err != nil {
		return result, err
	}
	if len(model) > 64*1024*1024 || !bytes.HasPrefix(model, []byte("Kaydara FBX Binary")) {
		return result, fmt.Errorf("worker fixture needs the small binary R_Floor.fbx")
	}
	guid := regexp.MustCompile(`(?m)^guid:\s*([0-9a-fA-F]{32})\s*$`).FindSubmatch(meta)
	if guid == nil {
		return result, fmt.Errorf("source FBX metadata has no GUID")
	}
	var oracle []byte
	if includeReference {
		toolRoot, rootErr := microEditorRoot()
		if rootErr != nil {
			return result, rootErr
		}
		oracle, err = os.ReadFile(filepath.Join(toolRoot, "WorkerProbe/Editor/WorkerImportReference.cs"))
		if err != nil {
			return result, err
		}
	}
	if err = os.MkdirAll(output, 0755); err != nil {
		return result, err
	}
	project := filepath.Join(output, "project")
	if err = os.Mkdir(project, 0755); err != nil {
		return result, err
	}
	for _, directory := range []string{"Assets/Probe", "ProjectSettings", "Packages", "Temp", "Library", "Logs"} {
		if err = os.MkdirAll(filepath.Join(project, filepath.FromSlash(directory)), 0755); err != nil {
			return result, err
		}
	}
	inputs := map[string][]byte{modelAsset: model, modelAsset + ".meta": meta,
		audioAsset: fixtureWAV(), audioAsset + ".meta": fixtureAudioMeta(),
		"ProjectSettings/ProjectVersion.txt": []byte("m_EditorVersion: " + fixtureUnityVersion + "\n"),
		"Packages/manifest.json":             []byte("{\"dependencies\": {}}\n")}
	if includeReference {
		if err = os.Mkdir(filepath.Join(project, "Assets/Editor"), 0755); err != nil {
			return result, err
		}
		inputs["Assets/Editor/WorkerImportReference.cs"] = oracle
	}
	result = Fixture{SchemaVersion: 1, Project: project, UnityVersion: fixtureUnityVersion,
		ModelAsset: modelAsset, AudioAsset: audioAsset, ModelGUID: strings.ToLower(string(guid[1])), AudioGUID: audioGUID,
		ReferenceIncluded: includeReference, InputHashes: make(map[string]string),
		SourceHashes: map[string]string{source: bytesSHA(model), source + ".meta": bytesSHA(meta)}}
	for name, content := range inputs {
		if err = os.WriteFile(filepath.Join(project, filepath.FromSlash(name)), content, 0644); err != nil {
			return result, err
		}
		result.InputHashes[name] = bytesSHA(content)
	}
	encoded, err := json.MarshalIndent(result, "", "  ")
	if err != nil {
		return result, err
	}
	err = os.WriteFile(filepath.Join(output, "fixture.json"), append(encoded, '\n'), 0644)
	return result, err
}
