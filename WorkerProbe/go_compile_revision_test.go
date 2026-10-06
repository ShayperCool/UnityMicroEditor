package main

import (
	"context"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

func goCodeRevisionFixture(t *testing.T) (*goCompiler, map[string]string) {
	t.Helper()
	root := t.TempDir()
	write := func(path, value string, mode os.FileMode) string {
		t.Helper()
		filename := filepath.Join(root, path)
		if err := os.MkdirAll(filepath.Dir(filename), 0700); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filename, []byte(value), mode); err != nil {
			t.Fatal(err)
		}
		return filename
	}
	player := write("staged/Host.x86_64", "native executable", 0700)
	dll := write("staged/Host_Data/Managed/Game.dll", "baseline game", 0600)
	write("staged/Host_Data/Managed/Host.dll", "managed bridge", 0600)
	write("staged/UnityPlayer.so", "native Unity", 0600)
	source := write("Assets/Game.cs", "class Game {}", 0600)
	helper := write("compiler/lifecycle/test/LifecycleCompiler.dll", "verified lifecycle helper", 0600)
	helperHash, _ := goFileHash(helper)
	if err := atomicJSON(filepath.Join(filepath.Dir(helper), "files.json"), map[string]string{"LifecycleCompiler.dll": helperHash}); err != nil {
		t.Fatal(err)
	}
	csc := write("sdk/csc.dll", "Roslyn", 0600)
	write("sdk/Microsoft.CodeAnalysis.dll", "Roslyn dependency", 0600)
	dotnet := write("sdk/dotnet", "SDK executable", 0600)
	generator := write("generators/Unity.SourceGenerators.dll", "generator", 0600)
	write("generators/Dependency.dll", "generator dependency", 0600)
	proof := goLifecycleProof{SchemaVersion: 1, Verified: true, ScriptableObjectBarrier: true,
		Phases:              []string{"Awake", "OnEnable", "OnDisable", "OnDestroy"},
		RuntimeInitializers: []runtimeInitializer{{Assembly: "Game", TypeName: "Game", MethodName: "Init", Phase: 2}}}
	dllHash, _ := goFileHash(dll)
	sourceHash, _ := goFileHash(source)
	sources := map[string]string{source: sourceHash}
	compiler := &goCompiler{project: root, cache: filepath.Join(root, "compiler"), player: player, baselinePlayer: player,
		helper: helper, csc: csc, dotnet: dotnet, generators: []string{generator}, currentDLLs: map[string]string{"Game": dll},
		manifest: goCompileManifest{Schema: 1, Project: root, Assemblies: []goCompileAssembly{{Name: "Game", SHA256: dllHash,
			Sources: []string{source}, LifecycleBarrier: proof}}, CompilerInputs: sources},
		proofs: map[string]goLifecycleProof{"Game": proof}, sources: sources,
		configuration: map[string]string{}, configurationHashes: map[string]string{"Game": "configuration"}}
	return compiler, sources
}

func goSaveCodeRevisionFixture(t *testing.T, compiler *goCompiler, sources map[string]string, key string) *compileUpdate {
	t.Helper()
	folder := filepath.Join(compiler.cache, ".code-revision-test", "Player")
	if err := os.MkdirAll(filepath.Dir(folder), 0700); err != nil {
		t.Fatal(err)
	}
	if _, err := goCompilerCommand(context.Background(), "", "cp", "-a", filepath.Dir(compiler.player), folder); err != nil {
		t.Fatal(err)
	}
	player := filepath.Join(folder, filepath.Base(compiler.player))
	dll := filepath.Join(folder, "Host_Data/Managed/Game.dll")
	if err := os.WriteFile(dll, []byte("verified recompiled game"), 0600); err != nil {
		t.Fatal(err)
	}
	update := &compileUpdate{Player: player, SourceHashes: sources, Assemblies: []string{dll}}
	if err := compiler.saveCodeRevision(key, update, compiler.proofs); err != nil {
		t.Fatal(err)
	}
	return update
}

func TestGoCodeRevisionRestoresVerifiedPlayerAndLifecycleState(t *testing.T) {
	compiler, sources := goCodeRevisionFixture(t)
	key, err := compiler.codeRevisionKey(context.Background(), sources)
	if err != nil {
		t.Fatal(err)
	}
	original := goSaveCodeRevisionFixture(t, compiler, sources, key)
	restored := compiler.restoreCodeRevision(key, sources)
	if restored == nil || !restored.CacheReused || restored.Player != original.Player || len(restored.CompiledAssemblies) != 0 ||
		!reflect.DeepEqual(restored.Assemblies, original.Assemblies) || !reflect.DeepEqual(restored.RuntimeInitializers, compiler.proofs["Game"].RuntimeInitializers) {
		t.Fatalf("verified complete revision was not reused: %+v", restored)
	}
	compiler.useCodeRevision(restored, compiler.proofs)
	if !compiler.startupComplete || compiler.player != original.Player || compiler.currentDLLs["Game"] != original.Assemblies[0] {
		t.Fatal("cached graph was not installed as current compilation state")
	}
}

