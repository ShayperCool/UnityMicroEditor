package main

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"

	unitypackages "unitymicroeditor/internal/project/packages"
)

func defaultsClosureProject(t *testing.T) (string, *unitypackages.Snapshot) {
	t.Helper()
	project := t.TempDir()
	for _, name := range []string{"Assets", "ProjectSettings"} {
		if err := os.MkdirAll(filepath.Join(project, name), 0700); err != nil {
			t.Fatal(err)
		}
	}
	return project, &unitypackages.Snapshot{SchemaVersion: 1, Project: project}
}

func defaultsClosureFile(t *testing.T, path, content string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(content), 0600); err != nil {
		t.Fatal(err)
	}
}

func defaultsClosureObject(fields string) string {
	return "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!114 &11400000\nSettings:\n" + fields
}

func defaultsClosureSeed(t *testing.T, project, fields string) map[string]string {
	t.Helper()
	logical := "ProjectSettings/GraphicsSettings.asset"
	content := defaultsClosureObject(fields)
	defaultsClosureFile(t, filepath.Join(project, filepath.FromSlash(logical)), content)
	return map[string]string{logical: bytesSHA([]byte(content))}
}

func defaultsClosureAsset(t *testing.T, project, logical, guid, fields string) {
	t.Helper()
	defaultsClosureFile(t, filepath.Join(project, filepath.FromSlash(logical)), defaultsClosureObject(fields))
	defaultsClosureFile(t, filepath.Join(project, filepath.FromSlash(logical))+".meta", "fileFormatVersion: 2\nguid: "+guid+"\nNativeFormatImporter:\n  externalObjects: {}\n")
}

func TestComponentDefaultsReferenceClosureTracksReachableAssetsAndCycles(t *testing.T) {
	project, snapshot := defaultsClosureProject(t)
	a, b, c := strings.Repeat("a", 32), strings.Repeat("b", 32), strings.Repeat("c", 32)
	defaultsClosureAsset(t, project, "Assets/Render.asset", a, "  material: {fileID: 2100000, guid: "+b+", type: 2}\n")
	defaultsClosureAsset(t, project, "Assets/Default.mat", b, "  back: {fileID: 11400000, guid: "+a+", type: 2}\n")
	defaultsClosureFile(t, filepath.Join(project, "Assets/Default.mat.meta"), "fileFormatVersion: 2\nguid: "+b+"\nImporter:\n  texture: {fileID: 2800000, guid: "+c+", type: 3}\n  userData: 'guid: not-a-reference'\n")
	defaultsClosureFile(t, filepath.Join(project, "Assets/Texture.png"), "binary texture leaf")
	defaultsClosureFile(t, filepath.Join(project, "Assets/Texture.png.meta"), "fileFormatVersion: 2\nguid: "+c+"\nTextureImporter:\n  externalObjects: {}\n")
	defaultsClosureAsset(t, project, "Assets/Unrelated.asset", strings.Repeat("d", 32), "  value: 1\n")
	seed := func() map[string]string {
		return defaultsClosureSeed(t, project, "  pipeline: {fileID: 11400000, guid: "+a+", type: 2}\n")
	}
	first := seed()
	if err := componentDefaultsReferenceClosure(context.Background(), project, snapshot, first); err != nil {
		t.Fatal(err)
	}
	if len(first) != 7 || first["Assets/Render.asset"] == "" || first["Assets/Texture.png.meta"] == "" || first["Assets/Unrelated.asset"] != "" {
		t.Fatal("closure contains unrelated or missing inputs", first)
	}
	defaultsClosureAsset(t, project, "Assets/Unrelated.asset", strings.Repeat("d", 32), "  value: 2\n")
	second := seed()
	if err := componentDefaultsReferenceClosure(context.Background(), project, snapshot, second); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(first, second) {
		t.Fatal("unrelated source changed the defaults profile")
	}
	defaultsClosureAsset(t, project, "Assets/Default.mat", b, "  back: {fileID: 11400000, guid: "+a+", type: 2}\n  changed: 1\n")
	third := seed()
	if err := componentDefaultsReferenceClosure(context.Background(), project, snapshot, third); err != nil {
		t.Fatal(err)
	}
	if first["Assets/Default.mat"] == third["Assets/Default.mat"] {
		t.Fatal("same-GUID material change did not invalidate profile")
	}
}

