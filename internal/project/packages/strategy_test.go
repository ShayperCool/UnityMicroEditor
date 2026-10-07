package packages

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestRegistryStrategiesAndDirectMinimumUpgrade(t *testing.T) {
	for _, tc := range []struct {
		strategy, minimum, expected string
	}{
		{"lowest", "1.2.0", "1.2.0"},
		{"highestPatch", "1.2.0", "1.2.7"},
		{"highestMinor", "1.2.0", "1.3.0"},
		{"highest", "1.2.0", "2.0.0"},
		{"highestMinor", "0.2.0", "0.2.7"},
	} {
		t.Run(tc.strategy+tc.minimum, func(t *testing.T) {
			f := newRegistry(t)
			f.add(t, "com.example.root", "1.0.0", map[string]string{"com.example.dep": tc.minimum})
			for _, version := range []string{"0.2.0", "0.2.7", "0.3.0", "1.2.0", "1.2.7", "1.3.0", "2.0.0", "3.0.0-preview.1"} {
				f.add(t, "com.example.dep", version, nil)
			}
			root := projectFixture(t, nil)
			jsonFile(t, filepath.Join(root, "Packages", "manifest.json"), manifest{Dependencies: map[string]string{"com.example.root": "1.0.0"}, Registry: f.server.URL, ResolutionStrategy: tc.strategy})
			s, err := Resolve(context.Background(), root, "", Options{})
			if err != nil {
				t.Fatal(err)
			}
			if p := packageByName(t, s, "com.example.dep"); p.Version != tc.expected {
				t.Fatalf("expected %s, got %+v", tc.expected, p)
			}
		})
	}
	f := newRegistry(t)
	f.add(t, "com.example.root", "1.0.0", map[string]string{"com.example.dep": "1.3.0"})
	for _, version := range []string{"1.2.0", "1.3.0", "2.0.0"} {
		f.add(t, "com.example.dep", version, nil)
	}
	root := projectFixture(t, nil)
	jsonFile(t, filepath.Join(root, "Packages", "manifest.json"), manifest{Dependencies: map[string]string{"com.example.root": "1.0.0", "com.example.dep": "1.2.0"}, Registry: f.server.URL, ResolutionStrategy: "highest"})
	s, err := Resolve(context.Background(), root, "", Options{})
	if err != nil {
		t.Fatal(err)
	}
	if packageByName(t, s, "com.example.dep").Version != "1.3.0" {
		t.Fatal("direct minimum was treated as a pin or automatically maximized")
	}
}

func TestLockPreferenceAndDisabledLockPreserveAuthoredBytes(t *testing.T) {
	f := newRegistry(t)
	for _, version := range []string{"1.0.0", "1.1.0", "2.0.0-preview.1"} {
		f.add(t, "com.example.a", version, nil)
	}
	for _, tc := range []struct {
		lockVersion, expected string
		enabled               bool
	}{
		{"1.1.0", "1.1.0", true},
		{"1.1.0", "1.0.0", false},
		{"2.0.0-preview.1", "1.0.0", true},
	} {
		t.Run(tc.expected+tc.lockVersion, func(t *testing.T) {
			root := projectFixture(t, nil)
			jsonFile(t, filepath.Join(root, "Packages", "manifest.json"), manifest{Dependencies: map[string]string{"com.example.a": "1.0.0"}, Registry: f.server.URL, EnableLockFile: &tc.enabled})
			lockPath := filepath.Join(root, "Packages", "packages-lock.json")
			jsonFile(t, lockPath, lockFile{Dependencies: map[string]lockEntry{"com.example.a": {Version: tc.lockVersion, Source: "registry", URL: f.server.URL, Dependencies: map[string]string{}}}})
			before, _ := os.ReadFile(lockPath)
			s, err := Resolve(context.Background(), root, "", Options{})
			if err != nil {
				t.Fatal(err)
			}
			if packageByName(t, s, "com.example.a").Version != tc.expected {
				t.Fatalf("unexpected selected package %+v", s.Packages)
			}
			after, _ := os.ReadFile(lockPath)
			if string(before) != string(after) || !tc.enabled && !s.LockChanged {
				t.Fatal("authored lock changed or missing new proposal")
			}
		})
	}
}

