package main

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

func writeGoGraphFile(t *testing.T, project, relative, contents string) string {
	t.Helper()
	filename := filepath.Join(project, relative)
	if err := os.MkdirAll(filepath.Dir(filename), 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filename, []byte(contents), 0600); err != nil {
		t.Fatal(err)
	}
	return filename
}

func TestGoCompilerPreparationProjectVersionUsesExactLines(t *testing.T) {
	for _, test := range []struct {
		name, text string
		valid      bool
	}{
		{"lf", "m_EditorVersion: " + fixtureUnityVersion + "\n", true},
		{"crlf", "m_EditorVersion: " + fixtureUnityVersion + "\r\n", true},
		{"no-final-newline", "m_EditorVersion: " + fixtureUnityVersion, true},
		{"wrong", "m_EditorVersion: 6000.6.4f1\n", false},
		{"suffix", "m_EditorVersion: " + fixtureUnityVersion + "-suffix\n", false},
		{"duplicate", "m_EditorVersion: " + fixtureUnityVersion + "\nm_EditorVersion: " + fixtureUnityVersion + "\n", false},
	} {
		t.Run(test.name, func(t *testing.T) {
			project := t.TempDir()
			writeGoGraphFile(t, project, "ProjectSettings/ProjectVersion.txt", test.text)
			_, err := prepareGoCompilerGraph(context.Background(), project, "unused-editor", "unused-player", filepath.Join(project, "missing-graph.json"), filepath.Join(project, "Temp/compiler.json"))
			if test.valid && !errors.Is(err, os.ErrNotExist) || !test.valid && (err == nil || !strings.Contains(err.Error(), "verified Unity")) {
				t.Fatalf("project version preflight validity=%v err=%v", test.valid, err)
			}
			if _, err := os.Stat(filepath.Join(project, "Temp")); !errors.Is(err, os.ErrNotExist) {
				t.Fatal("compiler preflight failure mutated the output directory")
			}
		})
	}
}