func TestComponentDefaultsReferenceClosureNoExternalGUIDDoesNotIndex(t *testing.T) {
	project, _ := defaultsClosureProject(t)
	if err := os.Symlink(t.TempDir(), filepath.Join(project, "Assets/Unsafe")); err != nil {
		t.Fatal(err)
	}
	inputs := defaultsClosureSeed(t, project, "  shader: {fileID: 69, guid: 0000000000000000f000000000000000, type: 0}\n  zero: {fileID: 0, guid: 00000000000000000000000000000000, type: 0}\n")
	before := len(inputs)
	if err := componentDefaultsReferenceClosure(context.Background(), project, nil, inputs); err != nil {
		t.Fatal(err)
	}
	if len(inputs) != before {
		t.Fatal("builtin-only profile changed")
	}
	if err := componentDefaultsReferenceClosure(context.Background(), project, nil, map[string]string{"ProjectSettings/QualitySettings.asset": "absent"}); err != nil {
		t.Fatal(err)
	}
	defaultsClosureFile(t, filepath.Join(project, "ProjectSettings/QualitySettings.asset"), defaultsClosureObject("  value: 1\n"))
	if err := componentDefaultsReferenceClosure(context.Background(), project, nil, map[string]string{"ProjectSettings/QualitySettings.asset": "absent"}); err == nil {
		t.Fatal("new settings file accepted against absent proof")
	}
}

func TestComponentDefaultsReferenceClosureRejectsUnsafeIdentityBeforePublication(t *testing.T) {
	for _, mode := range []string{"missing_asset", "duplicate_guid", "symlink", "binary_asset", "bad_nested_guid", "metadata_alias"} {
		t.Run(mode, func(t *testing.T) {
			project, snapshot := defaultsClosureProject(t)
			guid := strings.Repeat("a", 32)
			defaultsClosureAsset(t, project, "Assets/Pipeline.asset", guid, "  value: 1\n")
			switch mode {
			case "missing_asset":
				if err := os.Remove(filepath.Join(project, "Assets/Pipeline.asset")); err != nil {
					t.Fatal(err)
				}
			case "duplicate_guid":
				defaultsClosureAsset(t, project, "Assets/Duplicate.asset", guid, "  value: 1\n")
			case "symlink":
				if err := os.Symlink(t.TempDir(), filepath.Join(project, "Assets/Unsafe")); err != nil {
					t.Fatal(err)
				}
			case "binary_asset":
				defaultsClosureFile(t, filepath.Join(project, "Assets/Pipeline.asset"), "\x00binary serialized object")
			case "bad_nested_guid":
				defaultsClosureFile(t, filepath.Join(project, "Assets/Pipeline.asset.meta"), "guid: "+guid+"\nImporter:\n  ref: {guid: broken}\n")
			case "metadata_alias":
				defaultsClosureFile(t, filepath.Join(project, "Assets/Pipeline.asset.meta"), "guid: "+guid+"\nImporter:\n  ref: &x {value: 1}\n  alias: *x\n")
			}
			inputs := defaultsClosureSeed(t, project, "  pipeline: {fileID: 11400000, guid: "+guid+", type: 2}\n")
			before := map[string]string{}
			for key, value := range inputs {
				before[key] = value
			}
			if err := componentDefaultsReferenceClosure(context.Background(), project, snapshot, inputs); err == nil {
				t.Fatal("unsafe closure was accepted")
			}
			if !reflect.DeepEqual(inputs, before) {
				t.Fatal("failed capture partially published inputs")
			}
		})
	}
}

