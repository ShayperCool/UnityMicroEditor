package main

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"
	"testing"
)

func TestGoScriptTypeInventoryReuseRequiresExactDLLAndBindings(t *testing.T) {
	compiler, _ := goCodeRevisionFixture(t)
	var plan goCompileManifest
	if err := json.Unmarshal([]byte(`{"scripts":[{"guid":"script-guid","path":"Game.cs","assembly":"Game"}]}`), &plan); err != nil {
		t.Fatal(err)
	}
	compiler.manifest.Scripts = plan.Scripts
	assemblies := map[string]string{"Game": compiler.currentDLLs["Game"]}
	key, err := compiler.scriptTypeCacheKey(compiler.helper, assemblies)
	if err != nil {
		t.Fatal(err)
	}
	expected := map[string]string{"script-guid": "Namespace.Game"}
	if err := compiler.saveScriptTypeCache(key, expected); err != nil {
		t.Fatal(err)
	}
	// The fixture's fake dotnet executable cannot run. Reuse must satisfy
	// ScriptTypeNames entirely from verified metadata produced previously.
	actual, err := compiler.ScriptTypeNames(context.Background())
	if err != nil || !reflect.DeepEqual(actual, expected) {
		t.Fatalf("unchanged defining DLL inventory was not reused: %v %v", actual, err)
	}
	compiler.manifest.Scripts[0].GUID = "changed-guid"
	if changed, err := compiler.scriptTypeCacheKey(compiler.helper, assemblies); err != nil || changed == key {
		t.Fatalf("changed GUID binding reused stale type inventory: %s %v", changed, err)
	}
	compiler.manifest.Scripts[0].GUID = "script-guid"
	compiler.expectedTypes = map[string]map[string]string{"Game": {"Namespace.Other": "script-guid"}}
	if changed, err := compiler.scriptTypeCacheKey(compiler.helper, assemblies); err != nil || changed == key {
		t.Fatalf("changed required type binding reused inventory: %s %v", changed, err)
	}
	compiler.expectedTypes = nil
	if err := os.WriteFile(compiler.currentDLLs["Game"], []byte("changed real DLL output"), 0600); err != nil {
		t.Fatal(err)
	}
	if changed, err := compiler.scriptTypeCacheKey(compiler.helper, assemblies); err != nil || changed == key {
		t.Fatalf("changed compiled DLL reused old type inventory: %s %v", changed, err)
	}
}

func TestGoScriptTypeInventoryRejectsCorruptionForeignGUIDAndSymlink(t *testing.T) {
	for _, kind := range []string{"corrupt", "foreign-guid", "symlink"} {
		t.Run(kind, func(t *testing.T) {
			compiler, _ := goCodeRevisionFixture(t)
			var plan goCompileManifest
			if err := json.Unmarshal([]byte(`{"scripts":[{"guid":"known-guid","assembly":"Game"}]}`), &plan); err != nil {
				t.Fatal(err)
			}
			compiler.manifest.Scripts = plan.Scripts
			key := "test-key"
			types := map[string]string{"known-guid": "Namespace.Game"}
			if kind == "foreign-guid" {
				types["foreign-guid"] = "Namespace.Other"
			}
			if err := compiler.saveScriptTypeCache(key, types); err != nil {
				t.Fatal(err)
			}
			path := filepath.Join(compiler.cache, "script-types", key, "types.json")
			switch kind {
			case "corrupt":
				if err := os.WriteFile(path, []byte(`{"known-guid":"Namespace.Corrupt"}`), 0600); err != nil {
					t.Fatal(err)
				}
			case "symlink":
				outside := filepath.Join(compiler.project, "unowned.json")
				if err := os.Rename(path, outside); err != nil {
					t.Fatal(err)
				}
				if err := os.Symlink(outside, path); err != nil {
					t.Fatal(err)
				}
			}
			if result := compiler.readScriptTypeCache(key); result != nil {
				t.Fatalf("corrupt cached %s was reused: %v", kind, result)
			}
		})
	}
}
