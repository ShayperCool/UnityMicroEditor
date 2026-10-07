package packages

import (
	"archive/tar"
	"compress/gzip"
	"crypto/sha1"
	"crypto/sha256"
	"crypto/sha512"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"hash"
	"io"
	"net/http"
	"net/url"
	"os"
	"path"
	"path/filepath"
	"sort"
	"strings"
)

type registryVersion struct {
	Name         string            `json:"name"`
	Version      string            `json:"version"`
	Dependencies map[string]string `json:"dependencies"`
	Dist         struct {
		Tarball   string `json:"tarball"`
		Integrity string `json:"integrity"`
		Shasum    string `json:"shasum"`
	} `json:"dist"`
}
type registryMetadata struct {
	Name     string                     `json:"name"`
	Versions map[string]registryVersion `json:"versions"`
}
type contentReceipt struct{ Name, Version, Source, ContentHash, Proof, Registry string }

func (r *resolver) publishContentReceipt(p Package) error {
	return atomicJSON(filepath.Join(r.cache, "receipts", p.ContentHash+".json"), contentReceipt{p.Name, p.Version, p.Source, p.ContentHash, p.Proof, p.Registry})
}

func safeHTTP(value string) (*url.URL, error) {
	u, err := url.Parse(value)
	if err != nil {
		return nil, err
	}
	if (u.Scheme != "https" && u.Scheme != "http") || u.Host == "" || u.User != nil {
		return nil, errors.New("registry/download URL must be HTTP(S) without embedded credentials")
	}
	return u, nil
}
func (r *resolver) registryFor(name string) (string, error) {
	registry := r.manifest.Registry
	if registry == "" {
		registry = "https://packages.unity.com"
	}
	best := -1
	for _, item := range r.manifest.ScopedRegistries {
		if item.Name == "" || len(item.Scopes) == 0 {
			return "", errors.New("scoped registry requires a name and scopes")
		}
		for _, scope := range item.Scopes {
			if !packageNamePattern.MatchString(scope) {
				return "", fmt.Errorf("unsupported registry scope %q", scope)
			}
			if name == scope || strings.HasPrefix(name, scope+".") {
				if len(scope) == best && strings.TrimRight(registry, "/") != strings.TrimRight(item.URL, "/") {
					return "", fmt.Errorf("ambiguous scoped registry for %s", name)
				}
				if len(scope) > best {
					registry = item.URL
					best = len(scope)
				}
			}
		}
	}
	u, err := safeHTTP(registry)
	if err != nil {
		return "", err
	}
	if u.RawQuery != "" || u.Fragment != "" {
		return "", errors.New("registry URL query/fragment unsupported")
	}
	return strings.TrimRight(u.String(), "/"), nil
}
func (r *resolver) get(address, registry string) (*http.Response, error) {
	u, err := safeHTTP(address)
	if err != nil {
		return nil, err
	}
	req, err := http.NewRequestWithContext(r.ctx, http.MethodGet, u.String(), nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Accept", "application/json")
	req.Header.Set("User-Agent", "UnityMicroEditor-PackageResolver/1")
	base, _ := safeHTTP(registry)
	if base != nil && base.Scheme == u.Scheme && base.Host == u.Host && strings.HasPrefix(u.Path, strings.TrimRight(base.Path, "/")+"/") {
		if token := r.options.RegistryTokens[registry]; token != "" {
			req.Header.Set("Authorization", "Bearer "+token)
		}
	}
	client := r.options.HTTPClient
	if client == nil {
		client = &http.Client{Timeout: 90 * 1e9}
	}
	// Keep a caller's transport and redirect policy, without allowing a registry
	// bearer token to follow redirects into a different host or URL namespace.
	copyClient := *client
	copyClient.CheckRedirect = func(next *http.Request, via []*http.Request) error {
		if len(via) >= 10 {
			return errors.New("package redirect limit exceeded")
		}
		if _, err := safeHTTP(next.URL.String()); err != nil {
			return err
		}
		if base == nil || next.URL.Scheme != base.Scheme || next.URL.Host != base.Host || !strings.HasPrefix(next.URL.Path, strings.TrimRight(base.Path, "/")+"/") {
			next.Header.Del("Authorization")
		}
		if client.CheckRedirect != nil {
			return client.CheckRedirect(next, via)
		}
		return nil
	}
	response, err := copyClient.Do(req)
	if err != nil {
		return nil, err
	}
	if response.StatusCode != http.StatusOK {
		response.Body.Close()
		if response.StatusCode == 401 || response.StatusCode == 403 {
			return nil, fmt.Errorf("registry authentication required (%d) for %s", response.StatusCode, u.Host)
		}
		return nil, fmt.Errorf("package fetch HTTP %d from %s", response.StatusCode, u.Host)
	}
	return response, nil
}
func (r *resolver) packument(name string) (registryMetadata, error) {
	registry, err := r.registryFor(name)
	if err != nil {
		return registryMetadata{}, err
	}
	key := registry + "/" + name
	if m, ok := r.metadata[key]; ok {
		return m, nil
	}
	cachePath := filepath.Join(r.cache, "metadata", hashBytes([]byte(key))+".json")
	var data []byte
	if r.options.Offline {
		data, err = boundedRead(cachePath, maxJSON)
		if err != nil {
			return registryMetadata{}, fmt.Errorf("offline registry metadata unavailable for %s", name)
		}
	} else {
		response, err := r.get(key, registry)
		if err != nil {
			return registryMetadata{}, err
		}
		defer response.Body.Close()
		data, err = io.ReadAll(io.LimitReader(response.Body, maxJSON+1))
		if err != nil {
			return registryMetadata{}, err
		}
		if len(data) > maxJSON {
			return registryMetadata{}, errors.New("registry metadata exceeds bounds")
		}
	}
	var m registryMetadata
	if err = json.Unmarshal(data, &m); err != nil {
		return m, err
	}
	if m.Name != name || len(m.Versions) == 0 {
		return m, fmt.Errorf("registry identity/versions mismatch for %s", name)
	}
	for v, entry := range m.Versions {
		if _, err := parseVersion(v); err != nil {
			continue
		}
		if entry.Name != "" && entry.Name != name || entry.Version != "" && entry.Version != v {
			return m, fmt.Errorf("registry version identity mismatch for %s@%s", name, v)
		}
	}
	if !r.options.Offline {
		if err = r.checkCacheBudget(int64(len(data))); err != nil {
			return m, err
		}
		if err = atomicBytes(cachePath, data); err != nil {
			return m, err
		}
	}
	r.metadata[key] = m
	return m, nil
}
func (r *resolver) chooseRegistryVersion(name, minimum, strategy string) (string, error) {
	m, err := r.packument(name)
	if err != nil {
		return "", err
	}
	floor, _ := parseVersion(minimum)
	var candidates []string
	for candidate := range m.Versions {
		v, err := parseVersion(candidate)
		if err != nil || compareVersion(candidate, minimum) < 0 {
			continue
		}
		if len(floor.pre) == 0 && len(v.pre) > 0 {
			continue
		}
		if strategy == "highestPatch" && (v.major != floor.major || v.minor != floor.minor) {
			continue
		}
		if strategy == "highestMinor" && (v.major != floor.major || floor.major == "0" && v.minor != floor.minor) {
			continue
		}
		candidates = append(candidates, candidate)
	}
	if len(candidates) == 0 {
		return "", fmt.Errorf("no registry version satisfies %s >=%s (%s)", name, minimum, strategy)
	}
	sort.Slice(candidates, func(i, j int) bool {
		c := compareVersion(candidates[i], candidates[j])
		if c == 0 {
			return candidates[i] < candidates[j]
		}
		return c < 0
	})
	if strategy == "lowest" {
		return candidates[0], nil
	}
	return candidates[len(candidates)-1], nil
}
func (r *resolver) registryPackage(name, version, requested, registry string) (Package, error) {
	m, err := r.packument(name)
	if err != nil {
		return Package{}, err
	}
	entry, ok := m.Versions[version]
	if !ok {
		return Package{}, fmt.Errorf("registry version absent: %s@%s", name, version)
	}
	if entry.Dist.Tarball == "" {
		return Package{}, fmt.Errorf("registry tarball missing: %s@%s", name, version)
	}
	if entry.Dist.Integrity == "" && entry.Dist.Shasum == "" {
		return Package{}, fmt.Errorf("registry integrity missing: %s@%s", name, version)
	}
	if err = r.checkCacheBudget(r.options.MaxDownloadBytes + r.options.MaxExpandedBytes); err != nil {
		return Package{}, err
	}
	response, err := r.get(entry.Dist.Tarball, registry)
	if err != nil {
		return Package{}, err
	}
	defer response.Body.Close()
	f, err := os.CreateTemp(r.cache, ".download-")
	if err != nil {
		return Package{}, err
	}
	defer os.Remove(f.Name())
	written, err := io.Copy(f, io.LimitReader(response.Body, r.options.MaxDownloadBytes+1))
	if err == nil && written > r.options.MaxDownloadBytes {
		err = errors.New("package download exceeds byte bound")
	}
	if closeErr := f.Close(); err == nil {
		err = closeErr
	}
	if err != nil {
		return Package{}, err
	}
	if err = verifyIntegrity(f.Name(), entry.Dist.Integrity, entry.Dist.Shasum); err != nil {
		return Package{}, err
	}
	p, err := r.unpackFile(name, f.Name(), "registry", requested, "verified_registry_integrity")
	if err != nil {
		return p, err
	}
	if p.Version != version {
		return p, fmt.Errorf("downloaded version mismatch: %s@%s", name, p.Version)
	}
	p.Registry = registry
	if !sameDependencies(p.Dependencies, entry.Dependencies) {
		return p, fmt.Errorf("registry/archive dependency mismatch for %s@%s", name, version)
	}
	if err = r.publishContentReceipt(p); err != nil {
		return p, err
	}
	if r.caches[name] == nil {
		r.caches[name] = map[string][]string{}
	}
	r.caches[name][p.Version] = append(r.caches[name][p.Version], p.Path)
	return p, nil
}
func sameDependencies(a, b map[string]string) bool {
	if len(a) != len(b) {
		return false
	}
	for k, v := range a {
		if b[k] != v {
			return false
		}
	}
	return true
}
func verifyIntegrity(filename, integrity, shasum string) error {
	var h hash.Hash
	var expected []byte
	var err error
	if integrity != "" {
		best := 0
		for _, value := range strings.Fields(integrity) {
			algorithm, encoded, ok := strings.Cut(value, "-")
			if !ok {
				continue
			}
			rank := map[string]int{"sha1": 1, "sha256": 2, "sha384": 3, "sha512": 4}[algorithm]
			if rank <= best {
				continue
			}
			decoded, e := base64.StdEncoding.DecodeString(encoded)
			if e != nil {
				return e
			}
			expected = decoded
			best = rank
			switch algorithm {
			case "sha1":
				h = sha1.New()
			case "sha256":
				h = sha256.New()
			case "sha384":
				h = sha512.New384()
			case "sha512":
				h = sha512.New()
			}
		}
		if best == 0 {
			return errors.New("unsupported registry integrity algorithm")
		}
	} else {
		h = sha1.New()
		expected, err = hex.DecodeString(shasum)
		if err != nil {
			return err
		}
	}
	f, err := os.Open(filename)
	if err != nil {
		return err
	}
	defer f.Close()
	if _, err = io.Copy(h, f); err != nil {
		return err
	}
	if !strings.EqualFold(hex.EncodeToString(h.Sum(nil)), hex.EncodeToString(expected)) {
		return errors.New("package archive integrity mismatch")
	}
	return nil
}
func (r *resolver) localTarball(name, filename, requested string) (Package, error) {
	if err := r.checkCacheBudget(r.options.MaxExpandedBytes); err != nil {
		return Package{}, err
	}
	data, err := boundedRead(filename, r.options.MaxDownloadBytes)
	if err != nil {
		return Package{}, err
	}
	r.inputs[filename] = hashBytes(data)
	return r.unpackFile(name, filename, "local", requested, "verified_local_archive")
}
func (r *resolver) unpackFile(name, filename, source, requested, proof string) (Package, error) {
	f, err := os.Open(filename)
	if err != nil {
		return Package{}, err
	}
	defer f.Close()
	gz, err := gzip.NewReader(f)
	if err != nil {
		return Package{}, err
	}
	defer gz.Close()
	return r.extract(name, gz, source, requested, proof, true)
}
func (r *resolver) extract(name string, reader io.Reader, source, requested, proof string, stripPackage bool) (Package, error) {
	stage, err := os.MkdirTemp(r.cache, ".extract-")
	if err != nil {
		return Package{}, err
	}
	defer os.RemoveAll(stage)
	tr := tar.NewReader(reader)
	var total int64
	files := 0
	seen := map[string]bool{}
	for {
		if err = r.ctx.Err(); err != nil {
			return Package{}, err
		}
		header, err := tr.Next()
		if err == io.EOF {
			break
		}
		if err != nil {
			return Package{}, err
		}
		relative := header.Name
		if stripPackage {
			relative = strings.TrimPrefix(relative, "package/")
		}
		relative = strings.TrimSuffix(relative, "/")
		if relative == "" || relative == "package" && header.Typeflag == tar.TypeDir {
			continue
		}
		clean := path.Clean(relative)
		if clean == ".." || strings.HasPrefix(clean, "../") || path.IsAbs(clean) || strings.Contains(clean, "\\") || strings.Contains(clean, ":") || clean != relative {
			return Package{}, fmt.Errorf("unsafe package archive path %q", relative)
		}
		destination := filepath.Join(stage, filepath.FromSlash(clean))
		if !inside(stage, destination) {
			return Package{}, errors.New("archive path escape")
		}
		if header.Typeflag == tar.TypeDir {
			if err = os.MkdirAll(destination, 0700); err != nil {
				return Package{}, err
			}
			continue
		}
		if header.Typeflag != tar.TypeReg && header.Typeflag != tar.TypeRegA {
			return Package{}, fmt.Errorf("package archive link/special entry unsupported: %s", relative)
		}
		if seen[clean] {
			return Package{}, fmt.Errorf("duplicate archive file %s", clean)
		}
		seen[clean] = true
		files++
		total += header.Size
		if header.Size < 0 || total > r.options.MaxExpandedBytes || files > r.options.MaxFiles {
			return Package{}, errors.New("expanded package exceeds bounds")
		}
		if err = os.MkdirAll(filepath.Dir(destination), 0700); err != nil {
			return Package{}, err
		}
		f, err := os.OpenFile(destination, os.O_CREATE|os.O_EXCL|os.O_WRONLY, os.FileMode(header.Mode)&0777)
		if err != nil {
			return Package{}, err
		}
		n, copyErr := io.CopyN(f, tr, header.Size)
		closeErr := f.Close()
		if copyErr != nil {
			return Package{}, copyErr
		}
		if closeErr != nil {
			return Package{}, closeErr
		}
		if n != header.Size {
			return Package{}, errors.New("truncated archive entry")
		}
	}
	p, err := r.fromPath(name, stage, source, requested, proof)
	if err != nil {
		return Package{}, err
	}
	destination := filepath.Join(r.cache, "content", p.ContentHash)
	if err = os.MkdirAll(filepath.Dir(destination), 0700); err != nil {
		return Package{}, err
	}
	if err = os.Rename(stage, destination); err != nil {
		if !os.IsExist(err) {
			if info, e := os.Stat(destination); e != nil || !info.IsDir() {
				return Package{}, err
			}
		}
		existing, e := r.fromPath(name, destination, source, requested, proof)
		if e != nil || existing.ContentHash != p.ContentHash {
			return Package{}, errors.New("content-addressed package cache conflict")
		}
	}
	p.Path = destination
	return p, nil
}
