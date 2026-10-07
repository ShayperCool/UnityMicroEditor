package packages

import (
	"archive/tar"
	"bytes"
	"compress/gzip"
	"context"
	"crypto/sha512"
	"encoding/base64"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

func write(t *testing.T, path string, data []byte) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, data, 0600); err != nil {
		t.Fatal(err)
	}
}
func jsonFile(t *testing.T, path string, value any) {
	t.Helper()
	data, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	write(t, path, data)
}
func projectFixture(t *testing.T, deps map[string]string) string {
	t.Helper()
	root := t.TempDir()
	if err := os.Mkdir(filepath.Join(root, "Assets"), 0700); err != nil {
		t.Fatal(err)
	}
	jsonFile(t, filepath.Join(root, "Packages", "manifest.json"), manifest{Dependencies: deps})
	return root
}
func packageFixture(t *testing.T, path, name, version string, deps map[string]string) {
	t.Helper()
	jsonFile(t, filepath.Join(path, "package.json"), packageManifest{Name: name, Version: version, Dependencies: deps})
	write(t, filepath.Join(path, "Runtime", "Probe.cs"), []byte("public class Probe {}\n"))
}
func packageByName(t *testing.T, s *Snapshot, name string) Package {
	t.Helper()
	for _, p := range s.Packages {
		if p.Name == name {
			return p
		}
	}
	t.Fatalf("missing %s", name)
	return Package{}
}
func TestLocalAndEmbeddedChangesInvalidateWithoutAuthoredWrites(t *testing.T) {
	local := t.TempDir()
	packageFixture(t, local, "com.example.local", "1.0.0", nil)
	root := projectFixture(t, map[string]string{"com.example.local": "file:" + local, "com.example.embedded": "9.0.0"})
	packageFixture(t, filepath.Join(root, "Packages", "any-folder"), "com.example.embedded", "1.2.3", nil)
	before, _ := os.ReadFile(filepath.Join(root, "Packages", "manifest.json"))
	s, err := Resolve(context.Background(), root, "", Options{Offline: true})
	if err != nil {
		t.Fatal(err)
	}
	if len(s.Packages) != 2 || !s.LockChanged || s.ProposedLockPath == "" {
		t.Fatalf("invalid snapshot %+v", s)
	}
	if p := packageByName(t, s, "com.example.embedded"); p.Source != "embedded" || p.Version != "1.2.3" {
		t.Fatal(p)
	}
	after, _ := os.ReadFile(filepath.Join(root, "Packages", "manifest.json"))
	if !bytes.Equal(before, after) {
		t.Fatal("authored manifest changed")
	}
	if _, err = os.Stat(filepath.Join(root, "Packages", "packages-lock.json")); !os.IsNotExist(err) {
		t.Fatal("authored lock created")
	}
	write(t, filepath.Join(local, "Runtime", "Probe.cs"), []byte("public class Probe { public int changed; }"))
	if err = s.Validate(context.Background()); err == nil {
		t.Fatal("changed local C# ignored")
	}
	next, err := Resolve(context.Background(), root, "", Options{Offline: true})
	if err != nil {
		t.Fatal(err)
	}
	if next.ResolutionHash == s.ResolutionHash {
		t.Fatal("changed package content reused old snapshot")
	}
	jsonFile(t, filepath.Join(root, "Packages", "packages-lock.json"), lockFile{})
	if err = next.Validate(context.Background()); err == nil {
		t.Fatal("previously absent lock change ignored")
	}
}
func TestBuiltinPrecedenceAndFolderIndependentCacheIdentity(t *testing.T) {
	root := projectFixture(t, map[string]string{"com.example.engine": "1.0.0", "com.example.cached": "2.0.0"})
	editor := filepath.Join(t.TempDir(), "Unity")
	write(t, editor, []byte("explicit editor profile"))
	packageFixture(t, filepath.Join(filepath.Dir(editor), "Data", "Resources", "PackageManager", "BuiltInPackages", "arbitrary-folder"), "com.example.engine", "3.0.0", map[string]string{"com.example.cached": "1.0.0"})
	packageFixture(t, filepath.Join(root, "Library", "PackageCache", "folder-does-not-prove-identity"), "com.example.cached", "2.0.0", nil)
	s, err := Resolve(context.Background(), root, editor, Options{Offline: true})
	if err != nil {
		t.Fatal(err)
	}
	p := packageByName(t, s, "com.example.engine")
	if p.Source != "builtin" || p.Version != "3.0.0" || p.Proof != "engine_builtin" {
		t.Fatal(p)
	}
	if packageByName(t, s, "com.example.cached").Version != "2.0.0" {
		t.Fatal("cache manifest identity not used")
	}
}
func archive(t *testing.T, files map[string][]byte, special *tar.Header) []byte {
	t.Helper()
	var data bytes.Buffer
	gz := gzip.NewWriter(&data)
	tw := tar.NewWriter(gz)
	for _, name := range sortedKeys(files) {
		value := files[name]
		if err := tw.WriteHeader(&tar.Header{Name: name, Size: int64(len(value)), Mode: 0600, Typeflag: tar.TypeReg}); err != nil {
			t.Fatal(err)
		}
		if _, err := tw.Write(value); err != nil {
			t.Fatal(err)
		}
	}
	if special != nil {
		if err := tw.WriteHeader(special); err != nil {
			t.Fatal(err)
		}
	}
	if err := tw.Close(); err != nil {
		t.Fatal(err)
	}
	if err := gz.Close(); err != nil {
		t.Fatal(err)
	}
	return data.Bytes()
}

