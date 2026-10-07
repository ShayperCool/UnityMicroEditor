package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"

	goproject "unitymicroeditor/internal/project"
)

func TestPreparedSceneRequiresMatchingSourceAndAssemblyContent(t *testing.T) {
	project := t.TempDir()
	assembly := filepath.Join(project, "Temp/Game.dll")
	source := filepath.Join(project, "Assets/Boot.unity")
	for path, content := range map[string]string{assembly: "game assembly", source: "authored source"} {
		if err := os.MkdirAll(filepath.Dir(path), 0700); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, []byte(content), 0600); err != nil {
			t.Fatal(err)
		}
	}
	assemblyHash, _ := goFileHash(assembly)
	sourceHash, _ := goFileHash(source)
	manifest := filepath.Join(project, "Temp/manifest.json")
	value := map[string]any{"schemaVersion": 1, "supported": true, "projectRoot": project, "unityVersion": "6000.6.3f1", "assemblies": []string{assembly}, "sourceDependencies": []any{map[string]any{"path": "Assets/Boot.unity", "sha256": sourceHash}}}
	data, _ := json.Marshal(value)
	if err := os.WriteFile(manifest, data, 0600); err != nil {
		t.Fatal(err)
	}
	manifestHash, _ := goFileHash(manifest)
	launch := preparedLaunch{Project: project, AssemblyHashes: map[string]string{assembly: assemblyHash}}
	if _, err := validatePreparedScene(launch, manifest, manifestHash, map[string]string{}); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(source, []byte("new authored scene"), 0600); err != nil {
		t.Fatal(err)
	}
	if _, err := validatePreparedScene(launch, manifest, manifestHash, map[string]string{}); err == nil {
		t.Fatal("launched a stale source snapshot")
	}
	if err := os.WriteFile(source, []byte("authored source"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(assembly, []byte("new game assembly"), 0600); err != nil {
		t.Fatal(err)
	}
	if _, err := validatePreparedScene(launch, manifest, manifestHash, map[string]string{}); err == nil {
		t.Fatal("launched an unverified managed DLL")
	}
}

func TestPreparedArtifactRejectsConflictingHashes(t *testing.T) {
	file := filepath.Join(t.TempDir(), "artifact")
	if err := os.WriteFile(file, []byte("immutable"), 0600); err != nil {
		t.Fatal(err)
	}
	hash, _ := goFileHash(file)
	observed := map[string]string{}
	if err := requirePreparedFile(file, hash, observed); err != nil {
		t.Fatal(err)
	}
	if err := requirePreparedFile(file, bytesSHA([]byte("another version")), observed); err == nil {
		t.Fatal("accepted two declarations for one artifact")
	}
}

func TestPreparedNativeOnlySceneRequiresVerifiedHostRegistrations(t *testing.T) {
	project := t.TempDir()
	player := filepath.Join(project, "Library/UnityMicroEditor/Host/Game.x86_64")
	registrations := filepath.Join(filepath.Dir(player), "Game_Data/ScriptingAssemblies.json")
	if err := os.MkdirAll(filepath.Dir(registrations), 0700); err != nil {
		t.Fatal(err)
	}
	contents := []byte(`{"names":["Dungeon.CLIPlay.dll","UnityEngine.CoreModule.dll"]}`)
	if err := os.WriteFile(registrations, contents, 0600); err != nil {
		t.Fatal(err)
	}
	manifest := filepath.Join(project, "manifest.json")
	value := map[string]any{"schemaVersion": 1, "supported": true, "projectRoot": project,
		"unityVersion": fixtureUnityVersion, "assemblies": []string{},
		"compiler": map[string]any{"nativeOnly": true, "nativeSlotsSha256": bytesSHA(contents)}}
	validate := func() error {
		t.Helper()
		data, err := json.Marshal(value)
		if err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(manifest, data, 0600); err != nil {
			t.Fatal(err)
		}
		_, err = validatePreparedScene(preparedLaunch{Project: project, Player: player}, manifest, bytesSHA(data), map[string]string{})
		return err
	}
	if err := validate(); err != nil {
		t.Fatal(err)
	}
	delete(value, "compiler")
	if err := validate(); err == nil {
		t.Fatal("unproved empty game assembly list was accepted")
	}
	value["compiler"] = map[string]any{"nativeOnly": true}
	if err := validate(); err == nil {
		t.Fatal("native-only scene without registration digest was accepted")
	}
	value["compiler"] = map[string]any{"nativeOnly": true, "nativeSlotsSha256": bytesSHA(contents)}
	if err := os.WriteFile(registrations, []byte(`{"names":["Other.dll"]}`), 0600); err != nil {
		t.Fatal(err)
	}
	if err := validate(); err == nil {
		t.Fatal("changed native host registrations were accepted")
	}
	missingBridge := []byte(`{"names":["UnityEngine.CoreModule.dll"]}`)
	if err := os.WriteFile(registrations, missingBridge, 0600); err != nil {
		t.Fatal(err)
	}
	value["compiler"] = map[string]any{"nativeOnly": true, "nativeSlotsSha256": bytesSHA(missingBridge)}
	if err := validate(); err == nil {
		t.Fatal("registered runtime bridge was not required")
	}
	value["assemblies"] = []string{"Game.dll"}
	if err := validate(); err == nil {
		t.Fatal("native-only proof accepted additional game assemblies")
	}
}

func TestPreparedPackageSourceResolvesCacheAndRejectsAmbiguity(t *testing.T) {
	project := t.TempDir()
	logical := "Packages/com.unity.test/Shaders/Unlit.shader"
	physical := filepath.Join(project, "Library/PackageCache/com.unity.test@abc/Shaders/Unlit.shader")
	if err := os.MkdirAll(filepath.Dir(physical), 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(physical, []byte("current shader"), 0600); err != nil {
		t.Fatal(err)
	}
	resolved, err := resolvePreparedSource(project, logical)
	if err != nil || resolved != physical {
		t.Fatalf("UPM cache source not resolved: %s, %v", resolved, err)
	}
	sha, _ := goFileHash(physical)
	if err := requirePreparedFile(resolved, sha, map[string]string{}); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(physical, []byte("edited shader"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := requirePreparedFile(resolved, sha, map[string]string{}); err == nil {
		t.Fatal("UPM cache edit was not rejected")
	}
	if err := os.MkdirAll(filepath.Join(project, "Library/PackageCache/com.unity.test@another"), 0700); err != nil {
		t.Fatal(err)
	}
	if _, err := resolvePreparedSource(project, logical); err == nil {
		t.Fatal("ambiguous UPM cache selection accepted")
	}
}

func TestPreparedSceneRechecksShaderIncludeClosureBeforeLaunching(t *testing.T) {
	project := t.TempDir()
	assembly := filepath.Join(project, "Temp/Game.dll")
	shader := filepath.Join(project, "Assets/Screen.shader")
	for path, contents := range map[string]string{assembly: "game assembly", shader: "Shader \"Test/Screen\" {\n#if OPTIONAL\n#include \"Optional.hlsl\"\n#endif\n}\n"} {
		if err := os.MkdirAll(filepath.Dir(path), 0700); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, []byte(contents), 0600); err != nil {
			t.Fatal(err)
		}
	}
	requirements, missing, err := goproject.CollectShaderSourceRequirements(project, "Assets/Screen.shader")
	if err != nil {
		t.Fatal(err)
	}
	manifest := filepath.Join(project, "Temp/manifest.json")
	value := map[string]any{"schemaVersion": 1, "supported": true, "projectRoot": project, "unityVersion": "6000.6.3f1", "assemblies": []string{assembly},
		"compiledShaderSnapshots": []goproject.CompiledShaderSnapshot{{SchemaVersion: 1, SourcePath: "Assets/Screen.shader", SourceRequirements: requirements, MissingIncludes: missing}}}
	data, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(manifest, data, 0600); err != nil {
		t.Fatal(err)
	}
	manifestHash, _ := goFileHash(manifest)
	assemblyHash, _ := goFileHash(assembly)
	launch := preparedLaunch{Project: project, AssemblyHashes: map[string]string{assembly: assemblyHash}}
	if _, err := validatePreparedScene(launch, manifest, manifestHash, map[string]string{}); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(project, "Assets/Optional.hlsl"), []byte("new variant input"), 0600); err != nil {
		t.Fatal(err)
	}
	if _, err := validatePreparedScene(launch, manifest, manifestHash, map[string]string{}); err == nil {
		t.Fatal("prepared launch ignored a new optional Shader include")
	}
}