func TestGoCompilerNativeBasesRequireVerifiedDirectPackageDependency(t *testing.T) {
	project := t.TempDir()
	managed := filepath.Join(project, "Managed")
	host := writeGoGraphFile(t, project, "Managed/Package.Runtime.dll", "verified native DLL fixture")
	hash, _ := goFileHash(host)
	compiler := &goCompiler{manifest: goCompileManifest{CompilerInputs: map[string]string{host: hash}, Scripts: []goCompileScript{{Assembly: "Package.Runtime", HostProvided: true}}},
		configs: map[string]goAssemblyConfig{"Game": {Name: "Game", Dependencies: []string{"Package.Runtime"}}, "Other": {Name: "Other"}}, configurationHashes: map[string]string{}}
	if err := compiler.approvePreparedNativeBases(managed); err != nil || !reflect.DeepEqual(compiler.configs["Game"].NativeBase, []string{"Package.Runtime"}) || len(compiler.configs["Other"].NativeBase) != 0 {
		t.Fatalf("verified native bases were not limited to direct dependencies: %+v %v", compiler.configs, err)
	}
	config := compiler.configs["Other"]
	config.NativeBase = []string{"Package.Runtime"}
	compiler.configs["Other"] = config
	if err := compiler.approvePreparedNativeBases(managed); err == nil {
		t.Fatal("native base outside explicit assembly dependencies was approved")
	}
	compiler.configs["Other"] = goAssemblyConfig{Name: "Other"}
	if err := os.WriteFile(host, []byte("changed native DLL"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := compiler.approvePreparedNativeBases(managed); err == nil {
		t.Fatal("native base with changed deployment digest was approved")
	}
}

func TestGoCompilerNativeOnlyManifestRequiresExplicitProof(t *testing.T) {
	for _, test := range []struct {
		name     string
		manifest goCompileManifest
		valid    bool
	}{
		{"empty-unverified", goCompileManifest{Schema: 1, CompilerInputs: map[string]string{"input": "digest"}}, false},
		{"missing-slots", goCompileManifest{Schema: 1, NativeOnly: true, CompilerInputs: map[string]string{"input": "digest"}}, false},
		{"gameplay-script", goCompileManifest{Schema: 1, NativeOnly: true, NativeSlotsSHA: strings.Repeat("a", 64), CompilerInputs: map[string]string{"input": "digest"}, Scripts: []goCompileScript{{Assembly: "Game"}}}, false},
		{"contradictory-game-assembly", goCompileManifest{Schema: 1, NativeOnly: true, NativeSlotsSHA: strings.Repeat("a", 64), CompilerInputs: map[string]string{"input": "digest"}, Assemblies: []goCompileAssembly{{Name: "Game"}}}, false},
		{"native-only", goCompileManifest{Schema: 1, NativeOnly: true, NativeSlotsSHA: strings.Repeat("a", 64), CompilerInputs: map[string]string{"input": "digest"}, Scripts: []goCompileScript{{Assembly: "Package.Runtime", HostProvided: true}}}, true},
	} {
		t.Run(test.name, func(t *testing.T) {
			filename := filepath.Join(t.TempDir(), "compile.json")
			if err := atomicJSON(filename, test.manifest); err != nil {
				t.Fatal(err)
			}
			_, _, err := goCompilerManifest(filename)
			if (err == nil) != test.valid {
				t.Fatalf("manifest validity=%v err=%v", test.valid, err)
			}
		})
	}
}

func TestGoCompilerNativeManagedProofChecksActiveCopy(t *testing.T) {
	project := t.TempDir()
	original := writeGoGraphFile(t, project, "baseline/Host_Data/Managed/Package.Runtime.dll", "native package")
	active := writeGoGraphFile(t, project, "active/Host_Data/Managed/Package.Runtime.dll", "native package")
	hash, _ := goFileHash(original)
	compiler := &goCompiler{manifest: goCompileManifest{CompilerInputs: map[string]string{
		original: hash,
		filepath.Join(project, "baseline/Host_Data/Managed/UnityMicroEditor.dll"): "bridge SDK refresh permitted",
		filepath.Join(project, "baseline/Host_Data/Managed/Game.dll"):             "game proof checked separately",
	}}}
	managed := filepath.Dir(active)
	if err := compiler.verifyStagedNativeManagedInputs(managed, map[string]bool{"Game": true}); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(active, []byte("changed active copy"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := compiler.verifyStagedNativeManagedInputs(managed, map[string]bool{"Game": true}); err == nil {
		t.Fatal("changed active package DLL was concealed by an intact original baseline cache")
	}
	if err := os.Remove(active); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(original, active); err != nil {
		t.Fatal(err)
	}
	if err := compiler.verifyStagedNativeManagedInputs(managed, map[string]bool{"Game": true}); err == nil || !strings.Contains(err.Error(), "symlink") {
		t.Fatalf("native package symlink accepted: %v", err)
	}
}

func sourceExtensionFixture(t *testing.T) *goCompiler {
	t.Helper()
	project := t.TempDir()
	source := writeGoGraphFile(t, project, "Assets/Game/Known.cs", "class Known {}")
	meta := writeGoGraphFile(t, project, "Assets/Game/Known.cs.meta", "guid: 10000000000000000000000000000001\nMonoImporter:\n  executionOrder: 0\n")
	definition := writeGoGraphFile(t, project, "Assets/Game/Game.asmdef", `{"name":"Game"}`)
	definitionMeta := writeGoGraphFile(t, project, "Assets/Game/Game.asmdef.meta", "guid: 20000000000000000000000000000001\n")
	inputs := map[string]string{}
	for _, path := range []string{source, meta, definition, definitionMeta} {
		inputs[path], _ = goFileHash(path)
	}
	compiler := &goCompiler{project: project, sources: map[string]string{source: inputs[source]}, configuration: map[string]string{meta: inputs[meta]},
		configs: map[string]goAssemblyConfig{"Game": {Name: "Game", Sources: []string{source}}}, currentDLLs: map[string]string{"Game": "Game.dll"},
		manifest: goCompileManifest{CompilerInputs: inputs, Assemblies: []goCompileAssembly{{Name: "Game", Sources: []string{source}}},
			Scripts: []goCompileScript{{GUID: "10000000000000000000000000000001", Path: source, Assembly: "Game"}}}}
	compiler.collectConfigurationInputs()
	return compiler
}

func TestGoSourceGraphExtendsKnownAssemblyWithoutChangingAcceptedState(t *testing.T) {
	compiler := sourceExtensionFixture(t)
	added := writeGoGraphFile(t, compiler.project, "Assets/Game/New/Nested.cs", "namespace A { class Nested {} }")
	meta := writeGoGraphFile(t, compiler.project, "Assets/Game/New/Nested.cs.meta", "guid: 10000000000000000000000000000002\nMonoImporter:\n  executionOrder: 11\n")
	writeGoGraphFile(t, compiler.project, "Assets/Game/New.meta", "guid: 30000000000000000000000000000001\nfolderAsset: yes\n")
	before, _ := json.Marshal(compiler.manifest)
	candidate, err := compiler.sourceGraphCandidate([]string{added, meta})
	if err != nil {
		t.Fatal(err)
	}
	if candidate.graphSourceAssemblies()[added] != "Game" || len(candidate.manifest.Scripts) != 2 || candidate.configuration[meta] == "" ||
		len(candidate.configs["Game"].Sources) != 2 {
		t.Fatalf("new authored source/GUID was not added to the exact existing assembly: %+v", candidate.manifest)
	}
	after, _ := json.Marshal(compiler.manifest)
	if string(before) != string(after) || compiler.configuration[meta] != "" || len(compiler.configs["Game"].Sources) != 1 {
		t.Fatal("candidate discovery mutated accepted compiler graph")
	}
	if err := candidate.checkConfiguration(nil); err != nil {
		t.Fatal(err)
	}
	writeGoGraphFile(t, compiler.project, "Assets/Game/AppearedDuringCompile.cs", "class AppearedDuringCompile {}")
	if err := candidate.checkConfiguration(nil); err == nil {
		t.Fatal("source appearing after snapshot could be omitted from a published Player")
	}
}

func TestGoSourceGraphUsesGUIDAssemblyReferenceAndDefaultScopes(t *testing.T) {
	compiler := sourceExtensionFixture(t)
	reference := writeGoGraphFile(t, compiler.project, "Assets/Shared/Shared.asmref", `{"reference":"GUID:20000000000000000000000000000001"}`)
	meta := writeGoGraphFile(t, compiler.project, "Assets/Shared/Shared.asmref.meta", "guid: 20000000000000000000000000000002\n")
	for _, path := range []string{reference, meta} {
		compiler.configuration[path], _ = goFileHash(path)
		compiler.manifest.CompilerInputs[path] = compiler.configuration[path]
	}
	added := writeGoGraphFile(t, compiler.project, "Assets/Shared/Added.cs", "class Added {}")
	writeGoGraphFile(t, compiler.project, "Assets/Shared/Added.cs.meta", "guid: 10000000000000000000000000000002\n")
	candidate, err := compiler.sourceGraphCandidate(nil)
	if err != nil || candidate.graphSourceAssemblies()[added] != "Game" {
		t.Fatalf("GUID asmref did not resolve the nearest exact prepared assembly: %+v %v", candidate, err)
	}
	compiler.configs["Assembly-CSharp"] = goAssemblyConfig{Name: "Assembly-CSharp"}
	compiler.configs["Assembly-CSharp-firstpass"] = goAssemblyConfig{Name: "Assembly-CSharp-firstpass"}
	compiler.currentDLLs["Assembly-CSharp"], compiler.currentDLLs["Assembly-CSharp-firstpass"] = "default.dll", "firstpass.dll"
	roots, err := compiler.assemblySourceRoots()
	if err != nil {
		t.Fatal(err)
	}
	for path, expected := range map[string]string{"Assets/Other/Default.cs": "Assembly-CSharp", "Assets/Plugins/Plugin.cs": "Assembly-CSharp-firstpass", "Assets/Game/Sub/Game.cs": "Game"} {
		if actual, err := compiler.sourceAssembly(filepath.Join(compiler.project, path), roots); err != nil || actual != expected {
			t.Fatalf("source scope %s: %s %v", path, actual, err)
		}
	}
}

func TestGoSourceGraphRejectsUnsupportedOrConflictingAdditions(t *testing.T) {
	for _, kind := range []string{"missing-meta", "duplicate-script-guid", "duplicate-asset-guid", "new-asmdef", "changed-asmdef", "missing-slot", "symlink", "package-source"} {
		t.Run(kind, func(t *testing.T) {
			compiler := sourceExtensionFixture(t)
			source := writeGoGraphFile(t, compiler.project, "Assets/Game/Added.cs", "class Added {}")
			writeGoGraphFile(t, compiler.project, "Assets/Game/Added.cs.meta", "guid: 10000000000000000000000000000002\n")
			changed := []string{source}
			switch kind {
			case "missing-meta":
				if err := os.Remove(source + ".meta"); err != nil {
					t.Fatal(err)
				}
			case "duplicate-script-guid":
				writeGoGraphFile(t, compiler.project, "Assets/Game/Added.cs.meta", "guid: 10000000000000000000000000000001\n")
			case "duplicate-asset-guid":
				writeGoGraphFile(t, compiler.project, "Assets/Scene.unity.meta", "guid: 10000000000000000000000000000002\n")
			case "new-asmdef":
				writeGoGraphFile(t, compiler.project, "Assets/Game/Sub/New.asmdef", `{"name":"New"}`)
			case "changed-asmdef":
				writeGoGraphFile(t, compiler.project, "Assets/Game/Game.asmdef", `{"name":"Changed"}`)
			case "missing-slot":
				delete(compiler.currentDLLs, "Game")
			case "symlink":
				if err := os.Remove(source); err != nil {
					t.Fatal(err)
				}
				if err := os.Symlink(filepath.Join(compiler.project, "Assets/Game/Known.cs"), source); err != nil {
					t.Fatal(err)
				}
			case "package-source":
				changed = append(changed, "Packages/com.example/Added.cs")
			}
			if candidate, err := compiler.sourceGraphCandidate(changed); err == nil || candidate != nil {
				t.Fatalf("unsupported %s produced a graph: %+v %v", kind, candidate, err)
			}
		})
	}
}

func TestGoCompilerAssemblySlotExtensionPreservesHostFlags(t *testing.T) {
	filename := writeGoGraphFile(t, t.TempDir(), "ScriptingAssemblies.json", `{"names":["UnityEngine.dll","Bridge.dll"],"types":[2,16],"hostFlag":{"preserve":true}}`)
	if err := registerGoCompilerAssemblySlots(filename, []goCompileAssembly{{Name: "Game"}, {Name: "Bridge"}}); err != nil {
		t.Fatal(err)
	}
	var doc map[string]json.RawMessage
	if err := readJSONFile(filename, &doc); err != nil {
		t.Fatal(err)
	}
	var names []string
	var types []int
	json.Unmarshal(doc["names"], &names)
	json.Unmarshal(doc["types"], &types)
	if !reflect.DeepEqual(names, []string{"UnityEngine.dll", "Bridge.dll", "Game.dll"}) || !reflect.DeepEqual(types, []int{2, 16, 16}) || string(doc["hostFlag"]) != `{"preserve":true}` {
		t.Fatalf("host native assembly flags were overwritten: %s", doc)
	}
}

func TestGoCompilerPlayerSourceDeclarationsSDK(t *testing.T) {
	editor := os.Getenv("UNITY_GO_LIFECYCLE_SDK")
	if editor == "" {
		t.Skip("set the pinned Unity SDK for actual C# Player preprocessing proof")
	}
	compiler := &goCompiler{editor: editor, cache: t.TempDir()}
	if err := compiler.initializeSDK(); err != nil {
		t.Fatal(err)
	}
	helper, err := compiler.prepareLifecycleHelper(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	source := writeGoGraphFile(t, compiler.cache, "Conditional.cs", `namespace Package.@event;
#if UNITY_EDITOR
internal class DownloadableSample {}
#elif DEVELOPMENT_BUILD && FEATURE
internal class RuntimeDebug {}
#else
internal class ReleaseRuntime {}
#endif
internal class Owner<T> { internal class Inner {} }
`)
	request := filepath.Join(compiler.cache, "declarations.json")
	if err := atomicJSON(request, []any{map[string]any{"assemblyName": "Package.Runtime", "defines": []string{"DEVELOPMENT_BUILD", "FEATURE"}, "editorDefines": []string{"UNITY_EDITOR"},
		"sources": []any{map[string]string{"path": source, "originalPath": source}}}}); err != nil {
		t.Fatal(err)
	}
	output, err := goCompilerCommand(context.Background(), compiler.cache, compiler.dotnet, helper, "--source-type-declarations", request)
	if err != nil {
		t.Fatal(err)
	}
	var proof map[string]struct {
		Player map[string][]string `json:"player"`
		Editor map[string][]string `json:"editor"`
	}
	if err := json.Unmarshal(output, &proof); err != nil {
		t.Fatal(err)
	}
	common := []string{"Package.event.Owner`1", "Package.event.Owner`1+Inner"}
	if names := proof["Package.Runtime"].Player[source]; !reflect.DeepEqual(names, append(append([]string{}, common...), "Package.event.RuntimeDebug")) {
		t.Fatalf("Player preprocessing/nested/file-scoped namespace proof incorrect: %v", names)
	}
	if names := proof["Package.Runtime"].Editor[source]; !reflect.DeepEqual(names, append([]string{"Package.event.DownloadableSample"}, common...)) {
		t.Fatalf("Editor conditional declaration proof incorrect: %v", names)
	}
}

func TestGoCompilerNewMonoBehaviourAndRestartSDK(t *testing.T) {
	editor := os.Getenv("UNITY_GO_LIFECYCLE_SDK")
	if editor == "" {
		t.Skip("set UNITY_GO_LIFECYCLE_SDK to the matching Unity executable; SDK only, no Editor or Player starts")
	}
	project := t.TempDir()
	writeGoGraphFile(t, project, "ProjectSettings/ProjectVersion.txt", "m_EditorVersion: "+fixtureUnityVersion+"\n")
	writeGoGraphFile(t, project, "Packages/manifest.json", `{"dependencies":{}}`)
	compiler := &goCompiler{editor: editor}
	if err := compiler.initializeSDK(); err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()
	compile := func(dll, source string, extraRefs ...string) {
		t.Helper()
		filename := writeGoGraphFile(t, project, "Temp/initial/"+filepath.Base(dll)+".cs", source)
		if err := os.MkdirAll(filepath.Dir(dll), 0700); err != nil {
			t.Fatal(err)
		}
		args := []string{compiler.csc, "-nologo", "-target:library", "-nostdlib+", "-debug:portable", "-out:" + dll}
		for _, reference := range deduplicateGoReferences(append(append(compiler.baseReferences, compiler.engineReferences...), extraRefs...)) {
			args = append(args, "-reference:"+reference)
		}
		args = append(args, filename)
		if _, err := goCompilerCommand(ctx, project, compiler.dotnet, args...); err != nil {
			t.Fatal(err)
		}
	}
	player := writeGoGraphFile(t, project, "Temp/native-host/Host.x86_64", "SDK fixture executable, not launched")
	if err := os.Chmod(player, 0700); err != nil {
		t.Fatal(err)
	}
	managed := filepath.Join(filepath.Dir(player), "Host_Data/Managed")
	bridge := filepath.Join(managed, "UnityMicroEditor.dll")
	bridgeText := `using System; using UnityEngine; namespace UnityMicroEditor.Raw {
[AttributeUsage(AttributeTargets.Assembly)] public sealed class RawLifecycleInstrumentedAttribute : Attribute { public RawLifecycleInstrumentedAttribute(int version) {} }
[AttributeUsage(AttributeTargets.Assembly)] public sealed class RawScriptableObjectInstrumentedAttribute : Attribute { public RawScriptableObjectInstrumentedAttribute(int version) {} }
public static class RawLifecycle {
 public enum Phase { Awake, OnEnable, OnDisable, OnDestroy }
 public static bool ShouldCollect(MonoBehaviour target, Phase phase) => false;
 public static bool ShouldCollect(ScriptableObject target, Phase phase) => false;
 public static void Enqueue(MonoBehaviour target, Phase phase, Action callback, int order) {}
 public static void Enqueue(ScriptableObject target, Phase phase, Action callback, int order) {}
}}`
	compile(bridge, bridgeText)
	packageText := `namespace Package.First { public class Known : UnityEngine.MonoBehaviour {} }
namespace Package.Second { [UnityEngine.DefaultExecutionOrder(-5000)] public class Known : UnityEngine.MonoBehaviour {
 protected virtual void Awake() { } protected virtual void OnDestroy() { }
} }`
	packageRoot := filepath.Join(project, "Library/PackageCache/com.test.runtime@fixture")
	packageSource := writeGoGraphFile(t, project, "Library/PackageCache/com.test.runtime@fixture/Known.cs", packageText)
	packageMeta := writeGoGraphFile(t, project, "Library/PackageCache/com.test.runtime@fixture/Known.cs.meta", "guid: 10000000000000000000000000000005\n")
	editorOnlyText := `namespace Package { internal class DownloadableSample : UnityEngine.ScriptableObject {} }`
	editorOnlySource := writeGoGraphFile(t, project, "Library/PackageCache/com.test.runtime@fixture/DownloadableSample.cs", "#if UNITY_EDITOR\n"+editorOnlyText+"\n#endif\n")
	editorOnlyMeta := writeGoGraphFile(t, project, "Library/PackageCache/com.test.runtime@fixture/DownloadableSample.cs.meta", "guid: 10000000000000000000000000000006\n")
	packageHost := filepath.Join(managed, "Package.Runtime.dll")
	packageEditor := filepath.Join(project, "Library/ScriptAssemblies/Package.Runtime.dll")
	compile(packageHost, packageText)
	compile(packageEditor, packageText+editorOnlyText)
	writeGoGraphFile(t, project, "Temp/native-host/Host_Data/ScriptingAssemblies.json", `{"names":["UnityMicroEditor.dll","Package.Runtime.dll"],"types":[16,16],"fixtureFlag":true}`)
	knownText := `namespace Baseline { public class Known : Package.Second.Known { public int seed = 3; } }`
	known := writeGoGraphFile(t, project, "Assets/Known.cs", knownText)
	writeGoGraphFile(t, project, "Assets/Known.cs.meta", "guid: 10000000000000000000000000000001\nMonoImporter:\n  executionOrder: 0\n")
	consumerText := `public class Consumer { public Baseline.Known target; }`
	consumer := writeGoGraphFile(t, project, "Assets/Consumer/Consumer.cs", consumerText)
	writeGoGraphFile(t, project, "Assets/Consumer/Consumer.cs.meta", "guid: 10000000000000000000000000000002\n")
	writeGoGraphFile(t, project, "Assets/Consumer/Consumer.asmdef", `{"name":"Consumer","references":["Assembly-CSharp"]}`)
	writeGoGraphFile(t, project, "Assets/Consumer/Consumer.asmdef.meta", "guid: 20000000000000000000000000000001\n")
	knownDLL, consumerDLL := filepath.Join(project, "Library/ScriptAssemblies/Assembly-CSharp.dll"), filepath.Join(project, "Library/ScriptAssemblies/Consumer.dll")
	compile(knownDLL, knownText, bridge, packageHost)
	compile(consumerDLL, consumerText, knownDLL, bridge, packageHost)
	graph := goCompilerGraphExport{SchemaVersion: 1, Project: project, Unity: editor,
		Assemblies: []goCompilerGraphAssembly{
			{goAssemblyConfig: goAssemblyConfig{Name: "Consumer", Sources: []string{consumer}, Defines: []string{"UNITY_6000_6"}}, Path: consumerDLL, References: []string{"Assembly-CSharp"}},
			{goAssemblyConfig: goAssemblyConfig{Name: "Assembly-CSharp", Sources: []string{known}, Defines: []string{"UNITY_6000_6"}}, Path: knownDLL, References: []string{"UnityMicroEditor", "Package.Runtime"}},
		}, Scripts: []goCompileScript{{GUID: "10000000000000000000000000000001", Path: known, Assembly: "Assembly-CSharp", TypeName: "Baseline.Known"}, {GUID: "10000000000000000000000000000002", Path: consumer, Assembly: "Consumer", TypeName: "Consumer"},
			{GUID: "10000000000000000000000000000006", Path: editorOnlySource, Assembly: "Package.Runtime", TypeName: "Package.DownloadableSample", HostProvided: true},
			{GUID: "10000000000000000000000000000005", Path: packageSource, Assembly: "Package.Runtime", TypeName: "Package.Second.Known", HostProvided: true}},
		SourceRoots: []goCompilerSourceRoot{{Path: "Packages/com.test.runtime", Source: packageRoot}}, CompilerInputs: map[string]string{}}
	for _, path := range []string{packageSource, packageMeta, editorOnlySource, editorOnlyMeta} {
		hash, err := goFileHash(path)
		if err != nil {
			t.Fatal(err)
		}
		graph.CompilerInputs[path] = hash
	}
	packageHash, err := goFileHash(packageEditor)
	if err != nil {
		t.Fatal(err)
	}
	graph.HostAssemblies = []goCompilerHostAssembly{{Name: "Package.Runtime", Path: packageEditor, SHA256: packageHash,
		Sources: []string{packageSource, editorOnlySource}, Defines: []string{"UNITY_6000_6", "DEVELOPMENT_BUILD"}, EditorDefines: []string{"UNITY_EDITOR", "UNITY_EDITOR_LINUX"}}}
	graphFile, baseline := filepath.Join(project, "Temp/export.json"), filepath.Join(project, "Temp/prepared/compile.json")
	if err := atomicJSON(graphFile, graph); err != nil {
		t.Fatal(err)
	}
	// A public Editor export can reference a newer bridge than the prepared
	// native host. Never conceal this deployment drift with the Editor DLL.
	editorBridge := filepath.Join(project, "Library/ScriptAssemblies/UnityMicroEditor.dll")
	api := `namespace UnityMicroEditor { public static class AgentRuntimeCommands { public static int Register() => 2; public sealed class Token {} } internal class EditorDetail { public class Hidden {} } }`
	compile(editorBridge, bridgeText+api)
	oldBridgeHash, err := goFileHash(bridge)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := prepareGoCompilerGraph(ctx, project, editor, player, graphFile, baseline); err == nil || !strings.Contains(err.Error(), "native host runtime bridge is stale") || !strings.Contains(err.Error(), "AgentRuntimeCommands") {
		t.Fatalf("stale Player API was not rejected before instrumentation: %v", err)
	}
	if _, err := os.Lstat(baseline); !os.IsNotExist(err) {
		t.Fatal("stale runtime API published a successful compiler baseline")
	}
	if actual, err := goFileHash(bridge); err != nil || actual != oldBridgeHash {
		t.Fatal("stale host was overwritten with the Editor bridge")
	}
	// Matching public types allow different implementation details and bodies.
	compile(bridge, bridgeText+`namespace UnityMicroEditor { public static class AgentRuntimeCommands { public static int Register() => 1; public sealed class Token {} } }`)
	missingRuntime := `namespace Package { public class EditorOnlyMissing : UnityEngine.MonoBehaviour {} }`
	writeGoGraphFile(t, project, "Library/PackageCache/com.test.runtime@fixture/Known.cs", packageText+missingRuntime)
	compile(packageEditor, packageText+editorOnlyText+missingRuntime)
	graph.CompilerInputs[packageSource], _ = goFileHash(packageSource)
	graph.HostAssemblies[0].SHA256, _ = goFileHash(packageEditor)
	graph.Scripts[len(graph.Scripts)-1].TypeName = "Package.EditorOnlyMissing"
	if err := atomicJSON(graphFile, graph); err != nil {
		t.Fatal(err)
	}
	if _, err := prepareGoCompilerGraph(ctx, project, editor, player, graphFile, baseline); err == nil || !strings.Contains(err.Error(), "lacks exported MonoScript type Package.EditorOnlyMissing") {
		t.Fatalf("missing native package type was accepted or concealed with Editor DLL: %v", err)
	}
	writeGoGraphFile(t, project, "Library/PackageCache/com.test.runtime@fixture/Known.cs", packageText)
	compile(packageEditor, packageText+editorOnlyText)
	graph.CompilerInputs[packageSource], _ = goFileHash(packageSource)
	graph.HostAssemblies[0].SHA256, _ = goFileHash(packageEditor)
	graph.Scripts[len(graph.Scripts)-1].TypeName = "Package.Second.Known"
	// A verified native package does not make its private lifecycle messages
	// callable. Never bypass this boundary with reflection or a lost callback.
	privatePackage := strings.ReplaceAll(packageText, "protected virtual void", "private void")
	writeGoGraphFile(t, project, "Library/PackageCache/com.test.runtime@fixture/Known.cs", privatePackage)
	compile(packageHost, privatePackage)
	compile(packageEditor, privatePackage+editorOnlyText)
	graph.CompilerInputs[packageSource], _ = goFileHash(packageSource)
	graph.HostAssemblies[0].SHA256, _ = goFileHash(packageEditor)
	if err := atomicJSON(graphFile, graph); err != nil {
		t.Fatal(err)
	}
	if _, err := prepareGoCompilerGraph(ctx, project, editor, player, graphFile, baseline); err == nil || !strings.Contains(err.Error(), "inaccessible; a public/protected bridge is required") {
		t.Fatalf("private native callback was accepted or silently dropped: %v", err)
	}
	writeGoGraphFile(t, project, "Library/PackageCache/com.test.runtime@fixture/Known.cs", packageText)
	compile(packageHost, packageText)
	compile(packageEditor, packageText+editorOnlyText)
	graph.CompilerInputs[packageSource], _ = goFileHash(packageSource)
	graph.HostAssemblies[0].SHA256, _ = goFileHash(packageEditor)
	if err := atomicJSON(graphFile, graph); err != nil {
		t.Fatal(err)
	}
	prepared, err := prepareGoCompilerGraph(ctx, project, editor, player, graphFile, baseline)
	if err != nil {
		t.Fatal(err)
	}
	if prepared.Compiler.Assemblies[0].Name != "Assembly-CSharp" {
		t.Fatal("exported dependency graph was not ordered")
	}
	for _, script := range prepared.Compiler.Scripts {
		if script.Path == editorOnlySource || script.TypeName == "Package.DownloadableSample" {
			t.Fatal("inactive UNITY_EDITOR-only package ScriptableObject survived Player preprocessing")
		}
	}
	if prepared.Compiler.CompilerInputs[editorOnlySource] == "" || prepared.Compiler.CompilerInputs[editorOnlyMeta] == "" {
		t.Fatal("inactive Editor script lost its configuration drift fingerprints")
	}
	baseline = filepath.Join(project, "Temp/scene-prepared/current.json")
	if err := os.MkdirAll(filepath.Dir(baseline), 0700); err != nil {
		t.Fatal(err)
	}
	if err := atomicJSON(baseline, prepared); err != nil {
		t.Fatal(err)
	}
	cache := filepath.Join(project, "Temp/revisions")
	accepted, err := newGoCompiler(project, baseline, prepared.Player, editor, cache)
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(accepted.configs["Assembly-CSharp"].NativeBase, []string{"Package.Runtime"}) {
		t.Fatal("restart lost the verified native package base allowlist")
	}
	for _, phase := range []string{"Awake", "OnDestroy"} {
		covered := false
		for _, callback := range accepted.proofs["Assembly-CSharp"].Methods {
			if callback.Type == "Baseline.Known" && callback.Phase == phase && callback.Synthetic && callback.Order == -5000 {
				covered = true
			}
		}
		if !covered {
			t.Fatalf("protected inherited native %s lacks delayed source-owned wrapper proof", phase)
		}
	}
	if types, err := accepted.ScriptTypeNames(ctx); err != nil || types["10000000000000000000000000000005"] != "Package.Second.Known" {
		t.Fatalf("public package GUID/type descriptor lost with ambiguous filename: %v %v", types, err)
	}
	if update, err := accepted.Compile(ctx, nil); err != nil || update != nil {
		t.Fatalf("fresh prepared graph recompiled: %+v %v", update, err)
	}
	newGUID := "10000000000000000000000000000003"
	added := writeGoGraphFile(t, project, "Assets/NewNamespace/Known.cs", `namespace AgentAdded { public class Known : UnityEngine.MonoBehaviour { public int revision = 7; public string label = "agent"; void Awake() { revision++; } } }`)
	writeGoGraphFile(t, project, "Assets/NewNamespace/Known.cs.meta", "guid: "+newGUID+"\nMonoImporter:\n  executionOrder: 12\n")
	update, err := accepted.Compile(ctx, []string{added, added + ".meta"})
	if err != nil {
		t.Fatal(err)
	}
	if update == nil || !reflect.DeepEqual(update.CompiledAssemblies, []string{"Assembly-CSharp", "Consumer"}) || len(update.SourceHashes) != 3 {
		t.Fatalf("new API did not rebuild assembly and consumer: %+v", update)
	}
	types, err := accepted.ScriptTypeNames(ctx)
	if err != nil || types[newGUID] != "AgentAdded.Known" || types["10000000000000000000000000000001"] != "Baseline.Known" {
		t.Fatalf("exact per-source GUID/type binding lost with same class filename in two namespaces: %v %v", types, err)
	}
	proof := accepted.proofs["Assembly-CSharp"]
	covered := false
	for _, callback := range proof.Methods {
		if callback.Source == added && callback.Type == "AgentAdded.Known" && callback.Phase == "Awake" && callback.Order == 12 {
			covered = true
		}
	}
	if !covered {
		t.Fatalf("new authored MonoBehaviour lacks lifecycle/execution-order proof: %+v", proof)
	}
	// Publishing a new baseline must carry the accepted DLL SHA, expanded
	// source set, GUID/type binding and proof rather than the initial plan.
	rebased, err := accepted.PreparedManifest(ctx)
	if err != nil {
		t.Fatal(err)
	}
	rebasedFile := filepath.Join(project, "Temp/rebased/current.json")
	if err := os.MkdirAll(filepath.Dir(rebasedFile), 0700); err != nil {
		t.Fatal(err)
	}
	if err := atomicJSON(rebasedFile, goCompilerPreparationResult{SchemaVersion: 1, Player: update.Player, Compiler: rebased}); err != nil {
		t.Fatal(err)
	}
	rebasedCompiler, err := newGoCompiler(project, rebasedFile, update.Player, editor, filepath.Join(project, "Temp/rebased-revisions"))
	if err != nil {
		t.Fatalf("published accepted compiler baseline cannot restart: %v", err)
	}
	if redundant, err := rebasedCompiler.Compile(ctx, nil); err != nil || redundant != nil {
		t.Fatalf("published accepted sources unexpectedly recompiled: %+v %v", redundant, err)
	}
	if bound, err := rebasedCompiler.ScriptTypeNames(ctx); err != nil || bound[newGUID] != "AgentAdded.Known" {
		t.Fatalf("published compiler baseline lost added GUID/type: %v %v", bound, err)
	}
	acceptedDLL := accepted.currentDLLs["Assembly-CSharp"]
	untampered, err := os.ReadFile(acceptedDLL)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(acceptedDLL, append(append([]byte{}, untampered...), 0), 0600); err != nil {
		t.Fatal(err)
	}
	if _, err := accepted.PreparedManifest(ctx); err == nil || !strings.Contains(err.Error(), "accepted DLL changed") {
		t.Fatal("changed accepted DLL was silently rebound into a prepared baseline", err)
	}
	if err := os.WriteFile(acceptedDLL, untampered, 0600); err != nil {
		t.Fatal(err)
	}
	restarted, err := newGoCompiler(project, baseline, prepared.Player, editor, cache)
	if err != nil {
		t.Fatal(err)
	}
	reused, err := restarted.Compile(ctx, nil)
	if err != nil || reused == nil || !reused.CacheReused || reused.Player != update.Player || len(reused.CompiledAssemblies) != 0 {
		t.Fatalf("restart lost verified extended source graph: %+v %v", reused, err)
	}
	if types, err := restarted.ScriptTypeNames(ctx); err != nil || types[newGUID] != "AgentAdded.Known" {
		t.Fatalf("restart lost added script GUID: %v %v", types, err)
	}
	oldPlayer, oldScripts := restarted.player, len(restarted.manifest.Scripts)
	broken := writeGoGraphFile(t, project, "Assets/Broken.cs", "public class Broken { invalid C sharp }")
	writeGoGraphFile(t, project, "Assets/Broken.cs.meta", "guid: 10000000000000000000000000000004\n")
	if rejected, err := restarted.Compile(ctx, []string{broken}); err == nil || rejected != nil {
		t.Fatalf("broken new C# published: %+v %v", rejected, err)
	}
	if restarted.player != oldPlayer || len(restarted.manifest.Scripts) != oldScripts || restarted.graphSourceAssemblies()[broken] != "" {
		t.Fatal("failed extension replaced accepted Player or source graph")
	}
	if err := os.Remove(broken); err != nil {
		t.Fatal(err)
	}
	if err := os.Remove(broken + ".meta"); err != nil {
		t.Fatal(err)
	}
	writeGoGraphFile(t, project, "Assets/NewNamespace/Known.cs", strings.Replace(`namespace AgentAdded { public class Known : UnityEngine.MonoBehaviour { public int revision = 7; public string label = "agent"; void Awake() { revision++; } } }`, "AgentAdded", "RenamedNamespace", 1))
	if rejected, err := restarted.Compile(ctx, []string{added}); err == nil || rejected != nil || !strings.Contains(err.Error(), "removed or renamed") {
		t.Fatalf("accepted new script lost serialized type identity: %+v %v", rejected, err)
	}
	if restarted.player != oldPlayer {
		t.Fatal("renamed serialized type replaced accepted Player")
	}
	t.Run("native-only-fresh-and-restart", func(t *testing.T) {
		if err := os.Rename(filepath.Join(project, "Assets"), filepath.Join(project, "Temp/SavedGameAssets")); err != nil {
			t.Fatal(err)
		}
		if err := os.Mkdir(filepath.Join(project, "Assets"), 0700); err != nil {
			t.Fatal(err)
		}
		graph.Assemblies = nil
		var nativeScripts []goCompileScript
		for _, script := range graph.Scripts {
			if script.HostProvided {
				nativeScripts = append(nativeScripts, script)
			}
		}
		graph.Scripts = nativeScripts
		if err := atomicJSON(graphFile, graph); err != nil {
			t.Fatal(err)
		}
		nativeBaseline := filepath.Join(project, "Temp/native-prepared/compile.json")
		nativePrepared, err := prepareGoCompilerGraph(ctx, project, editor, player, graphFile, nativeBaseline)
		if err != nil {
			t.Fatal(err)
		}
		if !nativePrepared.Compiler.NativeOnly || len(nativePrepared.Compiler.Assemblies) != 0 || !validSHA256(nativePrepared.Compiler.NativeSlotsSHA) {
			t.Fatalf("empty game graph lost verified native-only provenance: %+v", nativePrepared.Compiler)
		}
		nativeCompiler, err := newGoCompiler(project, nativeBaseline, nativePrepared.Player, editor, filepath.Join(project, "Temp/native-revisions"))
		if err != nil {
			t.Fatal(err)
		}
		if update, err := nativeCompiler.Compile(ctx, nil); err != nil || update != nil {
			t.Fatalf("fresh native-only graph recompiled: %+v %v", update, err)
		}
		if types, err := nativeCompiler.ScriptTypeNames(ctx); err != nil || types["10000000000000000000000000000005"] != "Package.Second.Known" {
			t.Fatalf("native-only package GUID/type identity lost: %v %v", types, err)
		}
		nativeCompiler.manifest.Scripts = nil
		if types, err := nativeCompiler.ScriptTypeNames(ctx); err != nil || len(types) != 0 {
			t.Fatalf("empty native-only type inventory failed: %v %v", types, err)
		}
		first := writeGoGraphFile(t, project, "Assets/First.cs", "public class First : UnityEngine.MonoBehaviour {}")
		writeGoGraphFile(t, project, "Assets/First.cs.meta", "guid: 10000000000000000000000000000007\n")
		if update, err := nativeCompiler.Compile(ctx, []string{first}); err == nil || update != nil {
			t.Fatal("first gameplay script silently created an unprepared native assembly slot")
		}
		if err := os.Remove(first); err != nil {
			t.Fatal(err)
		}
		if err := os.Remove(first + ".meta"); err != nil {
			t.Fatal(err)
		}
		slots := filepath.Join(filepath.Dir(nativePrepared.Player), "Host_Data/ScriptingAssemblies.json")
		if err := os.WriteFile(slots, []byte(`{"names":["UnityMicroEditor.dll"],"types":[16]}`), 0600); err != nil {
			t.Fatal(err)
		}
		if _, err := newGoCompiler(project, nativeBaseline, nativePrepared.Player, editor, filepath.Join(project, "Temp/native-revisions")); err == nil || !strings.Contains(err.Error(), "native-only host registrations differ") {
			t.Fatalf("native-only registration drift accepted at restart: %v", err)
		}
	})
	t.Logf("SDK-only added MonoBehaviour GUID=%s type=AgentAdded.Known, lifecycle order=12, consumer rebuilt, fresh-process revision reused", newGUID)
}
