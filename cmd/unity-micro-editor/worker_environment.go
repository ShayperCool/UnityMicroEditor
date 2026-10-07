package main

import (
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	goproject "unitymicroeditor/internal/project"
)

// This profile belongs to the pinned 6000.6.3f1 Linux standalone importer.
// Automatic Linux graphics APIs were observed as OpenGLCore 17 and Vulkan 21.
// Project values are read from exact copied source, never from an Editor cache.
type workerEnvironment struct {
	UnityVersion                    string `json:"unityVersion"`
	SettingsSHA256                  string `json:"settingsSha256"`
	BuildTarget                     uint32 `json:"buildTarget"`
	Subtarget                       uint32 `json:"subtarget"`
	ColorSpace                      uint32 `json:"colorSpace"`
	GraphicsAPIMask                 uint32 `json:"graphicsApiMask"`
	ShaderPrecisionModel            uint32 `json:"shaderPrecisionModel"`
	TextureMaxSizeOverride          uint32 `json:"textureMaxSizeOverride"`
	TextureCompressionOverride      uint32 `json:"textureCompressionOverride"`
	DefaultTextureCompressionFormat uint32 `json:"defaultTextureCompressionFormat"`
}

// Bump this revision whenever the pinned importer wire environment changes.
const workerEnvironmentRevision = "6000.6.3f1-linux-native-environment-v2"

func runNativeEnvironmentCLI(args []string) error {
	flags := flag.NewFlagSet("native-environment", flag.ContinueOnError)
	project := flags.String("project", ".", "source Unity project root")
	editor := flags.String("editor", defaultUnityEditor(), "pinned Unity executable")
	if err := flags.Parse(args); err != nil {
		return err
	}
	if flags.NArg() != 0 {
		return errors.New("use native-environment --project <source> [--editor <pinned Unity executable>]")
	}
	fingerprint, err := nativeEnvironmentFingerprint(*project, *editor)
	if err != nil {
		return err
	}
	environment, err := readWorkerEnvironment(*project)
	if err != nil {
		return err
	}
	return json.NewEncoder(os.Stdout).Encode(struct {
		Revision    string             `json:"revision"`
		SHA256      string             `json:"environmentSha256"`
		Environment *workerEnvironment `json:"environment"`
	}{workerEnvironmentRevision, fingerprint, environment})
}

func nativeEnvironmentFingerprint(project, editor string) (string, error) {
	environment, err := readWorkerEnvironment(project)
	if err != nil {
		return "", err
	}
	data, err := json.Marshal(environment)
	if err != nil {
		return "", err
	}
	payload := []byte(workerEnvironmentRevision + "\x00" + editor + "\x00")
	payload = append(payload, data...)
	payload = append(payload, environment.message(initializeType)...)
	return bytesSHA(payload), nil
}

func readWorkerEnvironment(project string) (*workerEnvironment, error) {
	filename := filepath.Join(project, "ProjectSettings/ProjectSettings.asset")
	info, err := os.Stat(filename)
	if err != nil {
		return nil, err
	}
	if !info.Mode().IsRegular() || info.Size() > 1<<20 {
		return nil, errors.New("native worker PlayerSettings is not bounded source text")
	}
	data, err := os.ReadFile(filename)
	if err != nil {
		return nil, err
	}
	version, err := os.ReadFile(filepath.Join(project, "ProjectSettings/ProjectVersion.txt"))
	if err != nil || !strings.Contains(string(version), "m_EditorVersion: "+fixtureUnityVersion+"\n") &&
		!strings.Contains(string(version), "m_EditorVersion: "+fixtureUnityVersion+"\r\n") {
		return nil, errors.New("native worker environment requires pinned project Unity version")
	}
	environment, err := parseWorkerEnvironment(data)
	if err != nil {
		return nil, err
	}
	environment.SettingsSHA256 = bytesSHA(data)
	return environment, nil
}

