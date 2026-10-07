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

func TestGoSourceVersionIntervalsAndConstraints(t *testing.T) {
	for _, test := range []struct {
		current, expression string
		expected            bool
	}{{"1.20.0", "[1.19,2)", true}, {"1.20.0", "(1.20,2)", false}, {"1.20.0", "[1.20.0]", true}, {"6000.6.3f1", "2022.2.14f1", true}, {"1.0.0-preview.2", "1.0.0", false}, {"2.0.0", "(,2)", false}} {
		actual, err := matchGoSourceVersion(test.current, test.expression)
		if err != nil || actual != test.expected {
			t.Fatalf("%s in%s: %v %v", test.current, test.expression, actual, err)
		}
	}
	for _, test := range []struct {
		constraints []string
		expected    bool
	}{{[]string{"A || B", "!C"}, true}, {[]string{"A", "C"}, false}, {[]string{"!A || C"}, false}} {
		actual, err := matchGoSourceConstraints(test.constraints, []string{"A"})
		if err != nil || actual != test.expected {
			t.Fatalf("constraints %+v: %v %v", test.constraints, actual, err)
		}
	}
	if _, err := matchGoSourceConstraints([]string{"A && B"}, nil); err == nil {
		t.Fatal("unsupported constraints silently accepted")
	}
}

func TestGoSourceInitializerOverlayRetainsNativeEngineRowsOnce(t *testing.T) {
	project := t.TempDir()
	path := writeGoGraphFile(t, project, "Temp/RuntimeInitializeOnLoads.json", `{"root":[{"assemblyName":"Game","methodName":"Initialize"},{"assemblyName":"Unity.InputSystem","methodName":"RuntimeInitialize"}],"extra":{"profile":1}}`)
	if err := removeGoSourceNativeInitializers(path, []goCompileAssembly{{Name: "Game"}}); err != nil {
		t.Fatal(err)
	}
	data, _ := os.ReadFile(path)
	if strings.Contains(string(data), `"assemblyName":"Game"`) || !strings.Contains(string(data), "Unity.InputSystem") || !strings.Contains(string(data), "extra") {
		t.Fatalf("native ownership table was clobbered: %s", data)
	}
}

func TestGoSourceGraphDerivesGUIDAsmrefPlatformConstraintsAndPredefinedDependencies(t *testing.T) {
	project := t.TempDir()
	writeGoGraphFile(t, project, "Packages/manifest.json", `{"dependencies":{}}`)
	writeGoGraphFile(t, project, "Packages/packages-lock.json", `{"dependencies":{}}`)
	writeGoGraphFile(t, project, "Assets/A/A.asmdef", `{"name":"A","references":["GUID:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"],"versionDefines":[{"name":"Unity","expression":"[6000.6,6001)","define":"NEW_ENGINE"}],"defineConstraints":["NEW_ENGINE || OPTIONAL"]}`)
	writeGoGraphFile(t, project, "Assets/A/A.asmdef.meta", "guid: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n")
	writeGoGraphFile(t, project, "Assets/B/B.asmdef", `{"name":"B","autoReferenced":false,"noEngineReferences":true}`)
	writeGoGraphFile(t, project, "Assets/B/B.asmdef.meta", "guid: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n")
	writeGoGraphFile(t, project, "Assets/Attached/Assembly.asmref", `{"reference":"GUID:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}`)
	writeGoGraphFile(t, project, "Assets/OnlyEditor/Editor.asmdef", `{"name":"OnlyEditor","includePlatforms":["Editor"],"references":["AbsentEditorAssembly"]}`)
	writeGoGraphFile(t, project, "Assets/OnlyEditor/Editor.asmdef.meta", "guid: cccccccccccccccccccccccccccccccc\n")
	for index, path := range []string{"Assets/A/A.cs", "Assets/A/Editor/StillA.cs", "Assets/B/B.cs", "Assets/Attached/Attached.cs", "Assets/OnlyEditor/Skipped.cs", "Assets/Loose.cs", "Assets/Plugins/Early.cs", "Assets/Editor/Excluded.cs"} {
		writeGoGraphFile(t, project, path, "public class Type"+string(rune('A'+index))+" {}")
		writeGoGraphFile(t, project, path+".meta", "guid: "+strings.Repeat(string(rune('1'+index)), 32)+"\n")
	}
	snapshot, err := packages.Resolve(context.Background(), project, "", packages.Options{Offline: true})
	if err != nil {
		t.Fatal(err)
	}
	profile := &goSourceEngineProfile{SchemaVersion: 1, UnityVersion: fixtureUnityVersion, UnityRevision: "45d8eee7de74", Target: "LinuxStandalone64", ScriptingBackend: "Mono", Development: true, BaseDefines: []string{"UNITY_STANDALONE_LINUX", "ENABLE_MONO"}, NativeAssemblies: map[string]goSourceNativeAssembly{}}
	profile.ProfileSHA256, _ = goSourceProfileHash(*profile)
	graph, err := buildGoSourceGraph(context.Background(), goSourceGraphOptions{Project: project, Profile: profile, Packages: snapshot})
	if err != nil {
		t.Fatal(err)
	}
	byName := map[string]goAssemblyConfig{}
	for _, assembly := range graph.Assemblies {
		byName[assembly.Name] = assembly.goAssemblyConfig
	}
	if len(byName) != 4 || len(byName["A"].Sources) != 3 || !strings.Contains(strings.Join(byName["A"].Defines, ","), "NEW_ENGINE") || !equalStrings(byName["A"].Dependencies, []string{"B"}) {
		t.Fatalf("nearest asmdef/asmref/Player define graph incorrect: %+v", byName)
	}
	if !equalStrings(byName["Assembly-CSharp"].Dependencies, []string{"A", "Assembly-CSharp-firstpass"}) || len(byName["Assembly-CSharp"].Sources) != 1 || !byName["B"].Config.NoEngineReferences {
		t.Fatalf("predefined auto-reference/firstpass/Editor exclusion incorrect: %+v", byName)
	}
	first, _ := json.Marshal(graph)
	again, err := buildGoSourceGraph(context.Background(), goSourceGraphOptions{Project: project, Profile: profile, Packages: snapshot})
	if err != nil {
		t.Fatal(err)
	}
	second, _ := json.Marshal(again)
	if string(first) != string(second) {
		t.Fatal("source graph varies across unchanged source scans")
	}
}