type registryFixture struct {
	server         *httptest.Server
	archives       map[string][]byte
	versions       map[string]map[string]packageManifest
	wrongIntegrity bool
	requests       int
}

func newRegistry(t *testing.T) *registryFixture {
	t.Helper()
	f := &registryFixture{archives: map[string][]byte{}, versions: map[string]map[string]packageManifest{}}
	f.server = httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		f.requests++
		key := strings.TrimPrefix(r.URL.Path, "/")
		if strings.HasPrefix(key, "tar/") {
			data, ok := f.archives[strings.TrimPrefix(key, "tar/")]
			if !ok {
				http.NotFound(w, r)
				return
			}
			w.Write(data)
			return
		}
		versions, ok := f.versions[key]
		if !ok {
			http.NotFound(w, r)
			return
		}
		result := registryMetadata{Name: key, Versions: map[string]registryVersion{}}
		for version, m := range versions {
			entry := registryVersion{Name: key, Version: version, Dependencies: m.Dependencies}
			entry.Dist.Tarball = f.server.URL + "/tar/" + key + "@" + version
			sum := sha512.Sum512(f.archives[key+"@"+version])
			if f.wrongIntegrity {
				sum[0] ^= 1
			}
			entry.Dist.Integrity = "sha512-" + base64.StdEncoding.EncodeToString(sum[:])
			result.Versions[version] = entry
		}
		json.NewEncoder(w).Encode(result)
	}))
	t.Cleanup(f.server.Close)
	return f
}
func (f *registryFixture) add(t *testing.T, name, version string, deps map[string]string) {
	t.Helper()
	m := packageManifest{Name: name, Version: version, Dependencies: deps}
	if f.versions[name] == nil {
		f.versions[name] = map[string]packageManifest{}
	}
	f.versions[name][version] = m
	body, _ := json.Marshal(m)
	f.archives[name+"@"+version] = archive(t, map[string][]byte{"package/package.json": body, "package/Runtime/Source.cs": []byte("public class PackageProbe {}")}, nil)
}
func TestRegistryClosureIntegrityLockProposalAndOfflineReplay(t *testing.T) {
	f := newRegistry(t)
	f.add(t, "com.example.a", "1.0.0", map[string]string{"com.example.b": "1.0.0"})
	f.add(t, "com.example.b", "1.0.0", nil)
	root := projectFixture(t, nil)
	jsonFile(t, filepath.Join(root, "Packages", "manifest.json"), manifest{Dependencies: map[string]string{"com.example.a": "1.0.0"}, Registry: f.server.URL})
	s, err := Resolve(context.Background(), root, "", Options{})
	if err != nil {
		t.Fatal(err)
	}
	if len(s.Packages) != 2 || packageByName(t, s, "com.example.b").Depth != 1 || packageByName(t, s, "com.example.a").Proof != "verified_registry_integrity" {
		t.Fatal(s)
	}
	if _, err = os.Stat(filepath.Join(root, "Packages", "packages-lock.json")); !os.IsNotExist(err) {
		t.Fatal("lock silently published")
	}
	count := f.requests
	replay, err := Resolve(context.Background(), root, "", Options{Offline: true})
	if err != nil {
		t.Fatal(err)
	}
	if replay.ResolutionHash != s.ResolutionHash || f.requests != count {
		t.Fatal("offline replay fetched network")
	}
	current, err := LoadCurrent(context.Background(), root, "", Options{})
	if err != nil || current.ResolutionHash != s.ResolutionHash {
		t.Fatalf("current immutable snapshot: %v", err)
	}
	write(t, filepath.Join(packageByName(t, replay, "com.example.a").Path, "Runtime", "Source.cs"), []byte("tampered"))
	if err = replay.Validate(context.Background()); err == nil {
		t.Fatal("cache mutation not detected")
	}
	if _, err = LoadCurrent(context.Background(), root, "", Options{}); err == nil {
		t.Fatal("current snapshot ignored changed content")
	}
}
func TestRegistryIntegrityAndUnsafeArchivesRejectBeforeSnapshot(t *testing.T) {
	for _, kind := range []string{"integrity", "traversal", "symlink", "download", "expanded"} {
		t.Run(kind, func(t *testing.T) {
			f := newRegistry(t)
			f.add(t, "com.example.a", "1.0.0", nil)
			if kind == "integrity" {
				f.wrongIntegrity = true
			}
			if kind == "traversal" {
				f.archives["com.example.a@1.0.0"] = archive(t, map[string][]byte{"../escape": []byte("bad")}, nil)
			}
			if kind == "symlink" {
				f.archives["com.example.a@1.0.0"] = archive(t, nil, &tar.Header{Name: "package/link", Linkname: "/etc/passwd", Typeflag: tar.TypeSymlink})
			}
			root := projectFixture(t, nil)
			jsonFile(t, filepath.Join(root, "Packages", "manifest.json"), manifest{Dependencies: map[string]string{"com.example.a": "1.0.0"}, Registry: f.server.URL})
			options := Options{}
			if kind == "download" {
				options.MaxDownloadBytes = 10
			}
			if kind == "expanded" {
				options.MaxExpandedBytes = 10
			}
			if _, err := Resolve(context.Background(), root, "", options); err == nil {
				t.Fatal("invalid archive accepted")
			}
			entries, _ := os.ReadDir(filepath.Join(root, "Library", "UnityMicroEditor", "packages", "snapshots"))
			if len(entries) != 0 {
				t.Fatal("failed package snapshot published")
			}
		})
	}
}
func TestScopedRegistryUsesLongestNamespaceAndRejectsAmbiguity(t *testing.T) {
	a, b := newRegistry(t), newRegistry(t)
	b.add(t, "com.example.tools.a", "1.0.0", nil)
	root := projectFixture(t, nil)
	m := manifest{Dependencies: map[string]string{"com.example.tools.a": "1.0.0"}}
	m.ScopedRegistries = append(m.ScopedRegistries, struct {
		Name   string   `json:"name"`
		URL    string   `json:"url"`
		Scopes []string `json:"scopes"`
	}{"general", a.server.URL, []string{"com.example"}}, struct {
		Name   string   `json:"name"`
		URL    string   `json:"url"`
		Scopes []string `json:"scopes"`
	}{"specific", b.server.URL, []string{"com.example.tools"}})
	jsonFile(t, filepath.Join(root, "Packages", "manifest.json"), m)
	if _, err := Resolve(context.Background(), root, "", Options{}); err != nil {
		t.Fatal(err)
	}
	if a.requests != 0 || b.requests == 0 {
		t.Fatal("wrong registry selected")
	}
	m.ScopedRegistries[0].Scopes = []string{"com.example.tools"}
	jsonFile(t, filepath.Join(root, "Packages", "manifest.json"), m)
	if _, err := Resolve(context.Background(), root, "", Options{}); err == nil {
		t.Fatal("ambiguous scope accepted")
	}
}
func TestCyclesAndDirectVersionConflictsFail(t *testing.T) {
	for _, cycle := range []bool{false, true} {
		root := projectFixture(t, map[string]string{"com.example.a": "1.0.0", "com.example.b": "1.0.0"})
		dep := "2.0.0"
		if cycle {
			dep = "1.0.0"
		}
		packageFixture(t, filepath.Join(root, "Library", "PackageCache", "a"), "com.example.a", "1.0.0", map[string]string{"com.example.b": dep})
		deps := map[string]string{}
		if cycle {
			deps["com.example.a"] = "1.0.0"
		}
		packageFixture(t, filepath.Join(root, "Library", "PackageCache", "b"), "com.example.b", "1.0.0", deps)
		if _, err := Resolve(context.Background(), root, "", Options{Offline: true}); err == nil {
			t.Fatal("cycle/conflict accepted")
		}
	}
}
func TestSemVerOrderingAndUnsupportedRange(t *testing.T) {
	ordered := []string{"1.0.0-alpha", "1.0.0-alpha.1", "1.0.0-alpha.beta", "1.0.0-beta", "1.0.0-beta.2", "1.0.0-beta.11", "1.0.0-rc.1", "1.0.0", "1.0.1"}
	for i := 1; i < len(ordered); i++ {
		if compareVersion(ordered[i-1], ordered[i]) >= 0 {
			t.Fatalf("bad SemVer order %v", ordered)
		}
	}
	for _, invalid := range []string{"^1.0.0", "1.0", "01.0.0", "1.0.0-01"} {
		if _, err := parseVersion(invalid); err == nil {
			t.Fatalf("invalid version %s", invalid)
		}
	}
	if compareVersion("99999999999999999999999999999999.0.0", "100000000000000000000000000000000.0.0") >= 0 || compareVersion("1.0.0+build.1", "1.0.0+build.2") != 0 {
		t.Fatal("SemVer numeric overflow or build metadata changed precedence")
	}
}
func git(t *testing.T, dir string, args ...string) string {
	t.Helper()
	cmd := exec.Command("git", append([]string{"-C", dir}, args...)...)
	output, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("git %v: %v %s", args, err, output)
	}
	return strings.TrimSpace(string(output))
}
func TestGitLockedCommitAndSubdirectoryAreVerified(t *testing.T) {
	repo := t.TempDir()
	git(t, repo, "init")
	packageFixture(t, filepath.Join(repo, "Packages", "Sub"), "com.example.git", "1.0.0", nil)
	git(t, repo, "add", ".")
	git(t, repo, "-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid", "commit", "-m", "one")
	commit := git(t, repo, "rev-parse", "HEAD")
	git(t, repo, "tag", "stable")
	request := "git+file://" + repo + "?path=Packages/Sub#stable"
	root := projectFixture(t, map[string]string{"com.example.git": request})
	jsonFile(t, filepath.Join(root, "Packages", "packages-lock.json"), lockFile{Dependencies: map[string]lockEntry{"com.example.git": {Version: request, Source: "git", Hash: commit, Dependencies: map[string]string{}}}})
	write(t, filepath.Join(repo, "Packages", "Sub", "Runtime", "Probe.cs"), []byte("public class Newer {}"))
	git(t, repo, "add", ".")
	git(t, repo, "-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid", "commit", "-m", "two")
	git(t, repo, "tag", "-f", "stable")
	s, err := Resolve(context.Background(), root, "", Options{})
	if err != nil {
		t.Fatal(err)
	}
	p := packageByName(t, s, "com.example.git")
	if p.Revision != commit || p.Subdirectory != "Packages/Sub" || p.Proof != "verified_git_commit" {
		t.Fatal(p)
	}
	data, _ := os.ReadFile(filepath.Join(p.Path, "Runtime", "Probe.cs"))
	if strings.Contains(string(data), "Newer") {
		t.Fatal("moved tag replaced locked commit")
	}
	if _, err = Resolve(context.Background(), root, "", Options{Offline: true}); err != nil {
		t.Fatal(err)
	}
	cache := filepath.Join(root, "Library", "PackageCache", "git-folder-name-is-not-evidence")
	packageFixture(t, cache, "com.example.git", "1.0.0", nil)
	manifestData, _ := os.ReadFile(filepath.Join(p.Path, "package.json"))
	var manifestValue map[string]any
	if err = json.Unmarshal(manifestData, &manifestValue); err != nil {
		t.Fatal(err)
	}
	manifestValue["_fingerprint"] = "not-the-commit"
	jsonFile(t, filepath.Join(cache, "package.json"), manifestValue)
	materialized, err := Resolve(context.Background(), root, "", Options{Offline: true})
	if err != nil {
		t.Fatal(err)
	}
	copy := packageByName(t, materialized, "com.example.git")
	if copy.Path != cache || copy.CommitPath != p.Path || copy.CommitContentHash != p.ContentHash || copy.Revision != commit || copy.Proof != "verified_git_cache_copy" {
		t.Fatal(copy)
	}
	if _, err = LoadCurrent(context.Background(), root, "", Options{}); err != nil {
		t.Fatal(err)
	}
	write(t, filepath.Join(cache, "Runtime", "Probe.cs"), []byte("public class WrongCommit {}"))
	if err = materialized.Validate(context.Background()); err == nil {
		t.Fatal("materialized Git content change ignored")
	}
	recovered, err := Resolve(context.Background(), root, "", Options{Offline: true})
	if err != nil || packageByName(t, recovered, "com.example.git").Path != p.Path {
		t.Fatalf("verified commit recovery: %v", err)
	}
}