func parseWorkerEnvironment(data []byte) (*workerEnvironment, error) {
	if len(data) > 1<<20 {
		return nil, errors.New("native worker PlayerSettings exceeds supported bounds")
	}
	documents, err := goproject.ParseUnityYAML(data)
	if err != nil {
		return nil, fmt.Errorf("native worker requires one authored PlayerSettings document: %w", err)
	}
	if len(documents) != 1 || documents[0].ClassID != 129 || documents[0].TypeName != "PlayerSettings" {
		return nil, errors.New("native worker requires one authored PlayerSettings document")
	}
	settings := documents[0].Data
	color, ok := settings["m_ActiveColorSpace"].(int64)
	if !ok || color < 0 || color > 1 {
		return nil, errors.New("native worker PlayerSettings color space is missing or unsupported")
	}
	precision, ok := settings["shaderPrecisionModel"].(int64)
	if !ok || precision < 0 || precision > 1 {
		return nil, errors.New("native worker PlayerSettings shader precision is missing or unsupported")
	}
	environment := &workerEnvironment{UnityVersion: fixtureUnityVersion, BuildTarget: 24, Subtarget: 2,
		ColorSpace: uint32(color), GraphicsAPIMask: 1<<17 | 1<<21, ShaderPrecisionModel: uint32(precision)}
	apis, ok := settings["m_BuildTargetGraphicsAPIs"].([]any)
	if !ok || len(apis) > 64 {
		return nil, errors.New("native worker PlayerSettings graphics API overrides are missing or unsupported")
	}
	seen := map[string]bool{}
	for _, item := range apis {
		entry, ok := item.(map[string]any)
		name, validName := entry["m_BuildTarget"].(string)
		if !ok || !validName || name == "" || seen[name] {
			return nil, errors.New("native worker PlayerSettings graphics target is invalid or repeated")
		}
		seen[name] = true
		if name != "LinuxStandaloneSupport" {
			continue
		}
		automatic, ok := entry["m_Automatic"].(int64)
		if !ok || automatic != 0 && automatic != 1 {
			return nil, errors.New("native worker Linux graphics automatic setting is invalid")
		}
		if automatic == 1 {
			continue
		}
		raw, ok := entry["m_APIs"].(string)
		if !ok || !strings.HasSuffix(raw, "i") || len(raw) > 8*8+1 {
			return nil, errors.New("native worker Linux graphics API encoding is unsupported")
		}
		bytes, err := hex.DecodeString(strings.TrimSuffix(raw, "i"))
		if err != nil || len(bytes) == 0 || len(bytes)%4 != 0 {
			return nil, errors.New("native worker Linux graphics API encoding is invalid")
		}
		environment.GraphicsAPIMask = 0
		for index := 0; index < len(bytes); index += 4 {
			api := binary.LittleEndian.Uint32(bytes[index : index+4])
			if api != 17 && api != 21 || environment.GraphicsAPIMask&(1<<api) != 0 {
				return nil, errors.New("native worker Linux graphics API is unsupported or repeated")
			}
			environment.GraphicsAPIMask |= 1 << api
		}
	}
	formats, ok := settings["m_BuildTargetDefaultTextureCompressionFormat"].([]any)
	if !ok || len(formats) > 64 {
		return nil, errors.New("native worker default texture compression settings are missing or unsupported")
	}
	seen = map[string]bool{}
	for _, item := range formats {
		entry, ok := item.(map[string]any)
		name, validName := entry["m_BuildTarget"].(string)
		if !ok || !validName || name == "" || seen[name] {
			return nil, errors.New("native worker default texture compression target is invalid or repeated")
		}
		seen[name] = true
		if name != "Standalone" && name != "LinuxStandaloneSupport" {
			continue
		}
		raw, ok := entry["m_Formats"].(string)
		if !ok || raw != "00000000i" {
			return nil, errors.New("native worker Linux default texture compression requires the verified DXTC profile")
		}
	}
	return environment, nil
}

func (environment *workerEnvironment) message(kind byte) []byte {
	payload := initializeMessage(int32(environment.BuildTarget))
	payload[0] = kind
	binary.LittleEndian.PutUint32(payload[8:12], environment.Subtarget)
	payload[24] = 1 // The pinned worker enables texture import compression.
	binary.LittleEndian.PutUint32(payload[28:32], environment.ColorSpace)
	binary.LittleEndian.PutUint32(payload[32:36], environment.GraphicsAPIMask)
	binary.LittleEndian.PutUint32(payload[36:40], 1) // Unity 6 latest scripting runtime.
	keys := []string{"graphics/shader-precision-model", "graphics/metal-needs-renderpass-fbfetch-fallback", "graphics/texture-import-overrides", "graphics/default-texture-compression-format"}
	binary.LittleEndian.PutUint32(payload[44:48], 96-44)
	binary.LittleEndian.PutUint32(payload[48:52], uint32(len(keys)))
	payload = append(payload, make([]byte, len(keys)*24)...)
	for index, key := range keys {
		start := 96 + index*24
		binary.LittleEndian.PutUint32(payload[start:start+4], uint32(len(payload)-start))
		// Unity stores enum/bool+1 in the first Hash128 byte for these keys.
		// Linux cannot require the Metal renderpass fallback, hence false+1.
		payload[start+8] = 1
		if index == 0 {
			payload[start+8] += byte(environment.ShaderPrecisionModel)
		}
		// Native producer packs valid marker, optional log2(maxSize)+1, and
		// compression override. This coordinator explicitly uses no overrides.
		if index == 3 {
			payload[start+8] += byte(environment.DefaultTextureCompressionFormat)
		}
		payload = append(payload, key...)
		payload = append(payload, 0)
	}
	return payload
}
