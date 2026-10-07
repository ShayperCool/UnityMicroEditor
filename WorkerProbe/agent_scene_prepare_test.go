package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestAgentSceneLightingRequiresExactPublicProof(t *testing.T) {
	contents := []byte("%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!157 &3\nLightmapSettings:\n  m_LightingDataAsset: {fileID: 20201, guid: 0000000000000000f000000000000000, type: 0}\n  m_LightingSettings: {fileID: 0}\n")
	project := t.TempDir()
	scene := "Assets/Scenes/Test.unity"
	if err := validateAgentSceneLighting(contents, project, scene, ""); err == nil {
		t.Fatal("builtin GUID accepted without public evidence")
	}
	valid := map[string]any{"schemaVersion": 1, "projectRoot": project, "unityVersion": fixtureUnityVersion, "scenePath": scene, "sceneSHA256": bytesSHA(contents), "lightmapCount": 0, "lightProbeCount": 0, "activeSceneCount": 1, "isPlaying": false, "sceneDirty": false, "lightingDataIdentityKnown": true, "lightingDataGuid": "0000000000000000f000000000000000", "lightingDataFileId": 20201}
	for _, scenario := range []string{"valid", "wrong-hash", "baked-map", "baked-probe", "another-project", "another-engine", "dirty", "playing", "multiple-scenes", "missing-count", "wrong-identity", "missing-dirty"} {
		t.Run(scenario, func(t *testing.T) {
			proof := make(map[string]any)
			for k, v := range valid {
				proof[k] = v
			}
			switch scenario {
			case "wrong-hash":
				proof["sceneSHA256"] = "incorrect"
			case "baked-map":
				proof["lightmapCount"] = 1
			case "baked-probe":
				proof["lightProbeCount"] = 1
			case "another-project":
				proof["projectRoot"] = project + "-other"
			case "another-engine":
				proof["unityVersion"] = "6000.0.1f1"
			case "dirty":
				proof["sceneDirty"] = true
			case "playing":
				proof["isPlaying"] = true
			case "multiple-scenes":
				proof["activeSceneCount"] = 2
			case "missing-count":
				delete(proof, "lightmapCount")
			case "wrong-identity":
				proof["lightingDataGuid"] = "00000000000000000000000000000001"
			case "missing-dirty":
				delete(proof, "sceneDirty")
			}
			data, err := json.Marshal(proof)
			if err != nil {
				t.Fatal(err)
			}
			path := filepath.Join(t.TempDir(), "proof.json")
			if err := os.WriteFile(path, data, 0600); err != nil {
				t.Fatal(err)
			}
			err = validateAgentSceneLighting(contents, project, scene, path)
			if (err == nil) != (scenario == "valid") {
				t.Fatalf("unexpected proof acceptance: %v", err)
			}
		})
	}
	custom := []byte(strings.Replace(string(contents), "0000000000000000f000000000000000", "00000000000000000000000000000001", 1))
	if err := validateAgentSceneLighting(custom, project, scene, ""); err == nil {
		t.Fatal("custom baked asset accepted")
	}
	if err := validateAgentSceneLighting([]byte(strings.Replace(string(contents), "fileID: 20201, guid: 0000000000000000f000000000000000, type: 0", "fileID: 0", 1)), project, scene, ""); err != nil {
		t.Fatal(err)
	}
}

func TestAgentBaselinePublicationDoesNotOverwriteAcceptedOutput(t *testing.T) {
	path := filepath.Join(t.TempDir(), "current.json")
	if err := publishAgentBaseline(path, map[string]any{"accepted": 1}); err != nil {
		t.Fatal(err)
	}
	if err := publishAgentBaseline(path, map[string]any{"accepted": 2}); err == nil {
		t.Fatal("existing baseline replaced")
	}
	data, err := os.ReadFile(path)
	if err != nil || string(data) != "{\"accepted\":1}\n" {
		t.Fatalf("accepted baseline changed: %s %v", data, err)
	}
}