func TestEmbeddedAdditionAndLocalSymlinkRetargetInvalidateCurrent(t *testing.T) {
	root := projectFixture(t, nil)
	s, err := Resolve(context.Background(), root, "", Options{Offline: true})
	if err != nil {
		t.Fatal(err)
	}
	packageFixture(t, filepath.Join(root, "Packages", "added-without-manifest-change"), "com.example.embedded", "1.0.0", nil)
	if err := s.Validate(context.Background()); err == nil {
		t.Fatal("added embedded package omitted from old snapshot")
	}
	first, second := t.TempDir(), t.TempDir()
	packageFixture(t, first, "com.example.local", "1.0.0", nil)
	packageFixture(t, second, "com.example.local", "1.0.0", nil)
	link := filepath.Join(t.TempDir(), "selected")
	if err := os.Symlink(first, link); err != nil {
		t.Skip(err)
	}
	jsonFile(t, filepath.Join(root, "Packages", "manifest.json"), manifest{Dependencies: map[string]string{"com.example.local": "file:" + link}})
	s, err = Resolve(context.Background(), root, "", Options{Offline: true})
	if err != nil {
		t.Fatal(err)
	}
	if err = os.Remove(link); err != nil {
		t.Fatal(err)
	}
	if err = os.Symlink(second, link); err != nil {
		t.Fatal(err)
	}
	if err = s.Validate(context.Background()); err == nil {
		t.Fatal("local package symlink retarget reused old physical package")
	}
}

func TestCacheBudgetAndUnmaterializedGitAreExplicitFailures(t *testing.T) {
	f := newRegistry(t)
	f.add(t, "com.example.a", "1.0.0", nil)
	root := projectFixture(t, nil)
	jsonFile(t, filepath.Join(root, "Packages", "manifest.json"), manifest{Dependencies: map[string]string{"com.example.a": "1.0.0"}, Registry: f.server.URL})
	if _, err := Resolve(context.Background(), root, "", Options{MaxCacheBytes: 32}); err == nil || !strings.Contains(err.Error(), "budget") {
		t.Fatalf("cache bound: %v", err)
	}
	for _, entry := range []string{"large.fbx", ".gitmodules"} {
		path := filepath.Join(t.TempDir(), "package")
		write(t, filepath.Join(path, entry), []byte("version https://git-lfs.github.com/spec/v1\noid sha256:test\nsize 1000\n"))
		if err := checkGitMaterialized(path); err == nil {
			t.Fatalf("unmaterialized %s accepted", entry)
		}
	}
}

func TestBuiltinMaterializedCopyRequiresEverySourceByte(t *testing.T) {
	root := projectFixture(t, map[string]string{"com.example.engine": "1.0.0"})
	editor := filepath.Join(t.TempDir(), "Unity")
	write(t, editor, []byte("explicit engine"))
	profile := filepath.Join(filepath.Dir(editor), "Data", "Resources", "PackageManager", "BuiltInPackages", "com.example.engine")
	cache := filepath.Join(root, "Library", "PackageCache", "arbitrary-name")
	packageFixture(t, profile, "com.example.engine", "1.0.0", nil)
	packageFixture(t, cache, "com.example.engine", "1.0.0", nil)
	data, _ := os.ReadFile(filepath.Join(profile, "package.json"))
	var value map[string]any
	if err := json.Unmarshal(data, &value); err != nil {
		t.Fatal(err)
	}
	value["_fingerprint"] = "not-a-proof"
	jsonFile(t, filepath.Join(cache, "package.json"), value)
	s, err := Resolve(context.Background(), root, editor, Options{Offline: true})
	if err != nil {
		t.Fatal(err)
	}
	p := packageByName(t, s, "com.example.engine")
	if p.Path != cache || p.ProfilePath != profile || p.ContentHash == p.ProfileContentHash || p.Proof != "verified_engine_cache_copy" {
		t.Fatalf("materialized engine proof %+v", p)
	}
	if _, err := LoadCurrent(context.Background(), root, editor, Options{}); err != nil {
		t.Fatal(err)
	}
	write(t, filepath.Join(cache, "Runtime", "Probe.cs"), []byte("public class Altered {}"))
	if _, err := Resolve(context.Background(), root, editor, Options{Offline: true}); err == nil {
		t.Fatal("altered builtin source accepted based on matching version/folder")
	}
}