func TestComponentDefaultsReferenceClosureCapturesAbsenceAndInvalidatesNewGUID(t *testing.T) {
	project, snapshot := defaultsClosureProject(t)
	guid := strings.Repeat("a", 32)
	seed := func() map[string]string {
		return defaultsClosureSeed(t, project, "  optionalDebugShader: {fileID: 4800000, guid: "+guid+", type: 3}\n")
	}
	missing := seed()
	if err := componentDefaultsReferenceClosure(context.Background(), project, snapshot, missing); err != nil {
		t.Fatal(err)
	}
	if len(missing) != 2 || missing["settingsReferenceGuid/"+guid] != "absent" {
		t.Fatal("missing settings GUID did not produce explicit frozen absence", missing)
	}
	defaultsClosureAsset(t, project, "Assets/Resolved.asset", guid, "  value: 1\n")
	resolved := seed()
	if err := componentDefaultsReferenceClosure(context.Background(), project, snapshot, resolved); err != nil {
		t.Fatal(err)
	}
	if resolved["settingsReferenceGuid/"+guid] != "" || resolved["Assets/Resolved.asset"] == "" || reflect.DeepEqual(missing, resolved) {
		t.Fatal("newly imported GUID did not invalidate missing-state profile", resolved)
	}
}

func TestComponentDefaultsReferenceClosureUsesResolvedPackageAndStreamsModelIndex(t *testing.T) {
	project, snapshot := defaultsClosureProject(t)
	packageRoot := t.TempDir()
	guid := strings.Repeat("a", 32)
	defaultsClosureFile(t, filepath.Join(packageRoot, "Settings.asset"), defaultsClosureObject("  enabled: 1\n"))
	defaultsClosureFile(t, filepath.Join(packageRoot, "Settings.asset.meta"), "guid: "+guid+"\nImporter: {}\n")
	snapshot.Packages = []unitypackages.Package{{Name: "com.test.render", LogicalRoot: "Packages/com.test.render", Path: packageRoot, ContentHash: strings.Repeat("f", 64)}}
	defaultsClosureFile(t, filepath.Join(packageRoot, "Samples~/Duplicate.asset.meta"), "guid: "+guid+"\n")
	defaultsClosureFile(t, filepath.Join(project, "Assets/Unrelated.fbx.meta"), "guid: "+strings.Repeat("b", 32)+"\nModelImporter:\n  big: "+strings.Repeat("x", 1804405)+"\n")
	inputs := defaultsClosureSeed(t, project, "  pipeline: {fileID: 11400000, guid: "+guid+", type: 2}\n")
	if err := componentDefaultsReferenceClosure(context.Background(), project, snapshot, inputs); err != nil {
		t.Fatal(err)
	}
	if len(inputs) != 3 || inputs["Packages/com.test.render/Settings.asset"] == "" {
		t.Fatal("resolved package closure identity missing", inputs)
	}
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if err := componentDefaultsReferenceClosure(ctx, project, snapshot, inputs); !errors.Is(err, context.Canceled) {
		t.Fatal("cancellation was not returned", err)
	}
}

func TestComponentDefaultsReferenceClosureActualOwnedProject(t *testing.T) {
	project := os.Getenv("UNITY_MICRO_EDITOR_CLOSURE_PROJECT")
	if project == "" {
		t.Skip("explicit read-only owned project not supplied")
	}
	editor := os.Getenv("UNITY_MICRO_EDITOR_CLOSURE_EDITOR")
	if editor == "" {
		t.Fatal("explicit Editor path required for package identity; no Editor execution")
	}
	snapshot, err := unitypackages.Resolve(context.Background(), project, editor, unitypackages.Options{Offline: true})
	if err != nil {
		t.Fatal(err)
	}
	inputs := map[string]string{}
	for _, name := range []string{"GraphicsSettings.asset", "QualitySettings.asset", "PhysicsManager.asset", "Physics2DSettings.asset", "ProjectSettings.asset"} {
		logical := "ProjectSettings/" + name
		if _, err := os.Stat(filepath.Join(project, filepath.FromSlash(logical))); errors.Is(err, os.ErrNotExist) {
			inputs[logical] = "absent"
			continue
		}
		hash, err := goFileHash(filepath.Join(project, filepath.FromSlash(logical)))
		if err != nil {
			t.Fatal(err)
		}
		inputs[logical] = hash
	}
	if err := componentDefaultsReferenceClosure(context.Background(), project, snapshot, inputs); err != nil {
		t.Fatal(err)
	}
	missing := 0
	for key, value := range inputs {
		if strings.HasPrefix(key, "settingsReferenceGuid/") && value == "absent" {
			missing++
		}
	}
	t.Logf("read-only defaults closure inputs=%d explicitlyMissingSettingsGUIDs=%d", len(inputs), missing)
}