// Explicit opt-in uses existing public reference only as comparison/profile
// evidence. All assembly decisions in the result come from Go source scanning.
func TestGoSourceGraphActualComparison(t *testing.T) {
	project := os.Getenv("UNITY_GO_SOURCE_GRAPH_PROJECT")
	if project == "" {
		t.Skip("set UNITY_GO_SOURCE_GRAPH_PROJECT, _REFERENCE, _PLAYER, _PACKAGES for actual source graph comparison")
	}
	editor := os.Getenv("UNITY_GO_LIFECYCLE_SDK")
	snapshotFile := os.Getenv("UNITY_GO_SOURCE_GRAPH_PACKAGES")
	reference := os.Getenv("UNITY_GO_SOURCE_GRAPH_REFERENCE")
	player := os.Getenv("UNITY_GO_SOURCE_GRAPH_PLAYER")
	var snapshot packages.Snapshot
	if os.Getenv("UNITY_GO_SOURCE_GRAPH_RESOLVE") == "1" {
		resolved, err := packages.Resolve(context.Background(), project, editor, packages.Options{Offline: os.Getenv("UNITY_GO_SOURCE_GRAPH_OFFLINE") == "1"})
		if err != nil {
			t.Fatal(err)
		}
		snapshot = *resolved
	} else {
		if err := agentReadJSON(snapshotFile, 32<<20, &snapshot); err != nil {
			t.Fatal(err)
		}
	}
	output := filepath.Join(project, "Library/UnityMicroEditor/go-source-tests", strings.ReplaceAll(t.Name(), "/", "-")+"-"+agentID())
	if err := os.MkdirAll(output, 0700); err != nil {
		t.Fatal(err)
	}
	profile, err := captureGoSourceEngineProfile(context.Background(), project, editor, player, reference, filepath.Join(output, "engine-profile.json"), &snapshot)
	if err != nil {
		t.Fatal(err)
	}
	graph, err := buildGoSourceGraph(context.Background(), goSourceGraphOptions{Project: project, Editor: editor, Player: player, Profile: profile, Packages: &snapshot})
	if err != nil {
		t.Fatal(err)
	}
	if err := atomicJSON(filepath.Join(output, "go-graph.json"), graph); err != nil {
		t.Fatal(err)
	}
	var oracle goCompilerGraphExport
	if err := agentReadJSON(reference, 32<<20, &oracle); err != nil {
		t.Fatal(err)
	}
	differences := compareGoSourceGraphs(graph, &oracle)
	if err := atomicJSON(filepath.Join(output, "comparison.json"), differences); err != nil {
		t.Fatal(err)
	}
	data, _ := json.Marshal(differences)
	t.Logf("own graph %d SDK +%d native assemblies; comparison %s; output%s", len(graph.Assemblies), len(graph.HostAssemblies), data, output)
	if os.Getenv("UNITY_GO_SOURCE_GRAPH_REQUIRE_MATCH") == "1" && len(differences) != 0 {
		t.Fatalf("own graph differs from explicit public oracle; see %s", filepath.Join(output, "comparison.json"))
	}
	if os.Getenv("UNITY_GO_SOURCE_GRAPH_COMPILE") == "1" {
		result, err := prepareGoSourceCompilerGraph(context.Background(), goSourceGraphOptions{Project: project, Editor: editor, Player: player, Profile: profile, Packages: &snapshot}, filepath.Join(output, "compiler.json"))
		if err != nil {
			t.Fatal(err)
		}
		if result.Compiler.Backend != "go_authored_source" {
			t.Fatal("source compilation lost its backend")
		}
		compiler, err := newGoCompiler(project, filepath.Join(output, "compiler.json"), result.Player, editor, filepath.Join(output, "restart-cache"))
		if err != nil {
			t.Fatal(err)
		}
		if _, err := compiler.Compile(context.Background(), nil); err != nil {
			t.Fatal(err)
		}
		if _, err := compiler.PreparedManifest(context.Background()); err != nil {
			t.Fatal(err)
		}
		t.Logf("own SDK compiler/restart accepted %d assemblies; Player %s", len(result.Compiler.Assemblies), result.Player)
	}
}
