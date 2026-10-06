package main

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestGoCompilerConfigurationDriftAndNewSourcesAreRejected(t *testing.T) {
	project := t.TempDir()
	if err := os.MkdirAll(filepath.Join(project, "Assets/Runtime"), 0700); err != nil {
		t.Fatal(err)
	}
	source := filepath.Join(project, "Assets/Runtime/Script.cs")
	meta := source + ".meta"
	if err := os.WriteFile(source, []byte("class Script {}"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(meta, []byte("guid: b1d15452ab70b6a669e9b8e3c0b91251\n"), 0600); err != nil {
		t.Fatal(err)
	}
	hash, _ := goFileHash(meta)
	compiler := &goCompiler{project: project, sources: map[string]string{source: "hash"}, configuration: map[string]string{meta: hash},
		manifest: goCompileManifest{CompilerInputs: map[string]string{source: "hash"}}}
	if err := compiler.checkConfiguration([]string{source}); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(project, "Assets/Runtime/NewScript.cs"), []byte("class NewScript {}"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := compiler.checkConfiguration(nil); err == nil {
		t.Fatal("new runtime source was silently omitted from old DLL")
	}
	if err := os.Remove(filepath.Join(project, "Assets/Runtime/NewScript.cs")); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(meta, []byte("guid: c1d15452ab70b6a669e9b8e3c0b91251\n"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := compiler.checkConfiguration(nil); err == nil {
		t.Fatal("MonoScript GUID change accepted against stale binding graph")
	}
}

func TestGoCompilerRejectsChangedDLLAndImporterMetadataBeforeSourceReuse(t *testing.T) {
	project, sdk := t.TempDir(), t.TempDir()
	if err := os.MkdirAll(filepath.Join(project, "Assets/Plugins"), 0700); err != nil {
		t.Fatal(err)
	}
	plugin := filepath.Join(project, "Assets/Plugins/Plugin.dll")
	meta := plugin + ".meta"
	external := filepath.Join(sdk, "ImmutableHost.dll")
	inputs := map[string]string{}
	for path, contents := range map[string]string{plugin: "original plugin", meta: "original importer flags", external: "original SDK/host reference"} {
		if err := os.WriteFile(path, []byte(contents), 0600); err != nil {
			t.Fatal(err)
		}
		inputs[path], _ = goFileHash(path)
	}
	compiler := &goCompiler{project: project, configuration: map[string]string{}, manifest: goCompileManifest{CompilerInputs: inputs}}
	compiler.collectConfigurationInputs()
	if err := compiler.checkConfiguration(nil); err != nil {
		t.Fatal(err)
	}
	for _, path := range []string{plugin, meta, external} {
		t.Run(filepath.Base(path), func(t *testing.T) {
			original, _ := os.ReadFile(path)
			if err := os.WriteFile(path, []byte("changed binary reference/import settings"), 0600); err != nil {
				t.Fatal(err)
			}
			if update, err := compiler.Compile(context.Background(), []string{path}); err == nil || update != nil || !strings.Contains(err.Error(), "regenerate source graph") {
				t.Fatalf("reused assemblies against modified baseline DLL input %s: %+v %v", path, update, err)
			}
			if err := os.WriteFile(path, original, 0600); err != nil {
				t.Fatal(err)
			}
		})
	}
	if _, err := compiler.Compile(context.Background(), []string{"Packages/com.example/NewPlugin.dll"}); err == nil {
		t.Fatal("untracked package DLL event silently reused the old managed graph")
	}
}

func TestGoCompilerRejectsMissingOrRenamedBaselineScriptTypes(t *testing.T) {
	var manifest goCompileManifest
	if err := json.Unmarshal([]byte(`{"scripts":[{"guid":"known-guid","path":"/project/Assets/AmbientSound.cs","assembly":"Game"},{"guid":"other-guid","path":"/project/Assets/Helpers.cs","assembly":"Other"}]}`), &manifest); err != nil {
		t.Fatal(err)
	}
	compiler := &goCompiler{manifest: manifest, expectedTypes: map[string]map[string]string{"Game": {"Dungeon.CustomPrimary": "explicit-scene-guid"}}}
	previous := []string{"Dungeon.AmbientSound", "Dungeon.CustomPrimary", "Dungeon.PrivateHelper"}
	if err := compiler.validateRequiredScriptTypes("Game", previous, []string{"Dungeon.AmbientSound", "Dungeon.CustomPrimary"}); err != nil {
		t.Fatalf("accepted body edit or unrelated private helper deletion was rejected: %v", err)
	}
	for _, next := range [][]string{
		{"Dungeon.RenamedAmbientSound", "Dungeon.CustomPrimary"},
		{"OtherNamespace.AmbientSound", "Dungeon.CustomPrimary"},
		{"Dungeon.AmbientSound"},
	} {
		if err := compiler.validateRequiredScriptTypes("Game", previous, next); err == nil {
			t.Fatalf("published stale GUID/type bindings after removing or renaming required script: %v", next)
		}
	}
}

func TestGoCompilerCollectsExactSceneGUIDTypeBindings(t *testing.T) {
	project := t.TempDir()
	manifest := filepath.Join(project, "scene.json")
	data, _ := json.Marshal(map[string]any{"projectRoot": project, "assets": []map[string]string{
		{"kind": "script", "guid": "script-guid", "typeName": "Namespace.NonFilenameType"},
		{"kind": "script", "guid": "host-guid", "typeName": "Unity.HostType"},
	}})
	if err := os.WriteFile(manifest, data, 0600); err != nil {
		t.Fatal(err)
	}
	baseline := filepath.Join(project, "current.json")
	data, _ = json.Marshal(map[string]string{"manifest": manifest})
	if err := os.WriteFile(baseline, data, 0600); err != nil {
		t.Fatal(err)
	}
	var plan goCompileManifest
	if err := json.Unmarshal([]byte(`{"scripts":[{"guid":"script-guid","assembly":"Game"},{"guid":"host-guid","assembly":"Unity.Host","hostProvided":true}]}`), &plan); err != nil {
		t.Fatal(err)
	}
	compiler := &goCompiler{project: project, manifest: plan, currentDLLs: map[string]string{"Game": "Game.dll"}}
	if err := compiler.collectExpectedScriptTypes(baseline); err != nil {
		t.Fatal(err)
	}
	if err := compiler.validateRequiredScriptTypes("Game", nil, []string{"Namespace.DifferentType"}); err == nil {
		t.Fatal("scene GUID binding was lost when class name differed from source filename")
	}
	if err := compiler.validateRequiredScriptTypes("Game", nil, []string{"Namespace.NonFilenameType"}); err != nil {
		t.Fatal(err)
	}
}

func TestGoCompilerRejectsInvalidLifecycleProofAndCacheTampering(t *testing.T) {
	proof := goLifecycleProof{SchemaVersion: 1, Verified: true, ScriptableObjectBarrier: true,
		Phases:              []string{"Awake", "OnEnable", "OnDisable", "OnDestroy"},
		RuntimeInitializers: []runtimeInitializer{{Assembly: "Game", TypeName: "Init", MethodName: "Call", Phase: 2}}}
	if !validGoLifecycleProof(proof, "Game") {
		t.Fatal("valid lifecycle barrier proof rejected")
	}
	proof.RuntimeInitializers[0].Assembly = "Other"
	if validGoLifecycleProof(proof, "Game") {
		t.Fatal("foreign assembly initializer accepted")
	}
	directory := t.TempDir()
	filename := filepath.Join(directory, "Game.dll")
	if err := os.WriteFile(filename, []byte("verified bytes"), 0600); err != nil {
		t.Fatal(err)
	}
	hash, _ := goFileHash(filename)
	if !verifyGoCacheFiles(directory, map[string]string{"Game.dll": hash}) {
		t.Fatal("valid immutable compile payload rejected")
	}
	if verifyGoCacheFiles(directory, map[string]string{"../Game.dll": hash}) {
		t.Fatal("cache key path traversal accepted")
	}
	if err := os.WriteFile(filename, []byte("tampered bytes"), 0600); err != nil {
		t.Fatal(err)
	}
	if verifyGoCacheFiles(directory, map[string]string{"Game.dll": hash}) {
		t.Fatal("tampered DLL was reused")
	}
}

func TestGoCompilerReferencesKeepMonoSDKAndEmittedDependencies(t *testing.T) {
	managed := t.TempDir()
	for _, name := range []string{"mscorlib.dll", "Dungeon.CLIPlay.dll", "Game.dll", "Dependency.dll"} {
		if err := os.WriteFile(filepath.Join(managed, name), []byte(name), 0600); err != nil {
			t.Fatal(err)
		}
	}
	compiler := &goCompiler{baseReferences: []string{"/sdk/mscorlib.dll"}, engineReferences: []string{"/sdk/UnityEngine.CoreModule.dll"},
		currentDLLs: map[string]string{"Game": filepath.Join(managed, "Game.dll"), "Dependency": filepath.Join(managed, "Dependency.dll")},
		configs:     map[string]goAssemblyConfig{"Game": {Name: "Game"}}}
	refs, err := compiler.references(goCompileAssembly{Name: "Game", References: []string{"Dependency"}}, map[string]string{"Dependency": "/new/Dependency.dll"})
	if err != nil {
		t.Fatal(err)
	}
	text := strings.Join(refs, "\n")
	if !strings.Contains(text, "/sdk/mscorlib.dll") || strings.Contains(text, filepath.Join(managed, "mscorlib.dll")) ||
		!strings.Contains(text, "/new/Dependency.dll") || strings.Contains(text, filepath.Join(managed, "Dependency.dll")) ||
		strings.Contains(text, filepath.Join(managed, "Game.dll")) {
		t.Fatalf("references mixed stale/current game DLLs or Mono SDK: %s", text)
	}
}

func TestGoCompilerChangedInterfaceRebuildsDependentGraph(t *testing.T) {
	assemblies := []goCompileAssembly{
		{Name: "API", Sources: []string{"api.cs"}},
		{Name: "Unrelated", Sources: []string{"unrelated.cs"}},
		{Name: "Left", Sources: []string{"left.cs"}, References: []string{"API"}},
		{Name: "Right", Sources: []string{"right.cs"}, References: []string{"API"}},
		{Name: "Game", Sources: []string{"game.cs"}, References: []string{"Left", "Right"}},
	}
	previous := map[string]string{"api.cs": "old API", "unrelated.cs": "same", "left.cs": "same", "right.cs": "same", "game.cs": "same"}
	current := map[string]string{"api.cs": "new API", "unrelated.cs": "same", "left.cs": "same", "right.cs": "same", "game.cs": "same"}
	dirty := goDirtyAssemblies(assemblies, previous, current)
	if len(dirty) != 4 || !dirty["API"] || !dirty["Left"] || !dirty["Right"] || !dirty["Game"] || dirty["Unrelated"] {
		t.Fatalf("changed interface did not invalidate the complete dependent graph: %+v", dirty)
	}
}

func TestGoCompilerBodyOnlyChangeKeepsDependentAssemblies(t *testing.T) {
	api := goCompileAssembly{Name: "API", Sources: []string{"api.cs"}}
	left := goCompileAssembly{Name: "Left", Sources: []string{"left.cs"}, References: []string{"API"}}
	game := goCompileAssembly{Name: "Game", Sources: []string{"game.cs"}, References: []string{"Left"}}
	dirty := goSourceDirtyAssemblies([]goCompileAssembly{api, left, game}, map[string]string{"api.cs": "old", "left.cs": "same", "game.cs": "same"},
		map[string]string{"api.cs": "new", "left.cs": "same", "game.cs": "same"})
	if len(dirty) != 1 || !goAssemblyNeedsCompile(api, dirty, nil) || goAssemblyNeedsCompile(left, dirty, map[string]bool{"API": false}) {
		t.Fatal("body-only source edit rebuilt an unchanged dependent declaration graph")
	}
	if !goAssemblyNeedsCompile(left, dirty, map[string]bool{"API": true}) {
		t.Fatal("changed dependency declarations did not rebuild their consumer")
	}
	if goAssemblyNeedsCompile(game, dirty, map[string]bool{"API": true, "Left": false}) {
		t.Fatal("unchanged intermediate API caused an unnecessary transitive rebuild")
	}
	if !goAssemblyNeedsCompile(game, dirty, map[string]bool{"Left": true}) {
		t.Fatal("changed intermediate API did not propagate to its consumer")
	}
}

func TestGoCompilerDeclarationFingerprintSDK(t *testing.T) {
	editor := os.Getenv("UNITY_GO_DECLARATION_SDK")
	if editor == "" {
		t.Skip("set UNITY_GO_DECLARATION_SDK to matching Unity Editor executable; uses its C# SDK only")
	}
	project := t.TempDir()
	data := filepath.Join(filepath.Dir(editor), "Data")
	compilers, _ := filepath.Glob(filepath.Join(data, "DotNetSdk/sdk/*/Roslyn/bincore/csc.dll"))
	if len(compilers) != 1 {
		t.Fatalf("expected one exact SDK compiler: %v", compilers)
	}
	root := t.TempDir()
	compiler := &goCompiler{project: project, cache: filepath.Join(root, "compiler-cache"),
		dotnet: filepath.Join(data, "DotNetSdk/dotnet"), csc: compilers[0]}
	helper, err := compiler.prepareLifecycleHelper(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	compile := func(name, source string) string {
		t.Helper()
		folder := filepath.Join(root, name)
		if err := os.Mkdir(folder, 0700); err != nil {
			t.Fatal(err)
		}
		filename := filepath.Join(folder, "Shape.cs")
		if err := os.WriteFile(filename, []byte(source), 0600); err != nil {
			t.Fatal(err)
		}
		dll := filepath.Join(folder, "Shape.dll")
		if _, err := goCompilerCommand(context.Background(), folder, compiler.dotnet, compiler.csc,
			"-nologo", "-target:library", "-nostdlib+", "-deterministic+", "-debug:portable", "-optimize-", "-out:"+dll,
			"-reference:"+filepath.Join(data, "MonoBleedingEdge/lib/mono/unityaot-linux/mscorlib.dll"), filename); err != nil {
			t.Fatal(err)
		}
		return dll
	}
	const source = `using System;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
[assembly: InternalsVisibleTo("FriendOne")]
[StructLayout(LayoutKind.Sequential, Pack=4)]
[Obsolete("original")]
public class Shape<T> where T : class {
 public const int Value = 1;
 internal int Field;
 private string Hidden() { return "before"; }
 public int Compute(int optional = 4) { return 41; }
}`
	baseline := compile("baseline", source)
	body := compile("body", strings.Replace(strings.Replace(source, "return 41;", "return 42;", 1), "return \"before\";", "return \"after with a longer user-string literal\";", 1))
	changed, err := compiler.declarationsChanged(context.Background(), helper, baseline, body)
	if err != nil || changed {
		t.Fatalf("real IL/user-string/MVID changes were mistaken for declaration changes: %v %v", changed, err)
	}
	baseHash, _ := goFileHash(baseline)
	bodyHash, _ := goFileHash(body)
	if baseHash == bodyHash {
		t.Fatal("SDK body fixture did not produce different real DLL bytes")
	}
	compiler.expectedTypes = map[string]map[string]string{"Shape": {"Shape`1": "script-guid"}}
	if err := compiler.validateCompiledScriptTypes(context.Background(), helper, "Shape", baseline, body); err != nil {
		t.Fatalf("actual body-only compiled DLL rejected against its baseline script TypeDef: %v", err)
	}
	renamed := compile("renamed-type", strings.Replace(source, "class Shape<T>", "class RenamedShape<T>", 1))
	if err := compiler.validateCompiledScriptTypes(context.Background(), helper, "Shape", baseline, renamed); err == nil {
		t.Fatal("real SDK-compiled class rename did not reject stale MonoScript bindings")
	}
	for name, modification := range map[string][2]string{
		"constant":           {"Value = 1", "Value = 2"},
		"attribute":          {"Obsolete(\"original\")", "Obsolete(\"changed\")"},
		"layout":             {"Pack=4", "Pack=8"},
		"generic-constraint": {"where T : class", "where T : struct"},
		"friend-assembly":    {"FriendOne", "FriendTwo"},
		"internal-field":     {"internal int Field", "internal long Field"},
		"default-parameter":  {"optional = 4", "optional = 5"},
		"signature":          {"public int Compute", "public long Compute"},
	} {
		t.Run(name, func(t *testing.T) {
			dll := compile(name, strings.Replace(source, modification[0], modification[1], 1))
			changed, err := compiler.declarationsChanged(context.Background(), helper, baseline, dll)
			if err != nil || !changed {
				t.Fatalf("changed %s did not invalidate the declaration fingerprint: %v %v", name, changed, err)
			}
		})
	}
	bad := filepath.Join(root, "malformed.dll")
	if err := os.WriteFile(bad, []byte("not a managed PE"), 0600); err != nil {
		t.Fatal(err)
	}
	if changed, err := compiler.declarationsChanged(context.Background(), helper, baseline, bad); !changed || err == nil {
		t.Fatal("unknown metadata did not retain conservative dependency rebuild")
	}
}

func TestGoCompilerSceneAndNativeChangesDoNotScanManagedInputs(t *testing.T) {
	// Deliberately invalid tracked configuration would fail a full scan.
	// Existing scene/native edits bypass it through watcher classification.
	compiler := &goCompiler{project: t.TempDir(), configuration: map[string]string{"/missing/config.cs.meta": "hash"}}
	for _, paths := range [][]string{
		{"Assets/Scenes/SampleScene.unity"}, {"Assets/floor.fbx", "Assets/floor.fbx.meta"},
		{"Assets/audio.wav"}, {"Assets/Texture.png", "Assets/Texture.png.meta"},
	} {
		if update, err := compiler.Compile(context.Background(), paths); err != nil || update != nil {
			t.Fatalf("non-code edit invoked compiler scans: %+v %v", update, err)
		}
	}
	for _, paths := range [][]string{{"Assets/Script.cs"}, {"Assets/Script.cs.meta"}, {"Assets/Game.asmdef"}, {"ProjectSettings/ProjectSettings.asset"}, {"Packages/manifest.json"}, {"Assets/NewDirectory"}} {
		if !compiler.compilerChanges(paths) {
			t.Fatalf("source/config/topology edit skipped stale-code validation: %v", paths)
		}
	}
}

func TestGoCompilerSDKIntegrationWithoutEditor(t *testing.T) {
	baseline, player := os.Getenv("UNITY_GO_COMPILER_BASELINE"), os.Getenv("UNITY_GO_COMPILER_PLAYER")
	if baseline == "" || player == "" {
		t.Skip("optional SDK-only integration; set UNITY_GO_COMPILER_BASELINE and UNITY_GO_COMPILER_PLAYER")
	}
	manifest, _, err := goCompilerManifest(baseline)
	if err != nil {
		t.Fatal(err)
	}
	cache := filepath.Join(manifest.Project, "Temp/UnityPlay/go-compiler-proof", randomID())
	compiler, err := newGoCompiler(manifest.Project, baseline, player, manifest.Unity, cache)
	if err != nil {
		t.Fatal(err)
	}
	initial, err := compiler.Compile(context.Background(), nil)
	if err != nil {
		t.Fatal(err)
	}
	for _, assembly := range compiler.manifest.Assemblies {
		if goNeedsNetcode(assembly) && (initial == nil || !compiler.netcodeProcessed[assembly.Name]) {
			t.Fatalf("plain Roslyn network baseline was reused without NGO postprocessing: %+v", initial)
		}
	}
	if update, err := compiler.Compile(context.Background(), nil); err != nil || update != nil {
		t.Fatalf("processed unchanged baseline should reuse verified source DLLs: %+v %v", update, err)
	}
	if initial != nil {
		restarted, err := newGoCompiler(manifest.Project, baseline, player, manifest.Unity, cache)
		if err != nil {
			t.Fatal(err)
		}
		reused, err := restarted.Compile(context.Background(), nil)
		if err != nil || reused == nil || !reused.CacheReused || reused.Player != initial.Player || len(reused.CompiledAssemblies) != 0 ||
			len(reused.RuntimeInitializers) != len(initial.RuntimeInitializers) {
			t.Fatalf("fresh process repeated SDK compilation instead of reusing verified complete graph: %+v %v", reused, err)
		}
		for _, assembly := range manifest.Assemblies {
			if goNeedsNetcode(assembly) && !restarted.netcodeProcessed[assembly.Name] {
				t.Fatal("cached NGO postprocessing proof did not restore networking state")
			}
		}
		t.Logf("SDK-only fresh-process cache: %.3fs, Player=%s", reused.ElapsedSeconds, reused.Player)
	}
	// Simulate a stale source checksum in the compiler state, without
	// changing author-owned project sources or launching any Player.
	leaf := compiler.manifest.Assemblies[len(compiler.manifest.Assemblies)-1]
	compiler.sources[leaf.Sources[0]] = "stale source checksum"
	update, err := compiler.Compile(context.Background(), nil)
	if err != nil {
		t.Fatal(err)
	}
	if update == nil || update.Player == player || len(update.CompiledAssemblies) != 1 || update.CompiledAssemblies[0] != leaf.Name ||
		len(update.Assemblies) != len(compiler.manifest.Assemblies) {
		t.Fatalf("stale source did not create a complete immutable Player revision: %+v", update)
	}
	if _, err := os.Stat(update.Player); err != nil {
		t.Fatal(err)
	}
	// Validate the real scene's GUID/full-name binding map against DLL TypeDefs,
	// without loading any assembly or starting another Unity process.
	helper, err := compiler.prepareLifecycleHelper(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	requiredCount := 0
	for assembly, names := range compiler.expectedTypes {
		requiredCount += len(names)
		if len(names) == 0 {
			continue
		}
		previous := filepath.Join(filepath.Dir(player), strings.TrimSuffix(filepath.Base(player), filepath.Ext(player))+"_Data/Managed", assembly+".dll")
		if err := compiler.validateCompiledScriptTypes(context.Background(), helper, assembly, previous, compiler.currentDLLs[assembly]); err != nil {
			t.Fatalf("actual scene baseline script bindings did not match emitted DLL: %v", err)
		}
	}
	t.Logf("validated actual scene bindings: %d full script types across %d assemblies", requiredCount, len(compiler.expectedTypes))
	if repeat, err := compiler.Compile(context.Background(), nil); err != nil || repeat != nil {
		t.Fatalf("unchanged code should reuse compiled revision: %+v %v", repeat, err)
	}
	t.Logf("SDK-only revision=%s compiled=%v elapsed=%.3fs", update.Player, update.CompiledAssemblies, update.ElapsedSeconds)
}
