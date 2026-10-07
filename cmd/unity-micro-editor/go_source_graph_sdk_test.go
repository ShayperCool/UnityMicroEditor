package main

import (
	"context"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"

	"unitymicroeditor/internal/project/packages"
)

// The native executable/bridge are deliberately synthetic SDK fixtures. No
// public Editor evidence is asserted, and neither Editor nor Player executes.
// Production profile capture is exercised separately against a real build.
func TestGoSourceGraphNewAsmdefPackageAndBodyChangeSDK(t *testing.T) {
	editor := os.Getenv("UNITY_GO_LIFECYCLE_SDK")
	if editor == "" {
		t.Skip("set UNITY_GO_LIFECYCLE_SDK; SDK only, no Editor or Player starts")
	}
	project := t.TempDir()
	writeGoGraphFile(t, project, "ProjectSettings/ProjectVersion.txt", "m_EditorVersion: "+fixtureUnityVersion+"\n")
	writeGoGraphFile(t, project, "Packages/manifest.json", `{"dependencies":{"com.test.pure":"1.0.0"}}`)
	writeGoGraphFile(t, project, "Packages/com.test.pure/package.json", `{"name":"com.test.pure","version":"1.0.0"}`)
	writeGoGraphFile(t, project, "Packages/com.test.pure/Pure.asmdef", `{"name":"Pure.Package","autoReferenced":false,"versionDefines":[{"name":"Unity","expression":"[6000.6,6001)","define":"PINNED_UNITY"}],"defineConstraints":["PINNED_UNITY"]}`)
	writeGoGraphFile(t, project, "Packages/com.test.pure/Pure.asmdef.meta", "guid: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n")
	pure := writeGoGraphFile(t, project, "Packages/com.test.pure/Counter.cs", "namespace Pure { public static class Counter { public static int Read() => 7; } }")
	writeGoGraphFile(t, project, "Packages/com.test.pure/Counter.cs.meta", "guid: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n")
	writeGoGraphFile(t, project, "Assets/Game/Game.asmdef", `{"name":"New.Game","references":["GUID:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]}`)
	writeGoGraphFile(t, project, "Assets/Game/Game.asmdef.meta", "guid: cccccccccccccccccccccccccccccccc\n")
	probe := writeGoGraphFile(t, project, "Assets/Game/Probe.cs", "public class Probe : UnityEngine.MonoBehaviour { public int revision = Pure.Counter.Read(); private void Awake() {} }")
	writeGoGraphFile(t, project, "Assets/Game/Probe.cs.meta", "guid: dddddddddddddddddddddddddddddddd\nMonoImporter:\n  executionOrder: 0\n")
	sdk := &goCompiler{editor: editor}
	if err := sdk.initializeSDK(); err != nil {
		t.Fatal(err)
	}
	player := writeGoGraphFile(t, project, "Temp/native/Host.x86_64", "synthetic SDK executable, never launched")
	managed := goSourceManagedDirectory(player)
	bridgeSource := writeGoGraphFile(t, project, "Temp/Bridge.cs", `using System; using UnityEngine;
namespace UnityMicroEditor.Raw {
[AttributeUsage(AttributeTargets.Assembly)] public sealed class RawLifecycleInstrumentedAttribute : Attribute { public RawLifecycleInstrumentedAttribute(int version) {} }
[AttributeUsage(AttributeTargets.Assembly)] public sealed class RawScriptableObjectInstrumentedAttribute : Attribute { public RawScriptableObjectInstrumentedAttribute(int version) {} }
public static class RawLifecycle { public enum Phase { Awake, OnEnable, OnDisable, OnDestroy }
public static bool ShouldCollect(MonoBehaviour value,Phase phase) => false;
public static bool ShouldCollect(ScriptableObject value,Phase phase) => false;
public static void Enqueue(MonoBehaviour value,Phase phase,Action callback,int order) {}
public static void Enqueue(ScriptableObject value,Phase phase,Action callback,int order) {}
} }`)
	if err := os.MkdirAll(managed, 0700); err != nil {
		t.Fatal(err)
	}
	args := []string{sdk.csc, "-nologo", "-target:library", "-nostdlib+", "-out:" + filepath.Join(managed, "UnityMicroEditor.dll")}
	for _, path := range deduplicateGoReferences(append(slices.Clone(sdk.baseReferences), sdk.engineReferences...)) {
		args = append(args, "-reference:"+path)
	}
	args = append(args, bridgeSource)
	if _, err := goCompilerCommand(context.Background(), project, sdk.dotnet, args...); err != nil {
		t.Fatal(err)
	}
	writeGoGraphFile(t, project, "Temp/native/Host_Data/ScriptingAssemblies.json", `{"names":["UnityMicroEditor.dll"],"types":[16],"retainNative":true}`)
	writeGoGraphFile(t, project, "Temp/native/Host_Data/RuntimeInitializeOnLoads.json", `{"root":[{"assemblyName":"Pure.Package","methodName":"Old"},{"assemblyName":"Native.Engine","methodName":"Retain"}]}`)
	profile := &goSourceEngineProfile{SchemaVersion: 1, UnityVersion: fixtureUnityVersion, UnityRevision: "45d8eee7de74", Editor: editor, Player: player, Target: "LinuxStandalone64", ScriptingBackend: "Mono", Development: true, BaseDefines: []string{"UNITY_6000_6", "UNITY_STANDALONE_LINUX", "ENABLE_MONO"}, BaseReferences: sdk.baseReferences, EngineReferences: sdk.engineReferences, Inputs: map[string]string{}, NativeAssemblies: map[string]goSourceNativeAssembly{}}
	for _, path := range append(slices.Clone(sdk.baseReferences), sdk.engineReferences...) {
		hash, err := goFileHash(path)
		if err != nil {
			t.Fatal(err)
		}
		profile.Inputs[path] = hash
	}
	profile.ProfileSHA256, _ = goSourceProfileHash(*profile)
	options := goSourceGraphOptions{Project: project, Editor: editor, Player: player, Profile: profile}
	resolve := func() {
		t.Helper()
		var err error
		options.Packages, err = packages.Resolve(context.Background(), project, editor, packages.Options{Offline: true})
		if err != nil {
			t.Fatal(err)
		}
	}
	resolve()
	output := filepath.Join(project, "Temp/own/first.json")
	result, err := prepareGoSourceCompilerGraph(context.Background(), options, output)
	if err != nil {
		t.Fatal(err)
	}
	if len(result.Compiler.Assemblies) != 2 || result.Compiler.Backend != "go_authored_source" {
		t.Fatalf("SDK graph %+v", result.Compiler)
	}
	if result.Compiler.Scripts[len(result.Compiler.Scripts)-1].GUID == "" {
		t.Fatal("GUID binding absent")
	}
	found := false
	for _, script := range result.Compiler.Scripts {
		if script.Path == probe && script.GUID == strings.Repeat("d", 32) && script.TypeName == "Probe" && script.Assembly == "New.Game" {
			found = true
		}
	}
	if !found {
		t.Fatal("new GUID/full type/assembly assignment lost")
	}
	var slots struct {
		Names  []string `json:"names"`
		Retain bool     `json:"retainNative"`
	}
	if err := readJSONFile(filepath.Join(filepath.Dir(goSourceManagedDirectory(result.Player)), "ScriptingAssemblies.json"), &slots); err != nil {
		t.Fatal(err)
	}
	if !slots.Retain || !slices.Contains(slots.Names, "Pure.Package.dll") || !slices.Contains(slots.Names, "New.Game.dll") {
		t.Fatalf("new assembly slots clobbered native profile: %+v", slots)
	}
	compiler, err := newGoCompiler(project, output, result.Player, editor, filepath.Join(project, "Temp/restart"))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := compiler.Compile(context.Background(), nil); err != nil {
		t.Fatal(err)
	}
	accepted, err := compiler.PreparedManifest(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if accepted.EngineProfileSHA256 != profile.ProfileSHA256 {
		t.Fatal("restart lost source provenance")
	}
	oldHash := result.Compiler.Assemblies[0].SHA256
	if err := os.WriteFile(pure, []byte("namespace Pure { public static class Counter { public static int Read() => 13; } }"), 0600); err != nil {
		t.Fatal(err)
	}
	resolve()
	next, err := prepareGoSourceCompilerGraph(context.Background(), options, filepath.Join(project, "Temp/own/changed.json"))
	if err != nil {
		t.Fatal(err)
	}
	if next.Compiler.Assemblies[0].SHA256 == oldHash {
		t.Fatal("package body change reused old native/SDK DLL")
	}
	if _, err := os.Stat(filepath.Join(project, "Library/ScriptAssemblies")); !os.IsNotExist(err) {
		t.Fatal("own preparation relied on Editor-produced assembly directory")
	}
}
