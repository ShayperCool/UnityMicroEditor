package main

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
)

func TestSourceSceneProofPreservesMetadataAndRejectsChangedIdentity(t *testing.T) {
	project := agentReviewProject(t)
	scene := "Assets/SourceProof.unity"
	contents := []byte("%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!157 &3\nLightmapSettings:\n  m_LightingDataAsset: {fileID: 0}\n  m_LightingSettings: {fileID: 0}\n  m_Lightmaps: []\n")
	meta := []byte("fileFormatVersion: 2\nguid: 77777777777777777777777777777777\nDefaultImporter:\n  externalObjects: {}\n")
	if err := os.WriteFile(filepath.Join(project, scene), contents, 0600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(project, scene)+".meta", meta, 0600); err != nil {
		t.Fatal(err)
	}
	output := filepath.Join(project, "Library/UnityMicroEditor/source-scene/proof.json")
	proof, err := buildAgentSourceSceneProof(context.Background(), project, "/explicit/unused/SDK/Unity", scene, output)
	if err != nil || proof.SchemaVersion != 2 || proof.Backend != "go_source_native_resource" || proof.BuiltinLighting != nil {
		t.Fatalf("own source evidence failed: %#v %v", proof, err)
	}
	if err := validateAgentSourceSceneProof(contents, project, scene, output); err != nil {
		t.Fatal(err)
	}
	current, _ := os.ReadFile(filepath.Join(project, scene))
	currentMeta, _ := os.ReadFile(filepath.Join(project, scene) + ".meta")
	if string(current) != string(contents) || string(currentMeta) != string(meta) {
		t.Fatal("source-only proof changed authored scene/meta")
	}
	changed := append(append([]byte{}, contents...), []byte("# new source revision\n")...)
	if err := validateAgentSourceSceneProof(changed, project, scene, output); err == nil {
		t.Fatal("proof accepted changed scene content")
	}
	if err := os.WriteFile(filepath.Join(project, scene)+".meta", append(meta, '#'), 0600); err != nil {
		t.Fatal(err)
	}
	if err := validateAgentSourceSceneProof(contents, project, scene, output); err == nil {
		t.Fatal("proof accepted changed metadata")
	}
	if err := os.WriteFile(filepath.Join(project, scene)+".meta", meta, 0600); err != nil {
		t.Fatal(err)
	}
	proof.Backend = "unity_editor"
	encoded, _ := json.Marshal(proof)
	if err := os.WriteFile(output, encoded, 0600); err != nil {
		t.Fatal(err)
	}
	if err := validateAgentSourceSceneProof(contents, project, scene, output); err == nil {
		t.Fatal("source proof was accepted after relabeling it as Editor evidence")
	}
}

func TestSourceSceneRejectsBakedAndMalformedReferences(t *testing.T) {
	for _, field := range []string{
		"m_LightingDataAsset: {fileID: 20201, guid: 77777777777777777777777777777777}",
		"m_LightingDataAsset: invalid",
		"m_LightingSettings: {fileID: 11400000, guid: 77777777777777777777777777777777}",
		"m_OcclusionCullingData: {fileID: 12}",
		"m_NavMeshData: {fileID: 12}",
		"m_Lightmaps: [{texture: {fileID: 12}}]",
		"m_Lightmaps: invalid",
	} {
		t.Run(field, func(t *testing.T) {
			content := []byte("%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!157 &3\nLightmapSettings:\n  " + field + "\n")
			if _, err := inspectAgentSourceScene(content); err == nil {
				t.Fatal("unsupported baked state was accepted")
			}
		})
	}
}
