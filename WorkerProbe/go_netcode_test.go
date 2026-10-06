package main

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestGoNetcodePostprocessorSDK(t *testing.T) {
	editor := os.Getenv("UNITY_GO_NETCODE_SDK")
	if editor == "" {
		t.Skip("set UNITY_GO_NETCODE_SDK to Unity executable; only its SDK/package ILPP is used")
	}
	projectSource := os.Getenv("UNITY_MICRO_EDITOR_TEST_PROJECT")
	if projectSource == "" {
		t.Skip("set UNITY_MICRO_EDITOR_TEST_PROJECT to the prepared game project for this integration check")
	}
	project, err := filepath.Abs(projectSource)
	if err != nil {
		t.Fatal(err)
	}
	data := filepath.Join(filepath.Dir(editor), "Data")
	csc, _ := filepath.Glob(filepath.Join(data, "DotNetSdk/sdk/*/Roslyn/bincore/csc.dll"))
	if len(csc) != 1 {
		t.Fatal("expected one pinned SDK")
	}
	root := t.TempDir()
	compiler := &goCompiler{project: project, editor: editor, cache: filepath.Join(root, "compiler"),
		player: filepath.Join(project, "Builds/CLIPlayHybridHost/UnityPlayHost.x86_64"),
		dotnet: filepath.Join(data, "DotNetSdk/dotnet"), csc: csc[0],
		configs: map[string]goAssemblyConfig{"NetcodeFixture": {Name: "NetcodeFixture", Defines: []string{"UNITY_STANDALONE_LINUX"}}}}
	processor, err := compiler.prepareNetcodeProcessor(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	refs, _ := filepath.Glob(filepath.Join(data, "MonoBleedingEdge/lib/mono/unityaot-linux/*.dll"))
	facades, _ := filepath.Glob(filepath.Join(data, "MonoBleedingEdge/lib/mono/unityaot-linux/Facades/*.dll"))
	managed := filepath.Join(project, "Builds/CLIPlayHybridHost/UnityPlayHost_Data/Managed")
	host, _ := filepath.Glob(filepath.Join(managed, "*.dll"))
	refs = deduplicateGoReferences(append(append(refs, facades...), host...))
	source := filepath.Join(root, "NetcodeFixture.cs")
	if err := os.WriteFile(source, []byte(`using Unity.Netcode;
public sealed class NetcodeFixture : NetworkBehaviour {
 public readonly NetworkVariable<int> Counter = new NetworkVariable<int>();
 [Rpc(SendTo.Server)] public void UseRpc(int amount, RpcParams rpc = default) { Counter.Value += amount; }
}`), 0600); err != nil {
		t.Fatal(err)
	}
	dll := filepath.Join(root, "NetcodeFixture.dll")
	args := []string{compiler.csc, "-nologo", "-target:library", "-nostdlib+", "-debug:portable", "-out:" + dll}
	for _, reference := range refs {
		args = append(args, "-reference:"+reference)
	}
	args = append(args, source)
	if _, err := goCompilerCommand(context.Background(), root, compiler.dotnet, args...); err != nil {
		t.Fatal(err)
	}
	proof := goLifecycleProof{SchemaVersion: 1, Verified: true, ScriptableObjectBarrier: true,
		Phases: []string{"Awake", "OnEnable", "OnDisable", "OnDestroy"}}
	processed, nextProof, err := compiler.postprocessNetcode(context.Background(), root, dll,
		goCompileAssembly{Name: "NetcodeFixture", References: []string{"Unity.Netcode.Runtime"}}, refs, proof)
	if err != nil {
		t.Fatal(err)
	}
	var result struct {
		RPCCount int `json:"rpcCount"`
		RPCs     []struct {
			Method, Handler                  string
			SendCalls, WriteCalls, ReadCalls []string
		} `json:"rpcs"`
	}
	bytes, _ := os.ReadFile(filepath.Join(root, "netcode-proof.json"))
	if err := json.Unmarshal(bytes, &result); err != nil {
		t.Fatal(err)
	}
	if result.RPCCount != 1 || len(result.RPCs) != 1 || result.RPCs[0].Method != "UseRpc" ||
		!strings.HasPrefix(result.RPCs[0].Handler, "__rpc_handler_") || len(result.RPCs[0].SendCalls) < 2 ||
		len(result.RPCs[0].WriteCalls) == 0 || len(result.RPCs[0].ReadCalls) == 0 {
		t.Fatalf("RPC send/receive payload code was not generated: %+v", result)
	}
	if !validGoLifecycleProof(nextProof, "NetcodeFixture") || len(nextProof.RuntimeInitializers) != 1 ||
		nextProof.RuntimeInitializers[0].MethodName != "__UnityPlay_RuntimeInitialize_Netcode" {
		t.Fatalf("generated NetworkVariable serializers have no public initialization bridge: %+v", nextProof)
	}
	originalHash, _ := goFileHash(dll)
	processedHash, _ := goFileHash(processed)
	if originalHash == processedHash {
		t.Fatal("ILPP returned the original plain Roslyn assembly")
	}
	if reused, err := compiler.prepareNetcodeProcessor(context.Background()); err != nil || reused.Helper != processor.Helper {
		t.Fatalf("verified SDK helper cache was not reused: %+v %v", reused, err)
	}
	input := filepath.Join(root, "PinnedProcessorInput")
	os.WriteFile(input, []byte("original"), 0600)
	processor.Inputs[input], _ = goFileHash(input)
	os.WriteFile(input, []byte("modified"), 0600)
	if _, err := compiler.prepareNetcodeProcessor(context.Background()); err == nil {
		t.Fatal("postprocessor dependency drift silently reused compiled network code")
	}
	t.Logf("SDK-only NGO result: %s, rpc=%s, serializers=%v", processed, result.RPCs[0].Handler, result.RPCs[0].WriteCalls)
}
