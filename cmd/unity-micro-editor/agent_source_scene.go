package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"reflect"

	goproject "unitymicroeditor/internal/project"
	"unitymicroeditor/internal/runtimeartifact"
)

type agentSourceSceneProof struct {
	SchemaVersion   int                                      `json:"schemaVersion"`
	Backend         string                                   `json:"backend"`
	ProjectRoot     string                                   `json:"projectRoot"`
	UnityVersion    string                                   `json:"unityVersion"`
	ScenePath       string                                   `json:"scenePath"`
	SceneSHA256     string                                   `json:"sceneSHA256"`
	SceneMetaSHA256 string                                   `json:"sceneMetaSha256"`
	BuiltinLighting *runtimeartifact.BuiltinLightingIdentity `json:"builtinLighting,omitempty"`
	ResourcePath    string                                   `json:"resourcePath,omitempty"`
	ResourceSHA256  string                                   `json:"resourceSha256,omitempty"`
}

func inspectAgentSourceScene(contents []byte) (bool, error) {
	documents, err := goproject.ParseUnityYAML(contents)
	if err != nil {
		return false, err
	}
	needsBuiltin := false
	for _, document := range documents {
		for _, property := range []string{"m_LightingDataAsset", "m_LightingSettings", "m_OcclusionCullingData", "m_NavMeshData"} {
			value, exists := document.Data[property]
			if !exists {
				continue
			}
			reference, valid := value.(map[string]any)
			if !valid || reference["fileID"] == nil {
				return false, fmt.Errorf("scene baked-data property is not an exact authored PPtr: %s", property)
			}
			if fmt.Sprint(reference["fileID"]) == "0" {
				continue
			}
			if document.ClassID == 157 && property == "m_LightingDataAsset" && fmt.Sprint(reference["fileID"]) == "20201" && reference["guid"] == goproject.BuiltinResourceGUID {
				needsBuiltin = true
				continue
			}
			return false, fmt.Errorf("raw source preparation does not support retained baked data: %s", property)
		}
		if value, exists := document.Data["m_Lightmaps"]; exists {
			lightmaps, valid := value.([]any)
			if !valid || len(lightmaps) != 0 {
				return false, errors.New("raw source preparation requires an empty authored lightmap array")
			}
		}
	}
	return needsBuiltin, nil
}

// Our proof is explicitly source/native-resource evidence. It never claims an
// Editor opened the target scene, nor fabricates public Editor observations.
func buildAgentSourceSceneProof(ctx context.Context, project, editor, scene, output string) (agentSourceSceneProof, error) {
	proof := agentSourceSceneProof{SchemaVersion: 2, Backend: "go_source_native_resource", ProjectRoot: project,
		UnityVersion: fixtureUnityVersion, ScenePath: scene}
	path := filepath.Join(project, filepath.FromSlash(scene))
	if err := agentCanonical(path); err != nil {
		return proof, err
	}
	contents, err := readAgentFile(path, 32<<20)
	if err != nil {
		return proof, err
	}
	needsBuiltin, err := inspectAgentSourceScene(contents)
	if err != nil {
		return proof, err
	}
	proof.SceneSHA256 = bytesSHA(contents)
	proof.SceneMetaSHA256, err = goFileHash(path + ".meta")
	if err != nil {
		return proof, err
	}
	if needsBuiltin {
		if err := ctx.Err(); err != nil {
			return proof, err
		}
		proof.ResourcePath = filepath.Join(filepath.Dir(editor), "Data/Resources/unity_builtin_extra")
		if err := agentCanonical(proof.ResourcePath); err != nil {
			return proof, err
		}
		file, err := os.Open(proof.ResourcePath)
		if err != nil {
			return proof, err
		}
		info, err := file.Stat()
		if err != nil || !info.Mode().IsRegular() {
			file.Close()
			return proof, errors.New("builtin lighting resource is not a regular native file")
		}
		identity, err := runtimeartifact.InspectBuiltinLighting(file, info.Size())
		file.Close()
		if err != nil {
			return proof, err
		}
		proof.BuiltinLighting = &identity
		proof.ResourceSHA256, err = goFileHash(proof.ResourcePath)
		if err != nil {
			return proof, err
		}
	}
	if current, err := goFileHash(path); err != nil || current != proof.SceneSHA256 {
		return proof, errors.New("scene changed during source/native lighting validation")
	}
	if current, err := goFileHash(path + ".meta"); err != nil || current != proof.SceneMetaSHA256 {
		return proof, errors.New("scene metadata changed during source/native lighting validation")
	}
	if output != "" {
		if !goproject.IsOwnedCachePath(project, output) {
			return proof, errors.New("source scene proof output is outside owned project caches")
		}
		if err := agentImmutableJSON(output, proof); err != nil {
			return proof, err
		}
	}
	return proof, nil
}

func validateAgentSourceSceneProof(contents []byte, project, scene, path string) error {
	var proof agentSourceSceneProof
	if err := agentReadJSON(path, 1<<20, &proof); err != nil {
		return err
	}
	if proof.SchemaVersion != 2 || proof.Backend != "go_source_native_resource" || proof.ProjectRoot != project || proof.UnityVersion != fixtureUnityVersion ||
		proof.ScenePath != scene || proof.SceneSHA256 != bytesSHA(contents) {
		return errors.New("source scene proof differs from its exact authored scene/project/version")
	}
	needsBuiltin, err := inspectAgentSourceScene(contents)
	if err != nil {
		return err
	}
	if current, err := goFileHash(filepath.Join(project, filepath.FromSlash(scene)) + ".meta"); err != nil || current != proof.SceneMetaSHA256 {
		return errors.New("source scene proof metadata identity changed")
	}
	if !needsBuiltin {
		if proof.BuiltinLighting != nil || proof.ResourcePath != "" || proof.ResourceSHA256 != "" {
			return errors.New("source scene proof claims unused native lighting evidence")
		}
		return nil
	}
	if proof.BuiltinLighting == nil || proof.ResourceSHA256 == "" || !filepath.IsAbs(proof.ResourcePath) {
		return errors.New("source scene proof lacks its exact native lighting resource")
	}
	if err := agentCanonical(proof.ResourcePath); err != nil {
		return err
	}
	if current, err := goFileHash(proof.ResourcePath); err != nil || current != proof.ResourceSHA256 {
		return errors.New("source scene proof native resource changed")
	}
	file, err := os.Open(proof.ResourcePath)
	if err != nil {
		return err
	}
	defer file.Close()
	info, err := file.Stat()
	if err != nil {
		return err
	}
	actual, err := runtimeartifact.InspectBuiltinLighting(file, info.Size())
	if err != nil || !reflect.DeepEqual(actual, *proof.BuiltinLighting) {
		return errors.Join(errors.New("source scene proof native lighting identity differs"), err)
	}
	return nil
}

func agentSceneProofBackend(path string) (int, string, error) {
	data, err := readAgentFile(path, 1<<20)
	if err != nil {
		return 0, "", err
	}
	var value struct {
		SchemaVersion int    `json:"schemaVersion"`
		Backend       string `json:"backend"`
	}
	if err := json.Unmarshal(data, &value); err != nil {
		return 0, "", err
	}
	return value.SchemaVersion, value.Backend, nil
}
