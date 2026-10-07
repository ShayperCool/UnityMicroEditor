package main

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"

	goproject "unitymicroeditor/project"
)

// Built-in LightingData is accepted only with public Editor observations bound
// to the exact authored scene. A GUID alone does not prove the asset is empty.
func validateAgentSceneLighting(contents []byte, project, scene, proofPath string) error {
	documents, err := goproject.ParseUnityYAML(contents)
	if err != nil {
		return err
	}
	needsProof := false
	for _, document := range documents {
		for _, property := range []string{"m_LightingDataAsset", "m_LightingSettings", "m_OcclusionCullingData", "m_NavMeshData"} {
			if reference, ok := document.Data[property].(map[string]any); ok && fmt.Sprint(reference["fileID"]) != "0" {
				if document.ClassID == 157 && property == "m_LightingDataAsset" && fmt.Sprint(reference["fileID"]) == "20201" && reference["guid"] == goproject.BuiltinResourceGUID {
					needsProof = true
					continue
				}
				return fmt.Errorf("first raw preparation does not support retained baked data: %s", property)
			}
		}
		if lightmaps, ok := document.Data["m_Lightmaps"].([]any); ok && len(lightmaps) != 0 {
			return errors.New("first raw preparation does not support baked lightmaps")
		}
	}
	if !needsProof {
		return nil
	}
	if proofPath == "" {
		return errors.New("built-in LightingData requires --scene-proof exported through public Editor APIs")
	}
	if schema, backend, err := agentSceneProofBackend(proofPath); err != nil {
		return err
	} else if schema == 2 && backend == "go_source_native_resource" {
		return validateAgentSourceSceneProof(contents, project, scene, proofPath)
	}
	var proof struct {
		SchemaVersion             int    `json:"schemaVersion"`
		ProjectRoot               string `json:"projectRoot"`
		UnityVersion              string `json:"unityVersion"`
		ScenePath                 string `json:"scenePath"`
		SceneSHA256               string `json:"sceneSHA256"`
		LightmapCount             *int   `json:"lightmapCount"`
		LightProbeCount           *int   `json:"lightProbeCount"`
		ActiveSceneCount          int    `json:"activeSceneCount"`
		IsPlaying                 *bool  `json:"isPlaying"`
		SceneDirty                *bool  `json:"sceneDirty"`
		LightingDataIdentityKnown bool   `json:"lightingDataIdentityKnown"`
		LightingDataGuid          string `json:"lightingDataGuid"`
		LightingDataFileID        int    `json:"lightingDataFileId"`
	}
	if err := readJSONFile(proofPath, &proof); err != nil {
		return err
	}
	if proof.SchemaVersion != 1 || proof.ProjectRoot != project || proof.UnityVersion != fixtureUnityVersion || proof.ScenePath != scene || proof.SceneSHA256 != bytesSHA(contents) || proof.LightmapCount == nil || *proof.LightmapCount != 0 || proof.LightProbeCount == nil || *proof.LightProbeCount != 0 || proof.ActiveSceneCount != 1 || proof.IsPlaying == nil || *proof.IsPlaying || proof.SceneDirty == nil || *proof.SceneDirty || !proof.LightingDataIdentityKnown || proof.LightingDataGuid != goproject.BuiltinResourceGUID || proof.LightingDataFileID != 20201 {
		return errors.New("scene lighting proof must match the exact saved scene, pinned Unity and project, with no baked lightmaps or probes")
	}
	return nil
}

// Publish without replacing a concurrent preparation or an existing baseline.
func publishAgentBaseline(path string, value any) error {
	data, err := json.Marshal(value)
	if err != nil {
		return err
	}
	file, err := os.CreateTemp(filepath.Dir(path), ".baseline-")
	if err != nil {
		return err
	}
	defer os.Remove(file.Name())
	if _, err := file.Write(append(data, '\n')); err != nil {
		file.Close()
		return err
	}
	if err := file.Sync(); err != nil {
		file.Close()
		return err
	}
	if err := file.Close(); err != nil {
		return err
	}
	return os.Link(file.Name(), path)
}
