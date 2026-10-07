package main

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"unitymicroeditor/project/packages"
)

func TestGoSourcePluginV2V3LinuxAndConstraints(t *testing.T) {
	for _, test := range []struct {
		name, metadata    string
		defines           []string
		enabled, explicit bool
	}{
		{"v2", "PluginImporter:\n  isExplicitlyReferenced: 1\n  platformData:\n  - first:\n      : Any\n    second:\n      enabled: 1\n      settings:\n        Exclude Linux64: 1\n  - first:\n      Standalone: Linux64\n    second:\n      enabled: 1\n      settings: {}\n", nil, true, true},
		{"v3", "PluginImporter:\n  defineConstraints: [DEVELOPMENT_BUILD]\n  platformData:\n    Any:\n      enabled: 1\n      settings: {}\n    Linux64:\n      enabled: 0\n      settings: {}\n", []string{"DEVELOPMENT_BUILD"}, false, false},
		{"constraint", "PluginImporter:\n  defineConstraints: [UNITY_EDITOR]\n  platformData:\n    Any:\n      enabled: 1\n      settings: {}\n", nil, false, false},
	} {
		t.Run(test.name, func(t *testing.T) {
			path := writeGoGraphFile(t, t.TempDir(), "Plugin.dll.meta", test.metadata)
			plugin, err := readGoSourcePlugin(strings.TrimSuffix(path, ".meta"), test.defines)
			if err != nil || plugin.Enabled != test.enabled || plugin.Explicit != test.explicit {
				t.Fatalf("plugin profile %+v: %v", plugin, err)
			}
			content, _ := os.ReadFile(path)
			if string(content) != test.metadata {
				t.Fatal("legacy metadata was rewritten")
			}
		})
	}
}

func TestGoSourceResponseExplicitPluginsAndOptionOrder(t *testing.T) {
	project := t.TempDir()
	source := writeGoGraphFile(t, project, "Assets/Assembly/Probe.cs", "public class Probe {}")
	writeGoGraphFile(t, project, "Assets/csc.rsp", "-define:FROM_RSP -unsafe+ -optimize+ -optimize- -r:Chosen.dll\n")
	plugins := map[string]goSourcePlugin{"Chosen.dll": {Path: filepath.Join(project, "Assets/Chosen.dll"), Enabled: true, Explicit: true}, "Automatic.dll": {Path: filepath.Join(project, "Assets/Automatic.dll"), Enabled: true}}
	config := goAssemblyConfig{Name: "Assembly"}
	config.Config.OverrideReferences = true
	config.Config.Precompiled = []string{"Chosen.dll"}
	inputs := map[string]string{}
	if err := applyGoSourceResponseFiles(project, []string{source}, nil, &config, plugins, inputs); err != nil {
		t.Fatal(err)
	}
	refs, err := goSourcePluginReferences(config, plugins)
	if err != nil || !equalStrings(refs, []string{plugins["Chosen.dll"].Path}) || !config.Config.AllowUnsafe || !equalStrings(config.Defines, []string{"FROM_RSP"}) || !equalStrings(config.CompilerOptions, []string{"-optimize+", "-optimize-"}) {
		t.Fatalf("reference/options mismatch: %+v %+v %v", config, refs, err)
	}
	config.Config.Precompiled = []string{"Missing.dll"}
	if _, err := goSourcePluginReferences(config, plugins); err == nil {
		t.Fatal("missing explicit plugin accepted")
	}
	writeGoGraphFile(t, project, "Assets/csc.rsp", "-analyzer:foreign.dll")
	if err := applyGoSourceResponseFiles(project, []string{source}, nil, &config, plugins, inputs); err == nil {
		t.Fatal("unsupported analyzer silently accepted")
	}
}

func TestGoSourceNativeOptionalReferenceRequiresUnchangedPackageProof(t *testing.T) {
	project := t.TempDir()
	if err := os.Mkdir(filepath.Join(project, "Assets"), 0700); err != nil {
		t.Fatal(err)
	}
	writeGoGraphFile(t, project, "Packages/manifest.json", `{"dependencies":{"com.test.native":"1.0.0"}}`)
	writeGoGraphFile(t, project, "Packages/com.test.native/package.json", `{"name":"com.test.native","version":"1.0.0"}`)
	definition := writeGoGraphFile(t, project, "Packages/com.test.native/Native.asmdef", `{"name":"Native","references":["GUID:ffffffffffffffffffffffffffffffff"]}`)
	writeGoGraphFile(t, project, "Packages/com.test.native/Native.asmdef.meta", "guid: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n")
	source := writeGoGraphFile(t, project, "Packages/com.test.native/Native.cs", "public class Native {}")
	meta := writeGoGraphFile(t, project, "Packages/com.test.native/Native.cs.meta", "guid: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n")
	snapshot, err := packages.Resolve(context.Background(), project, "", packages.Options{Offline: true})
	if err != nil {
		t.Fatal(err)
	}
	profile := &goSourceEngineProfile{SchemaVersion: 1, UnityVersion: fixtureUnityVersion, UnityRevision: "45d8eee7de74", Target: "LinuxStandalone64", ScriptingBackend: "Mono", BaseDefines: []string{"ENABLE_MONO", "UNITY_STANDALONE_LINUX"}, NativeAssemblies: map[string]goSourceNativeAssembly{}}
	sourceHash, _ := goFileHash(source)
	metaHash, _ := goFileHash(meta)
	profile.NativeAssemblies["Native"] = goSourceNativeAssembly{Name: "Native", Package: "com.test.native", PackageContentHash: snapshot.Packages[0].ContentHash, UnresolvedReferences: []string{"GUID:ffffffffffffffffffffffffffffffff"}, Configuration: goAssemblyConfig{Defines: profile.BaseDefines}, SourceHashes: map[string]string{"Packages/com.test.native/Native.cs": sourceHash, "Packages/com.test.native/Native.cs.meta": metaHash}}
	profile.ProfileSHA256, _ = goSourceProfileHash(*profile)
	options := goSourceGraphOptions{Project: project, Profile: profile, Packages: snapshot}
	if _, err := buildGoSourceGraph(context.Background(), options); err != nil {
		t.Fatal(err)
	}
	writeGoGraphFile(t, project, "Packages/com.test.native/Native.cs", "public class Native { public int changed; }")
	options.Packages, err = packages.Resolve(context.Background(), project, "", packages.Options{Offline: true})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := buildGoSourceGraph(context.Background(), options); err == nil {
		t.Fatal("modified package inherited absent reference exception")
	}
	data, _ := os.ReadFile(definition)
	var parsed goSourceAsmdef
	json.Unmarshal(data, &parsed)
	if goSourceTestAssembly(&goSourceAsmdef{Name: "Ordinary", DefineConstraints: []string{"DEBUG"}}) {
		t.Fatal("ordinary development assembly treated as test")
	}
	if !goSourceTestAssembly(&goSourceAsmdef{Name: "Performance", PrecompiledReferences: []string{"nunit.framework.dll"}}) {
		t.Fatal("explicit test dependency included in Player profile")
	}
}

func TestGoSourceSemverPrereleaseNumericParts(t *testing.T) {
	if matched, err := matchGoSourceVersion("1.0.0-preview.10", "1.0.0-preview.2"); err != nil || !matched {
		t.Fatalf("numeric prerelease precedence: %v %v", matched, err)
	}
}
