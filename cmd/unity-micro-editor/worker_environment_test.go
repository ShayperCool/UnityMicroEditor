package main

import (
	"bytes"
	"encoding/binary"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

const workerSettingsFixture = "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!129 &1\nPlayerSettings:\n  m_ActiveColorSpace: 1\n  shaderPrecisionModel: 0\n  m_BuildTargetGraphicsAPIs: []\n  m_BuildTargetDefaultTextureCompressionFormat: []\n"

func TestWorkerEnvironmentPreservesSourceGraphicsSettingsAndNativeDependencies(t *testing.T) {
	environment, err := parseWorkerEnvironment([]byte(workerSettingsFixture))
	if err != nil {
		t.Fatal(err)
	}
	if environment.ColorSpace != 1 || environment.GraphicsAPIMask != 2228224 || environment.ShaderPrecisionModel != 0 {
		t.Fatalf("source or pinned Linux automatic graphics profile changed: %+v", environment)
	}
	initialize, prepare := environment.message(initializeType), environment.message(prepareType)
	if initialize[0] != initializeType || prepare[0] != prepareType || !bytes.Equal(initialize[1:], prepare[1:]) {
		t.Fatal("Prepare loses native graphics dependencies")
	}
	if binary.LittleEndian.Uint32(initialize[4:8]) != 24 || binary.LittleEndian.Uint32(initialize[8:12]) != 2 || initialize[24] != 1 ||
		binary.LittleEndian.Uint32(initialize[28:32]) != 1 || binary.LittleEndian.Uint32(initialize[32:36]) != 2228224 || binary.LittleEndian.Uint32(initialize[36:40]) != 1 {
		t.Fatal("native environment fields overlap or lose authored values")
	}
	dependencies, _, err := blobArray(initialize, 44, 24)
	if err != nil || len(dependencies) != 96 {
		t.Fatalf("custom dependency table cannot be decoded by native blob layout: %v", err)
	}
	for index, key := range []string{"graphics/shader-precision-model", "graphics/metal-needs-renderpass-fbfetch-fallback", "graphics/texture-import-overrides", "graphics/default-texture-compression-format"} {
		value, err := blobString(initialize, 96+24*index)
		if err != nil || value != key {
			t.Fatalf("dependency key changed: %q %v", value, err)
		}
		if !bytes.Equal(dependencies[index*24+8:index*24+24], append([]byte{1}, make([]byte, 15)...)) {
			t.Fatal("native enum/bool dependency Hash128 changed")
		}
	}
	custom := strings.Replace(workerSettingsFixture, "shaderPrecisionModel: 0", "shaderPrecisionModel: 1", 1)
	custom = strings.Replace(custom, "m_BuildTargetGraphicsAPIs: []", "m_BuildTargetGraphicsAPIs:\n  - m_BuildTarget: LinuxStandaloneSupport\n    m_APIs: 15000000i\n    m_Automatic: 0", 1)
	environment, err = parseWorkerEnvironment([]byte(custom))
	if err != nil || environment.GraphicsAPIMask != 1<<21 || environment.ShaderPrecisionModel != 1 || environment.message(initializeType)[104] != 2 {
		t.Fatalf("authored Vulkan/precision override lost: %+v %v", environment, err)
	}
}

func TestWorkerEnvironmentRejectsAmbiguousOrUnprovenSettings(t *testing.T) {
	for _, source := range []string{
		strings.Replace(workerSettingsFixture, "m_ActiveColorSpace: 1", "m_ActiveColorSpace: 2", 1),
		strings.Replace(workerSettingsFixture, "shaderPrecisionModel: 0", "shaderPrecisionModel: 2", 1),
		strings.Replace(workerSettingsFixture, "shaderPrecisionModel: 0\n", "", 1),
		strings.Replace(workerSettingsFixture, "shaderPrecisionModel: 0", "shaderPrecisionModel: 0\n  shaderPrecisionModel: 1", 1),
		strings.Replace(workerSettingsFixture, "!u!129", "!u!114", 1),
		strings.Replace(workerSettingsFixture, "m_BuildTargetGraphicsAPIs: []", "m_BuildTargetGraphicsAPIs:\n  - m_BuildTarget: LinuxStandaloneSupport\n    m_APIs: 02000000i\n    m_Automatic: 0", 1),
		strings.Replace(workerSettingsFixture, "m_BuildTargetGraphicsAPIs: []", "m_BuildTargetGraphicsAPIs:\n  - m_BuildTarget: LinuxStandaloneSupport\n    m_APIs: 1500000015000000i\n    m_Automatic: 0", 1),
		strings.Replace(workerSettingsFixture, "m_BuildTargetDefaultTextureCompressionFormat: []", "m_BuildTargetDefaultTextureCompressionFormat:\n  - m_BuildTarget: Standalone\n    m_Formats: 03000000i", 1),
		strings.Replace(workerSettingsFixture, "m_BuildTargetDefaultTextureCompressionFormat: []", "m_BuildTargetDefaultTextureCompressionFormat:\n  - m_BuildTarget: Standalone\n    m_Formats: 00000000i\n  - m_BuildTarget: Standalone\n    m_Formats: 00000000i", 1),
	} {
		if _, err := parseWorkerEnvironment([]byte(source)); err == nil {
			t.Fatal("accepted unsupported native graphics settings")
		}
	}
	project := t.TempDir()
	if err := os.MkdirAll(filepath.Join(project, "ProjectSettings"), 0700); err != nil {
		t.Fatal(err)
	}
	settings := filepath.Join(project, "ProjectSettings/ProjectSettings.asset")
	if err := os.WriteFile(settings, []byte(workerSettingsFixture), 0600); err != nil {
		t.Fatal(err)
	}
	version := filepath.Join(project, "ProjectSettings/ProjectVersion.txt")
	if err := os.WriteFile(version, []byte("m_EditorVersion: 6000.6.3f10\n"), 0600); err != nil {
		t.Fatal(err)
	}
	if _, err := readWorkerEnvironment(project); err == nil {
		t.Fatal("accepted different Unity version for native protocol offsets")
	}
	if err := os.WriteFile(version, []byte("m_EditorVersion: "+fixtureUnityVersion+"\n"), 0600); err != nil {
		t.Fatal(err)
	}
	environment, err := readWorkerEnvironment(project)
	if err != nil || environment.SettingsSHA256 != bytesSHA([]byte(workerSettingsFixture)) {
		t.Fatalf("native environment lost exact PlayerSettings proof: %+v %v", environment, err)
	}
}
