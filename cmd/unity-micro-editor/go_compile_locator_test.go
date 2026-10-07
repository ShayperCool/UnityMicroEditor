package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

func compilerLocatorFixture(t *testing.T) (string, string, string, goCompileManifest, goAssemblyConfig) {
	t.Helper()
	project := t.TempDir()
	source := writeGoGraphFile(t, project, "Assets/Known.cs", "public class Known {}")
	dll := writeGoGraphFile(t, project, "Library/UnityMicroEditor/preparations/one/.compiler-preparation/Player/Player_Data/Managed/Assembly-CSharp.dll", "accepted unchanged DLL")
	digest, _ := goFileHash(dll)
	assembly := goCompileAssembly{Name: "Assembly-CSharp", Path: dll, SHA256: digest, Sources: []string{source}, AssemblyInputHash: strings.Repeat("a", 64)}
	plan := goCompileManifest{Schema: 1, Project: project, Unity: fixtureUnityVersion, Assemblies: []goCompileAssembly{assembly}, CompilerInputs: map[string]string{dll: digest}}
	config := goAssemblyConfig{Name: assembly.Name, Sources: assembly.Sources, Defines: []string{"UNITY_STANDALONE_LINUX"}}
	canonical, err := canonicalGoConfiguration(config)
	if err != nil {
		t.Fatal(err)
	}
	sourceDirectory := filepath.Join(project, "Library/UnityMicroEditor/preparations/one/.source-preparation")
	descriptor := filepath.Join(sourceDirectory, "compiler-cache/baseline-assemblies", assembly.AssemblyInputHash, "compile.json")
	if err := agentImmutableJSON(descriptor, map[string]any{"assemblyConfiguration": json.RawMessage(canonical),
		"compilerInputs": map[string]string{"assemblyConfiguration": bytesSHA(canonical)}, "assemblies": []goCompileAssembly{assembly}}); err != nil {
		t.Fatal(err)
	}
	scene := filepath.Join(sourceDirectory, "source/revisions/current/scene.json")
	if err := agentImmutableJSON(scene, map[string]any{"schemaVersion": 1, "supported": true, "projectRoot": project}); err != nil {
		t.Fatal(err)
	}
	wrapper := filepath.Join(project, "Library/UnityMicroEditor/preparations/one/current.json")
	if err := atomicJSON(wrapper, map[string]any{"compiler": plan, "manifest": scene}); err != nil {
		t.Fatal(err)
	}
	return project, wrapper, scene, plan, config
}

func TestGoCompilerLocatesUnchangedDLLConfigurationFromExplicitOwnedSourceManifest(t *testing.T) {
	_, wrapper, scene, plan, config := compilerLocatorFixture(t)
	if _, _, err := readGoAssemblyConfiguration(wrapper, plan.Assemblies[0]); err == nil {
		t.Fatal("fixture does not reproduce a descriptor outside baseline/DLL ancestors")
	}
	accepted, locator, err := goCompilerManifest(wrapper)
	if err != nil || locator != scene {
		t.Fatalf("exact owned scene locator was discarded: %s %v", locator, err)
	}
	actual, _, err := readGoAssemblyConfiguration(locator, accepted.Assemblies[0])
	if err != nil || !reflect.DeepEqual(actual, config) {
		t.Fatalf("unchanged accepted DLL lost its sibling source configuration: %+v %v", actual, err)
	}
}

func TestGoCompilerRejectsForeignOrSymlinkSceneConfigurationLocators(t *testing.T) {
	for _, name := range []string{"foreign cache", "noncache", "symlink", "wrong project", "unsupported scene"} {
		t.Run(name, func(t *testing.T) {
			project, wrapper, scene, plan, _ := compilerLocatorFixture(t)
			locator := scene
			switch name {
			case "foreign cache":
				locator = filepath.Join(t.TempDir(), "Library/UnityMicroEditor/scene.json")
				writeGoGraphFile(t, filepath.Dir(locator), filepath.Base(locator), `{}`)
			case "noncache":
				locator = writeGoGraphFile(t, project, "Assets/scene.json", `{}`)
			case "symlink":
				locator = filepath.Join(filepath.Dir(scene), "alias.json")
				if err := os.Symlink(scene, locator); err != nil {
					t.Fatal(err)
				}
			case "wrong project":
				if err := atomicJSON(scene, map[string]any{"schemaVersion": 1, "supported": true, "projectRoot": t.TempDir()}); err != nil {
					t.Fatal(err)
				}
			case "unsupported scene":
				if err := atomicJSON(scene, map[string]any{"schemaVersion": 1, "supported": false, "projectRoot": project}); err != nil {
					t.Fatal(err)
				}
			}
			if err := atomicJSON(wrapper, map[string]any{"compiler": plan, "manifest": locator}); err != nil {
				t.Fatal(err)
			}
			if _, _, err := goCompilerManifest(wrapper); err == nil {
				t.Fatal("unowned or invalid locator redirected compiler configuration discovery")
			}
		})
	}
}