func TestGoCodeRevisionIdentityTracksSourcesBridgeNativeSDKAndGenerators(t *testing.T) {
	compiler, sources := goCodeRevisionFixture(t)
	key, err := compiler.codeRevisionKey(context.Background(), sources)
	if err != nil {
		t.Fatal(err)
	}
	changedSources := map[string]string{}
	for path := range sources {
		changedSources[path] = bytesSHA([]byte("changed source"))
	}
	if changed, err := compiler.codeRevisionKey(context.Background(), changedSources); err != nil || changed == key {
		t.Fatalf("changed source was not included in code revision identity: %s %v", changed, err)
	}
	for _, path := range []string{filepath.Join(filepath.Dir(compiler.player), "Host_Data/Managed/Host.dll"),
		filepath.Join(filepath.Dir(compiler.player), "UnityPlayer.so"), filepath.Join(filepath.Dir(compiler.csc), "Microsoft.CodeAnalysis.dll"),
		filepath.Join(filepath.Dir(compiler.generators[0]), "Dependency.dll")} {
		t.Run(filepath.Base(path), func(t *testing.T) {
			original, err := os.ReadFile(path)
			if err != nil {
				t.Fatal(err)
			}
			if err := os.WriteFile(path, []byte("changed dependency"), 0600); err != nil {
				t.Fatal(err)
			}
			changed, err := compiler.codeRevisionKey(context.Background(), sources)
			if err != nil || changed == key {
				t.Fatalf("changed dependency did not invalidate startup revision: %s %v", changed, err)
			}
			if err := os.WriteFile(path, original, 0600); err != nil {
				t.Fatal(err)
			}
		})
	}
	compiler.configurationHashes["Game"] = "changed assembly configuration"
	if changed, err := compiler.codeRevisionKey(context.Background(), sources); err != nil || changed == key {
		t.Fatalf("changed assembly configuration did not invalidate revision: %s %v", changed, err)
	}
}

func TestGoCodeRevisionRejectsCorruptDLLAddedFilesAndInvalidProof(t *testing.T) {
	for _, kind := range []string{"dll", "native", "added-dll", "proof", "escape", "symlink"} {
		t.Run(kind, func(t *testing.T) {
			compiler, sources := goCodeRevisionFixture(t)
			key, err := compiler.codeRevisionKey(context.Background(), sources)
			if err != nil {
				t.Fatal(err)
			}
			update := goSaveCodeRevisionFixture(t, compiler, sources, key)
			var entry goCodeRevision
			if err := readJSONFile(compiler.codeRevisionPath(key), &entry); err != nil {
				t.Fatal(err)
			}
			switch kind {
			case "dll", "native":
				path := update.Assemblies[0]
				if kind == "native" {
					path = filepath.Join(filepath.Dir(update.Player), "UnityPlayer.so")
				}
				if err := os.WriteFile(path, []byte("corrupted output"), 0600); err != nil {
					t.Fatal(err)
				}
			case "added-dll":
				if err := os.WriteFile(filepath.Join(filepath.Dir(update.Assemblies[0]), "Unexpected.dll"), []byte("unverified assembly"), 0600); err != nil {
					t.Fatal(err)
				}
			case "proof":
				proof := entry.Proofs["Game"]
				proof.Verified = false
				entry.Proofs["Game"] = proof
			case "escape":
				entry.Player = "../staged/Host.x86_64"
			case "symlink":
				if err := os.Remove(update.Assemblies[0]); err != nil {
					t.Fatal(err)
				}
				if err := os.Symlink(compiler.currentDLLs["Game"], update.Assemblies[0]); err != nil {
					t.Fatal(err)
				}
			}
			if err := atomicJSON(compiler.codeRevisionPath(key), entry); err != nil {
				t.Fatal(err)
			}
			if restored := compiler.restoreCodeRevision(key, sources); restored != nil {
				t.Fatalf("corrupt cached %s selected a Player: %+v", kind, restored)
			}
		})
	}
}

func TestGoCodeRevisionTracksPinnedNetcodePayload(t *testing.T) {
	compiler, sources := goCodeRevisionFixture(t)
	directory := filepath.Join(compiler.cache, "fake-netcode")
	if err := os.MkdirAll(directory, 0700); err != nil {
		t.Fatal(err)
	}
	helper := filepath.Join(directory, "Unity.Netcode.Editor.CodeGen.dll")
	if err := os.WriteFile(helper, []byte("processed NGO helper"), 0600); err != nil {
		t.Fatal(err)
	}
	hash, _ := goFileHash(helper)
	compiler.netcode = &goNetcodeProcessor{Helper: helper, Files: map[string]string{filepath.Base(helper): hash}, Key: "verified-netcode-payload"}
	compiler.manifest.Assemblies[0].References = []string{"Unity.Netcode.Runtime"}
	key, err := compiler.codeRevisionKey(context.Background(), sources)
	if err != nil {
		t.Fatal(err)
	}
	compiler.netcode.Key = "new-verified-netcode-payload"
	if changed, err := compiler.codeRevisionKey(context.Background(), sources); err != nil || changed == key {
		t.Fatalf("changed NGO processed-output fingerprint did not invalidate graph: %s %v", changed, err)
	}
	if err := os.WriteFile(helper, []byte("tampered NGO payload"), 0600); err != nil {
		t.Fatal(err)
	}
	if _, err := compiler.codeRevisionKey(context.Background(), sources); err == nil || !strings.Contains(err.Error(), "dependency changed") {
		t.Fatalf("corrupt postprocessor allowed revision reuse: %v", err)
	}
}
